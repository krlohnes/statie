//! Integration tests for unit 22 part (a): `execute_try_fixes` (`core/src/
//! session.rs`) reads the harness-owned, persisted `HarnessState::
//! last_failure_summary` (`core/src/version.rs`) for both the "no failing
//! test on record" check and the `FixInput.failure` text, instead of the
//! session-local `Session::last_test_failure`. `last_test_failure` IS reset
//! on every `activate`/`activate_forced`, including a re-activation of the
//! SAME skill (it only backs the repeated-failure counter), but
//! `HarnessState::last_failure_summary` is a fact about the sandbox and
//! deliberately does NOT reset on activation -- it only clears on a passing
//! `test`. So `try_fixes` must keep working right after a re-activation with
//! no intervening `test` call, reading whatever failure was on record before
//! the re-activation.
//!
//! Fixture style copied from `core/tests/try_fixes.rs` (core cannot depend
//! on `statie-skills`/`statie-store`/`statie-models` as a normal
//! dependency; this test builds its own minimal `StateStore`, `Model`, and
//! `Skill` stubs), including using the REAL `statie_tools::TestTool` against
//! a real temporary git sandbox so the failing `test` turn is genuine, not
//! scripted, and `statie_core::MockCandidateRunner`/`MockFixer` for the
//! fixer/candidate-verification side.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, CandidateEdit, CandidateRunResult, Commitment, CommittedState, Event,
    FixOutput, Fixer, HarnessState, LogKind, LoggedPatch, MockCandidateRunner, MockFixer, Model,
    NullSink, Outcome, PatchError, PatchStrategy, RawOutput, SandboxRoot, Schema, Session,
    SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError,
    ToolSet, Version,
};
use statie_tools::TestTool;

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/try_fixes.rs`).
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
// `core/tests/try_fixes.rs`).
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
// from `core/tests/try_fixes.rs`).
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
            "a skill for try_fixes_source tests",
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

fn test_turn(cmd: &str) -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
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

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return;
        }
    };
}

async fn harness_state<K: Skill>(active: &mut statie_core::Active<'_, K>) -> HarnessState
where
    K::Workspace: Clone + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let (state, _version) = active.session().state().await.unwrap();
    state.harness
}

// =======================================================================
// (a) a failing `test` under the FIRST activation, then a re-activation of
// the SAME skill with NO second `test` in between, then `try_fixes {}`
// immediately: the fixer IS invoked, its recorded input's `failure` equals
// `harness.last_failure_summary` exactly as captured right before the
// re-activation -- proving the harness-owned mirror survives
// `activate_inner`'s re-activation branch even though the session-local
// `Session::last_test_failure` was reset by it. This is the case the bench
// run exercised 145 times and `try_fixes` wrongly refused every time before
// this correction.
// =======================================================================

#[tokio::test]
async fn try_fixes_after_reactivation_reads_the_harness_failure_summary() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    std::fs::write(root.join("answer.txt"), "WRONG\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        // Step 0: a failing `test` under the FIRST activation.
        test_turn("sh -c 'grep -q RIGHT answer.txt'"),
        // Step (after re-activation) 1: `try_fixes`, with NO second `test`
        // in between.
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![CandidateEdit {
            path: "answer.txt".to_string(),
            old: "WRONG\n".to_string(),
            new: "RIGHT\n".to_string(),
            rationale: "guess".to_string(),
        }],
        usage: None,
    })]));

    let mut files = HashMap::new();
    files.insert("answer.txt".to_string(), "WRONG\n".to_string());
    let runner = Arc::new(MockCandidateRunner::new(
        files,
        vec![Ok(CandidateRunResult {
            exit: Some(0),
            stdout: "ok".to_string(),
            stderr: String::new(),
            timed_out: false,
        })],
    ));

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

    // First activation: run the failing `test`, and capture
    // `harness.last_failure_summary` right after it, BEFORE re-activating.
    let expected_failure;
    {
        let mut active = session.activate(Arc::clone(&skill)).await.unwrap();
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
        let h = harness_state(&mut active).await;
        expected_failure = h.last_failure_summary.clone().expect(
            "the first activation's failing `test` should populate \
             harness.last_failure_summary",
        );
    }

    // Force a re-activation of the SAME skill (self-transition, always
    // allowed by `check_transition`). This resets `Session::
    // last_test_failure`, but `harness.last_failure_summary` is a fact
    // about the sandbox, not the activation, and must survive untouched.
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();
    let h_after_reactivation = harness_state(&mut active).await;
    assert_eq!(
        h_after_reactivation.last_failure_summary,
        Some(expected_failure.clone()),
        "harness.last_failure_summary must survive re-activation unchanged -- it is a \
         fact about the sandbox (the most recent test's result), not about this \
         activation, unlike the session-local Session::last_test_failure"
    );

    // `try_fixes {}`, immediately, with NO second `test` call.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step1 = rec1.entries().iter().map(|e| e.step).max().unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // No `tool_error` for this step: the fixer was invoked.
    let system_entries = log.entries(Some(LogKind::System), 100);
    let tool_error_at_step1 = system_entries.iter().any(|e| {
        e.step == step1 && e.payload.get("event").and_then(Value::as_str) == Some("tool_error")
    });
    assert!(
        !tool_error_at_step1,
        "try_fixes must not report a tool_error right after a re-activation with no \
         intervening test, as long as the harness has a failure on record"
    );

    // The fixer WAS invoked exactly once, with `failure` equal to the
    // harness `last_failure_summary` captured before the re-activation.
    let inputs = fixer.inputs();
    assert_eq!(inputs.len(), 1, "expected exactly one fixer call");
    assert_eq!(
        inputs[0].failure, expected_failure,
        "the fixer input's failure text must equal the harness-owned \
         last_failure_summary as it stood before the re-activation, not the (reset) \
         session-local last_test_failure"
    );

    // The sandbox's `answer.txt` is now RIGHT (the winning candidate was
    // applied), confirming the whole `try_fixes` flow actually ran.
    let contents = std::fs::read_to_string(root.join("answer.txt")).unwrap();
    assert_eq!(contents, "RIGHT\n");
}

// =======================================================================
// (b) no test ever run: `try_fixes {}` still yields the "no failing test to
// fix" tool error, and the fixer is never called.
// =======================================================================

#[tokio::test]
async fn try_fixes_with_no_test_ever_run_reports_no_failing_test_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![try_fixes_turn(), yield_turn()]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![],
        usage: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let system_entries = log.entries(Some(LogKind::System), 100);
    let tool_error = system_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .expect("expected a tool_error system entry");
    assert_eq!(
        tool_error.payload.get("name").and_then(Value::as_str),
        Some("try_fixes")
    );
    assert_eq!(
        tool_error.payload.get("reason").and_then(Value::as_str),
        Some("no failing test to fix; run test first")
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    assert_eq!(
        fixer.inputs().len(),
        0,
        "the fixer must never be called when there is no failing test on record"
    );
}

// =======================================================================
// (c) a failing `test`, then a PASSING `test`, then `try_fixes {}`: the
// passing `test` clears `harness.last_failure_summary` back to `None` (the
// one case where the mirror DOES clear -- see that field's doc), so
// `try_fixes` gives the "no failing test to fix" tool error, and the fixer
// is never called.
// =======================================================================

#[tokio::test]
async fn try_fixes_after_a_passing_test_reports_no_failing_test_error() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    std::fs::write(root.join("answer.txt"), "WRONG\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let model = ScriptedModel::new(vec![
        // Step 0: a failing `test`.
        test_turn("sh -c 'grep -q RIGHT answer.txt'"),
        // Step 1: fix the file out from under the session, then a PASSING
        // `test` -- clears harness.last_failure_summary back to None.
        test_turn("sh -c 'grep -q WRONG answer.txt'"),
        // Step 2: `try_fixes`.
        try_fixes_turn(),
        yield_turn(),
    ]);

    let fixer = Arc::new(MockFixer::new(vec![Ok(FixOutput {
        candidates: vec![],
        usage: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        test_tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_fixer(Arc::clone(&fixer) as Arc<dyn Fixer>, 3);

    let skill = Arc::new(TryFixesSkill);
    let mut active = session.activate(skill).await.unwrap();

    // Step 0: failing `test`.
    let rec0 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome0 = log.append_record(rec0).unwrap();
    assert!(matches!(outcome0, Outcome::Continue), "got {outcome0:?}");
    let h0 = harness_state(&mut active).await;
    assert!(
        h0.last_failure_summary.is_some(),
        "the failing `test` should populate harness.last_failure_summary"
    );

    // Step 1: passing `test` (the command now checks for "WRONG", which is
    // still in the file).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let h1 = harness_state(&mut active).await;
    assert_eq!(
        h1.last_failure_summary, None,
        "a passing `test` must clear harness.last_failure_summary back to None"
    );

    // Step 2: `try_fixes {}` -> the tool error, fixer never called.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step2 = rec2.entries().iter().map(|e| e.step).max().unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let system_entries = log.entries(Some(LogKind::System), 100);
    let tool_error = system_entries
        .iter()
        .find(|e| {
            e.step == step2 && e.payload.get("event").and_then(Value::as_str) == Some("tool_error")
        })
        .expect("expected a tool_error system entry at the try_fixes step");
    assert_eq!(
        tool_error.payload.get("name").and_then(Value::as_str),
        Some("try_fixes")
    );
    assert_eq!(
        tool_error.payload.get("reason").and_then(Value::as_str),
        Some("no failing test to fix; run test first")
    );

    assert_eq!(
        fixer.inputs().len(),
        0,
        "the fixer must never be called once a passing test has cleared the failure \
         on record"
    );
}
