//! Integration tests for the `delegate` executor (milestone 9,
//! ARCHITECTURE.md "The `delegate` action"). Core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models`, so this test builds its
//! own minimal `StateStore`, `Model`, `Skill`, and `ChildRunner` stubs, in
//! the same spirit as `core/tests/parse_validate.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, ChildRequest, ChildRunner, CommittedState, Event,
    FileNote, HarnessState, LogKind, LoggedPatch, MemoryLog, Model, ModelError, NullSink, Outcome,
    Park, PatchError, PatchStrategy, RawOutput, Report, ReportOutcome, ReportStats, SandboxRoot,
    Schema, Session, SessionConfig, SessionError, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>`, local to this test file (core
// cannot depend on `statie-store`).
// ---------------------------------------------------------------------

struct Inner<W> {
    initial: CommittedState<W>,
    current: CommittedState<W>,
    version: Version,
    log: Vec<LoggedPatch>,
}

struct TestStore<W> {
    inner: Mutex<Option<Inner<W>>>,
}

impl<W> TestStore<W> {
    fn new() -> Self {
        TestStore {
            inner: Mutex::new(None),
        }
    }
}

#[async_trait]
impl<W> StateStore<W> for TestStore<W>
where
    W: Clone + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    async fn load(&self) -> Result<Option<(CommittedState<W>, Version)>, StoreError> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .as_ref()
            .map(|inner| (inner.current.clone(), inner.version)))
    }

    async fn init(&self, initial: CommittedState<W>) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().unwrap();
        if guard.is_some() {
            return Err(StoreError::Backend("already initialized".to_string()));
        }
        let version = Version(0);
        *guard = Some(Inner {
            initial: initial.clone(),
            current: initial,
            version,
            log: Vec::new(),
        });
        Ok(version)
    }

    async fn commit(
        &self,
        patch: statie_core::ValidatedPatch<W>,
        harness: HarnessState,
        action: &Action,
        expect: Version,
    ) -> Result<Version, StoreError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        if patch.version() != expect || expect != inner.version {
            return Err(StoreError::VersionMismatch {
                expected: patch.version(),
                actual: inner.version,
            });
        }
        let (raw, next, _v) = patch.into_parts();
        let new_version = Version(inner.version.0 + 1);
        inner.current.agent = next;
        inner.current.harness = harness.clone();
        inner.version = new_version;
        inner.log.push(LoggedPatch {
            version: new_version,
            patch: raw,
            action: action.clone(),
            harness,
        });
        Ok(new_version)
    }

    async fn patch_log(&self) -> Result<Vec<LoggedPatch>, StoreError> {
        let guard = self.inner.lock().unwrap();
        Ok(guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?
            .log
            .clone())
    }

    async fn replay(
        &self,
        to: Version,
        apply: ApplyFn<'_, W>,
    ) -> Result<CommittedState<W>, StoreError> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        let mut agent = inner.initial.agent.clone();
        let mut harness = inner.initial.harness.clone();
        for entry in &inner.log {
            if entry.version > to {
                break;
            }
            agent = apply(&agent, &entry.patch).map_err(|e| StoreError::Backend(e.to_string()))?;
            harness = entry.harness.clone();
        }
        Ok(CommittedState { harness, agent })
    }

    async fn set_harness(&self, harness: HarnessState) -> Result<(), StoreError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| StoreError::Backend("not initialized".to_string()))?;
        inner.current.harness = harness;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// A minimal scripted `Model`: always returns a fixed "yield with empty
// patch" turn. The tests in this file drive delegation directly through
// `Active::step`/`execute` rather than needing the model to emit
// `Action::Delegate` itself, so the model is never actually exercised for
// the delegate step (delegation is triggered by constructing the
// `Action::Delegate` step manually via a tiny parent skill whose `rules`
// hook lets the test call `execute` -- see `run_delegate_step` below,
// which drives it through the *public* `Active::step` API by scripting the
// model to emit a `delegate` action).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<String>) -> Self {
        ScriptedModel {
            turns: Mutex::new(turns),
        }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace, patch strategy, and skill: `reports: BTreeMap<String,
// Report>`, `allowed_delegates = ["stub"]`, `absorb_report` inserts.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct ParentWorkspace {
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    reports: BTreeMap<String, Report>,
    #[serde(default)]
    done: bool,
}

struct ParentPatch;

impl PatchStrategy<ParentWorkspace> for ParentPatch {
    type Patch = ParentWorkspace;

    fn name() -> &'static str {
        "replace"
    }

    fn describe() -> &'static str {
        "The patch is the entire new workspace value."
    }

    fn apply(
        _cur: &ParentWorkspace,
        patch: &ParentWorkspace,
    ) -> Result<ParentWorkspace, PatchError> {
        Ok(patch.clone())
    }
}

struct ParentSkill {
    reject_absorb: bool,
}

impl ParentSkill {
    fn new() -> Arc<ParentSkill> {
        Arc::new(ParentSkill {
            reject_absorb: false,
        })
    }

    fn rejecting_absorb() -> Arc<ParentSkill> {
        Arc::new(ParentSkill {
            reject_absorb: true,
        })
    }
}

impl Skill for ParentSkill {
    type Workspace = ParentWorkspace;
    type Patch = ParentPatch;

    fn name(&self) -> &str {
        "parent"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "parent",
            "a parent skill for delegate tests",
        ))
    }

    fn initial_workspace(&self) -> Self::Workspace {
        ParentWorkspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![]
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        s.done
    }

    fn allowed_delegates(&self) -> Vec<String> {
        vec!["stub".to_string()]
    }

    fn absorb_report(&self, ws: &mut Self::Workspace, report: Report) -> Result<(), PatchError> {
        if self.reject_absorb {
            return Err(PatchError::RuleViolation(
                "this test skill refuses every report".to_string(),
            ));
        }
        ws.reports.insert(report.agent.clone(), report);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// A stub `ChildRunner` returning a canned `Report`.
// ---------------------------------------------------------------------

struct StubChildRunner {
    children: Vec<String>,
    report: Report,
}

impl StubChildRunner {
    fn new(report: Report) -> Arc<StubChildRunner> {
        Arc::new(StubChildRunner {
            children: vec!["stub".to_string()],
            report,
        })
    }
}

#[async_trait]
impl ChildRunner for StubChildRunner {
    fn children(&self) -> Vec<String> {
        self.children.clone()
    }

    async fn run(
        &self,
        req: ChildRequest,
        sink: &mut dyn StepSink,
    ) -> Result<Report, SessionError> {
        sink.child_started(&req.agent_path, &req.task);
        sink.child_finished(&req.agent_path, &self.report);
        Ok(self.report.clone())
    }
}

fn canned_report() -> Report {
    Report {
        agent: "stub".to_string(),
        task: "check something".to_string(),
        outcome: ReportOutcome::Done,
        summary: "all good".to_string(),
        findings: vec!["minor nit".to_string()],
        files: vec![FileNote {
            path: "src/lib.rs".to_string(),
            note: "relevant".to_string(),
        }],
        data: serde_json::json!({"ok": true})
            .as_object()
            .cloned()
            .unwrap(),
        stats: ReportStats {
            steps: 3,
            server_prompt_tokens: 100,
            server_completion_tokens: 50,
            wall_ms: 1000,
        },
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 1000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn delegate_turn(child: &str, task: &str) -> String {
    serde_json::json!({
        "patch": {"counter": 1},
        "action": {"type": "delegate", "skill": child, "task": task},
    })
    .to_string()
}

// ---------------------------------------------------------------------
// 1. Happy path: delegate absorbs the canned report.
// ---------------------------------------------------------------------

#[tokio::test]
async fn delegate_absorbs_report_and_commits_two_steps() {
    let root = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new(vec![delegate_turn("stub", "check something")]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let runner = StubChildRunner::new(canned_report());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_child_runner(runner);

    let skill = ParentSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (_, version_before) = active.session().state().await.unwrap();
    assert_eq!(version_before, Version(0));

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entries = rec.entries().to_vec();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "{outcome:?}");

    // harness.step advanced by 2 over the delegate step's start (the
    // parent's own patch commit is step 1, the absorb commit is step 2).
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(2));
    assert_eq!(state.harness.step, 2);
    assert_eq!(state.agent.reports.get("stub").unwrap().summary, "all good");
    assert_eq!(state.agent.reports.get("stub").unwrap().stats.steps, 3);

    // The patch log has one extra entry (beyond the parent's own patch)
    // whose payload has source: "subagent".
    let patch_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == LogKind::Patch)
        .collect();
    assert_eq!(patch_entries.len(), 2, "{patch_entries:?}");
    let subagent_patch = &patch_entries[1];
    assert_eq!(
        subagent_patch.payload.get("source").and_then(Value::as_str),
        Some("subagent")
    );
}

// ---------------------------------------------------------------------
// 1b. The Observation text itself, inspected via a prompt-recording model
// (test 1 above only asserts on state/log; the exact Observation text is
// asserted here so a `RecordingModel` failure doesn't also obscure test 1's
// state/log assertions).
// ---------------------------------------------------------------------

struct RecordingModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl RecordingModel {
    fn new(turns: Vec<String>) -> Arc<RecordingModel> {
        Arc::new(RecordingModel {
            turns: Mutex::new(turns),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for RecordingModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        self.prompts.lock().unwrap().push(prompt.user().to_string());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

#[tokio::test]
async fn next_prompt_observation_mentions_subagent_and_reports() {
    let root = tempfile::tempdir().unwrap();
    let model = RecordingModel::new(vec![
        delegate_turn("stub", "check something"),
        serde_json::json!({"patch": {}, "action": {"type": "yield"}}).to_string(),
    ]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let runner = StubChildRunner::new(canned_report());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_child_runner(runner);

    let skill = ParentSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let obs_idx = prompts[1]
        .find("## Observation")
        .expect("second prompt should carry an Observation section");
    let heading = "## Observation\n";
    let observation_body = prompts[1][obs_idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading");
    assert!(
        observation_body.starts_with("subagent stub finished (done, "),
        "got: {observation_body}"
    );
    assert!(
        observation_body.contains("reports[\"stub\"]"),
        "got: {observation_body}"
    );
}

// ---------------------------------------------------------------------
// 2. Delegate to an unknown child yields the rejection observation.
// ---------------------------------------------------------------------

#[tokio::test]
async fn delegate_to_unknown_child_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let model = RecordingModel::new(vec![
        delegate_turn("does-not-exist", "check something"),
        serde_json::json!({"patch": {}, "action": {"type": "yield"}}).to_string(),
    ]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let runner = StubChildRunner::new(canned_report());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_child_runner(runner);

    // `does-not-exist` is not in `ParentSkill::allowed_delegates()`
    // (["stub"]), so `validate_default` already rejects it before
    // `execute` is reached -- exercising the "belt and braces" path in
    // `execute` itself requires a skill whose `allowed_delegates()` is
    // wider than the runner's `children()`. Use `rejecting_absorb`'s
    // sibling constructed inline with a wider allow-list for that.
    struct WideParentSkill;
    impl Skill for WideParentSkill {
        type Workspace = ParentWorkspace;
        type Patch = ParentPatch;
        fn name(&self) -> &str {
            "parent"
        }
        fn spec(&self) -> Arc<SkillSpec> {
            Arc::new(SkillSpec::new("parent", "wide-allow parent"))
        }
        fn initial_workspace(&self) -> Self::Workspace {
            ParentWorkspace::default()
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn is_terminal(&self, s: &Self::Workspace) -> bool {
            s.done
        }
        fn allowed_delegates(&self) -> Vec<String> {
            vec!["does-not-exist".to_string()]
        }
        fn absorb_report(
            &self,
            ws: &mut Self::Workspace,
            report: Report,
        ) -> Result<(), PatchError> {
            ws.reports.insert(report.agent.clone(), report);
            Ok(())
        }
    }

    let skill = Arc::new(WideParentSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entries = rec1.entries().to_vec();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    // The patch still committed (delegation validation passed at the
    // `validate_default` layer since `does-not-exist` is in
    // `allowed_delegates()`); the rejection comes from `execute` not
    // knowing the runner's children.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert!(state.agent.reports.is_empty());

    let rejected = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("delegate_rejected"))
        .expect("expected a delegate_rejected system entry");
    assert_eq!(
        rejected.payload.get("skill").and_then(Value::as_str),
        Some("does-not-exist")
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].contains("is not available"),
        "got: {}",
        prompts[1]
    );
}

// ---------------------------------------------------------------------
// 3. `absorb_report` returning `Err` yields a rejection observation and
// leaves state unchanged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn absorb_report_error_is_rejected_and_state_unchanged_beyond_parent_patch() {
    let root = tempfile::tempdir().unwrap();
    let model = RecordingModel::new(vec![
        delegate_turn("stub", "check something"),
        serde_json::json!({"patch": {}, "action": {"type": "yield"}}).to_string(),
    ]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let runner = StubChildRunner::new(canned_report());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_child_runner(runner);

    let skill = ParentSkill::rejecting_absorb();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entries = rec1.entries().to_vec();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    // The parent's own patch (step 1, counter=1) committed; the absorb
    // commit (would-be step 2) did not, since absorb_report errored.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(state.harness.step, 1);
    assert_eq!(state.agent.counter, 1);
    assert!(state.agent.reports.is_empty());

    let rejected = entries
        .iter()
        .find(|e| {
            e.payload.get("event").and_then(Value::as_str) == Some("delegate_absorb_rejected")
        })
        .expect("expected a delegate_absorb_rejected system entry");
    assert!(
        rejected
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("refuses"),
        "{rejected:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert!(
        prompts[1].contains("could not be absorbed"),
        "got: {}",
        prompts[1]
    );
}

// ---------------------------------------------------------------------
// 4. Replaying the store reproduces the state with the report.
// ---------------------------------------------------------------------

#[tokio::test]
async fn replay_reproduces_state_with_absorbed_report() {
    let root = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new(vec![delegate_turn("stub", "check something")]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let runner = StubChildRunner::new(canned_report());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_child_runner(runner);

    let skill = ParentSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let (live, live_version) = active.session().state().await.unwrap();
    assert_eq!(live_version, Version(2));

    let replayed = active.replay(live_version).await.unwrap();
    assert_eq!(replayed.agent.counter, live.agent.counter);
    assert_eq!(
        replayed
            .agent
            .reports
            .get("stub")
            .map(|r| r.summary.clone()),
        live.agent.reports.get("stub").map(|r| r.summary.clone())
    );
    assert_eq!(replayed.harness.step, live.harness.step);
}

// ---------------------------------------------------------------------
// 5. No runner configured at all: delegation is rejected naming an empty
// set of configured children.
// ---------------------------------------------------------------------

#[tokio::test]
async fn delegate_with_no_runner_configured_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let model = RecordingModel::new(vec![
        delegate_turn("stub", "check something"),
        serde_json::json!({"patch": {}, "action": {"type": "yield"}}).to_string(),
    ]);
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());

    let mut session = Session::new(
        Box::new(TestStore::<ParentWorkspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    // Deliberately no `.with_child_runner(...)`.

    let skill = ParentSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "{outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert!(
        prompts[1].contains("configured children are []"),
        "got: {}",
        prompts[1]
    );
}

// Silence an unused-import warning for `Park` (kept for readability of
// this file's outcome matches, matching the style used elsewhere).
#[allow(dead_code)]
fn _use_park(p: Park) -> Park {
    p
}
