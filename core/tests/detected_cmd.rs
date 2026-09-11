//! Integration test for unit 23 part (a): a `test {}` call with no
//! `cmd`/`command` in its args (auto-detected by `tools/src/test_tool.rs`'s
//! `detect_test_command`) must still populate `harness.last_test_cmd`, by
//! parsing the command back out of the tool's own `command: <cmd>` first
//! line (`statie_core::detected_test_command`, `core/src/session.rs`'s
//! `Active::execute`) -- before this fix, an auto-detected `test` call left
//! `last_test_cmd` at `None`, so `execute_try_fixes`'s precondition (`Some(
//! cmd)` alongside `Some(failure)`, `core/src/session.rs`) was never met even
//! with a genuine failing test on record.
//!
//! Fixture style copied from `core/tests/try_fixes_source.rs` (core cannot
//! depend on `statie-skills`/`statie-store`/`statie-models` as a normal
//! dependency; this test builds its own minimal `StateStore`, `Model`, and
//! `Skill` stubs), including using the REAL `statie_tools::TestTool` against
//! a real temporary sandbox and `statie_core::MockCandidateRunner`/
//! `MockFixer` for the fixer/candidate-verification side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Commitment, CommittedState, Event, FixOutput, Fixer, HarnessState,
    LogKind, LoggedPatch, MockCandidateRunner, MockFixer, Model, NullSink, Outcome, PatchError,
    PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog,
    SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError, ToolSet, Version,
};
use statie_tools::TestTool;

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/
// try_fixes_source.rs`).
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
// A scripted `Model` driving a fixed sequence of turns (copied from
// `core/tests/try_fixes_source.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<String>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns),
            prompts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn capabilities(&self) -> statie_core::Capabilities {
        statie_core::Capabilities {
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
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.prompts.lock().unwrap().push(prompt.user().to_string());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(statie_core::ModelError::Backend(
                "script exhausted".to_string(),
            ));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace and skill: `try_fixes` active alongside `test` (copied
// from `core/tests/try_fixes_source.rs`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    next: Option<Commitment>,
    #[serde(default)]
    done: bool,
}

struct MergePatch;

impl PatchStrategy<Workspace> for MergePatch {
    type Patch = Value;

    fn name() -> &'static str {
        "merge"
    }

    fn describe() -> &'static str {
        "A JSON merge patch over the top-level workspace fields."
    }

    fn apply(cur: &Workspace, patch: &Value) -> Result<Workspace, PatchError> {
        let mut cur_value =
            serde_json::to_value(cur).map_err(|e| PatchError::Malformed(e.to_string()))?;
        if let (Value::Object(cur_map), Value::Object(patch_map)) = (&mut cur_value, patch) {
            for (k, v) in patch_map {
                cur_map.insert(k.clone(), v.clone());
            }
        } else {
            return Err(PatchError::Malformed("patch must be an object".to_string()));
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

struct TryFixesSkill;

impl Skill for TryFixesSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "try_fixes_skill"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "try_fixes_skill",
            "a skill for detected_cmd tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["test".to_string(), "try_fixes".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn session_config(root: &std::path::Path) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 40_000,
            observation: 8000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root).unwrap(),
    }
}

/// A `test {}` call with NO `cmd`/`command` argument -- the auto-detect path.
fn test_turn_no_cmd() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "tool", "name": "test", "args": {}},
        "ack_inbox": []
    })
    .to_string()
}

fn try_fixes_turn() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "tool", "name": "try_fixes", "args": {}},
        "ack_inbox": []
    })
    .to_string()
}

fn yield_turn() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "yield"},
        "ack_inbox": []
    })
    .to_string()
}

fn test_tool_set() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(TestTool {
        default_timeout_secs: 30,
        max_output_bytes: 200_000,
    }));
    tools
}

async fn harness_state<K: Skill>(active: &mut statie_core::Active<'_, K>) -> HarnessState
where
    K::Workspace: Clone + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let (state, _version) = active.session().state().await.unwrap();
    state.harness
}

// =======================================================================
// A `test {}` call with no `cmd` arg auto-detects `make test` (a `Makefile`
// with a `test:` target that fails is present in the sandbox), leaving
// `harness.last_test_cmd == Some("make test")` -- proving `detected_test_
// command` recovered it from the tool's own `command: make test` first line
// rather than leaving it `None`. Then `try_fixes {}` with a `MockFixer`
// actually invokes the fixer (no "no failing test" tool error), proving
// `execute_try_fixes`'s precondition is now met with both `last_test_cmd`
// and `last_failure_summary` populated.
// =======================================================================

#[tokio::test]
async fn auto_detected_test_command_populates_last_test_cmd_and_unblocks_try_fixes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Makefile"), "test:\n\texit 1\n").unwrap();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        // Step 0: `test {}` with no `cmd` -- auto-detects `make test`,
        // which fails (the Makefile's `test:` target always exits 1).
        test_turn_no_cmd(),
        // Step 1: `try_fixes {}`.
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![],
        usage: None,
    })]));

    let runner = Arc::new(MockCandidateRunner::new(HashMap::new(), vec![]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        test_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3)
    .with_candidate_runner(Arc::clone(&runner) as Arc<dyn statie_core::CandidateRunner>);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 0: the auto-detected, failing `test` call.
    let rec0 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome0 = log.append_record(rec0).unwrap();
    assert!(matches!(outcome0, Outcome::Continue), "got {outcome0:?}");

    let h0 = harness_state(&mut active).await;
    assert_eq!(
        h0.last_test_cmd,
        Some("make test".to_string()),
        "an auto-detected test call must populate harness.last_test_cmd by parsing the \
         tool's own `command: <cmd>` first line, not leave it None"
    );
    assert!(
        h0.last_failure_summary.is_some(),
        "the failing auto-detected test call should populate harness.last_failure_summary"
    );

    // Step 1: `try_fixes {}` -- must actually invoke the fixer now that both
    // `last_test_cmd` and `last_failure_summary` are populated, i.e. no "no
    // failing test" tool_error.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step1 = rec1.entries().iter().map(|e| e.step).max().unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let system_entries = log.entries(Some(LogKind::System), 100);
    let tool_error_at_step1 = system_entries.iter().find(|e| {
        e.step == step1 && e.payload.get("event").and_then(Value::as_str) == Some("tool_error")
    });
    assert!(
        tool_error_at_step1.is_none(),
        "try_fixes must not report a tool_error once last_test_cmd/last_failure_summary are \
         both populated from an auto-detected failing test, got: {tool_error_at_step1:?}"
    );

    let inputs = fixer.inputs();
    assert_eq!(
        inputs.len(),
        1,
        "expected exactly one fixer call now that try_fixes's precondition is met"
    );
}
