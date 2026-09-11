//! Integration tests for the no-green cap (unit 27, `core/src/session.rs`):
//! `HarnessState::fails_since_green` (`core/src/version.rs`) counts `test`
//! tool runs whose result was a failure since the last passing `test` run in
//! this activation, incremented at the same call site as `green_step`/
//! `repeated_failure_step`. At the failing `test` call that brings the
//! count to `NO_GREEN_PARK_AT` (16), the session parks through
//! `park_non_progress_cap` with the shape `"no green in 16 test runs"`, the
//! same path the stalled-path and repeated-tool-error guards use.
//!
//! Fixture style copied from `core/tests/it/repeated_failure.rs` (core
//! cannot depend on `statie-skills`/`statie-store`/`statie-models` as a
//! normal dependency; this test uses a minimal in-memory `StateStore`, a
//! scripted `Model`, and a scripted `test` tool so the pass/fail sequence is
//! fully controlled).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, Model, ModelError, NullSink, Outcome, Park, PatchError, PatchStrategy,
    RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill,
    SkillSpec, StateStore, StepSink, StoreError, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet,
    Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from
// `core/tests/it/repeated_failure.rs`).
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
// A scripted `Model` that replays a fixed sequence of raw step outputs
// (copied from `core/tests/it/repeated_failure.rs`).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<Value>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns.into_iter().map(|v| v.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        })
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
        self.prompts.lock().unwrap().push(prompt.user().to_string());
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace: same shape as `core/tests/it/repeated_failure.rs`'s.
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
                if v.is_null() {
                    cur_map.remove(k);
                } else {
                    cur_map.insert(k.clone(), v.clone());
                }
            }
        } else {
            return Err(PatchError::Malformed("patch must be an object".to_string()));
        }
        serde_json::from_value(cur_value).map_err(|e| PatchError::Malformed(e.to_string()))
    }
}

/// Active tools `["edit_file", "test"]`; terminal on `done`; no commitment
/// slot, no silent-read cap -- neither is relevant to this guard. Copied
/// from `core/tests/it/repeated_failure.rs`'s `RepeatSkill`.
struct NoGreenSkill;

impl Skill for NoGreenSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "no-green"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "no-green",
            "a skill for the no-green cap tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["edit_file".to_string(), "test".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 4000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

// ---------------------------------------------------------------------
// Fake `test` tool: pops the next scripted output off a queue and returns
// it verbatim. Copied from `core/tests/it/repeated_failure.rs`.
// ---------------------------------------------------------------------

struct ScriptedTest {
    outputs: Mutex<Vec<String>>,
}

#[async_trait]
impl Tool for ScriptedTest {
    fn name(&self) -> &str {
        "test"
    }
    fn description(&self) -> &str {
        "runs a scripted test result"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            return Err(ToolError::ExecutionFailed("script exhausted".to_string()));
        }
        Ok(ToolOutputFull::new(outputs.remove(0)))
    }
}

/// A no-op `edit_file` that always "succeeds".
struct NoopEditFile;

#[async_trait]
impl Tool for NoopEditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "edits a file"
    }
    fn args_schema(&self) -> Schema {
        Schema::of::<Value>()
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        Ok(ToolOutputFull::new("edited".to_string()))
    }
}

fn tool_set(test_outputs: Vec<String>) -> ToolSet {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(ScriptedTest {
        outputs: Mutex::new(test_outputs),
    }));
    tools.register(Arc::new(NoopEditFile));
    tools
}

fn test_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "test", "args": {"cmd": cmd}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn non_progress_cap_shapes(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("non_progress_cap")
        })
        .and_then(|e| e.payload.get("shapes").and_then(Value::as_str))
        .map(str::to_string)
}

/// A fail output with the `--- failure summary ---` marker, mirroring
/// `tools/src/test_tool.rs::format_result`'s shape on a non-zero exit.
/// Copied from `core/tests/it/repeated_failure.rs`. Each call gets its own
/// summary text (`n`) so the repeated-failure guard's own identical-summary
/// streak never itself interferes with this test's own assertions.
fn fail_output(n: usize) -> String {
    format!(
        "command: cargo test\nresult: fail (exit 1)\n--- failure summary ---\nfailure #{n}\n\
         --- stdout ---\nsome noise\n--- stderr ---\nmore noise\n"
    )
}

fn pass_output() -> String {
    "command: cargo test\nresult: pass\nexit: 0\n--- stdout ---\nok\n--- stderr ---\n".to_string()
}

/// Sixteen consecutive failing `test` runs park at the sixteenth:
/// `fails_since_green` climbs 1..=16 across the fifteen `Continue` steps and
/// the final parking step, and the sixteenth step logs a `non_progress_cap`
/// System entry with the shape `"no green in 16 test runs"`, parking as
/// `Park::AskUser`.
#[tokio::test]
async fn sixteen_failing_test_runs_park_at_the_sixteenth() {
    let root = tempfile::tempdir().unwrap();

    let turns: Vec<Value> = (1..=16)
        .map(|n| {
            step_output(
                serde_json::json!({}),
                test_action(&format!("cargo test {n}")),
            )
        })
        .collect();
    let model = ScriptedModel::new(turns);

    let outputs: Vec<String> = (1..=16).map(fail_output).collect();

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(outputs),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(NoGreenSkill)).await.unwrap();

    let mut outcomes = Vec::new();
    let mut fails_since_green = Vec::new();
    let mut cap_events = Vec::new();
    let mut cap_shapes = Vec::new();
    for _ in 0..16 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        cap_events.push(has_event(&rec, "non_progress_cap"));
        cap_shapes.push(non_progress_cap_shapes(&rec));
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push(outcome);

        let (state, _v) = active.session().state().await.unwrap();
        fails_since_green.push(state.harness.fails_since_green);
    }

    // `fails_since_green` climbs 1, 2, ..., 16 across the sixteen failing
    // runs -- reset to 0 never happens since every one of these is a fail.
    let expected: Vec<u32> = (1..=16).collect();
    assert_eq!(
        fails_since_green, expected,
        "fails_since_green must climb by exactly one on every failing test run"
    );

    // Steps 0-14 (the first 15 fails): ordinary Continue outcomes, no
    // non_progress_cap entry.
    for (i, (outcome, is_cap)) in outcomes.iter().zip(cap_events.iter()).enumerate().take(15) {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; the first 15 failing test runs must not park"
        );
        assert!(
            !is_cap,
            "step {i}: must not log non_progress_cap before the 16th failing run"
        );
    }

    // Step 15 (0-indexed; the SIXTEENTH failing test run): parks, with a
    // non_progress_cap log entry naming the shape from the constant.
    assert!(
        cap_events[15],
        "the 16th failing test run must log a non_progress_cap System entry"
    );
    let shape = cap_shapes[15]
        .as_ref()
        .expect("the 16th failing test run's non_progress_cap entry must carry a shapes string");
    assert_eq!(
        shape, "no green in 16 test runs",
        "the shape must be formatted from NO_GREEN_PARK_AT exactly as the brief specifies"
    );
    match &outcomes[15] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("no green in 16 test runs"),
                "park message should contain the shape: {msg}"
            );
            assert!(
                msg.contains("Reply with what it should do next."),
                "park message should end with the standard reply prompt: {msg}"
            );
        }
        other => panic!("step 15: expected Parked(AskUser(_)), got {other:?}"),
    }
}

/// A pass in the middle of a run of failures resets `fails_since_green` back
/// to 0: four fails (climbing 1, 2, 3, 4), then a pass (resets to 0), then
/// one more fail (back up to 1) -- proving the reset happens on a pass, not
/// just at the cap.
#[tokio::test]
async fn a_pass_resets_fails_since_green_then_a_fail_climbs_again() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        step_output(serde_json::json!({}), test_action("cargo test 1")), // fail 1
        step_output(serde_json::json!({}), test_action("cargo test 2")), // fail 2
        step_output(serde_json::json!({}), test_action("cargo test 3")), // fail 3
        step_output(serde_json::json!({}), test_action("cargo test 4")), // fail 4
        step_output(serde_json::json!({}), test_action("cargo test 5")), // pass
        step_output(serde_json::json!({}), test_action("cargo test 6")), // fail again
    ];
    let model = ScriptedModel::new(turns);

    let outputs = vec![
        fail_output(1),
        fail_output(2),
        fail_output(3),
        fail_output(4),
        pass_output(),
        fail_output(6),
    ];

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(outputs),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(NoGreenSkill)).await.unwrap();

    let mut fails_since_green = Vec::new();
    for _ in 0..6 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        let (state, _v) = active.session().state().await.unwrap();
        fails_since_green.push(state.harness.fails_since_green);
    }

    // fail, fail, fail, fail, pass, fail -> 1, 2, 3, 4, 0, 1.
    assert_eq!(
        fails_since_green,
        vec![1, 2, 3, 4, 0, 1],
        "fails_since_green must climb on each fail, reset to 0 on the pass, then climb again"
    );
}
