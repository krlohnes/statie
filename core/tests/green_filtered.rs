//! Integration tests for `HarnessState.green_filtered` (statie-spec.md "10.
//! Working set (recent observations)", subsection "Green checkpoint"): a
//! passing `test` run whose output notes tests were filtered out
//! (`tools/src/test_tool.rs`'s `filtered_out_note`, the "note: {n} test(s)
//! were filtered out by this command; run it without the filter before
//! approving." line) sets `green_filtered: true` alongside
//! `green_step: Some(step)`; a later passing run without that note clears
//! it back to `false`; a failing run sets both `green_step: None` and
//! `green_filtered: false`. Live run 67: `implement` passed a filtered
//! target test, and `review` ran the SAME filtered command, saw it pass,
//! and approved -- the unfiltered suite failed on disk.
//!
//! Fixture style copied from `core/tests/green_checkpoint.rs` (core cannot
//! depend on `statie-skills`/`statie-store`/`statie-models` as a normal
//! dependency; this test uses `statie-tools` -- a dev-dependency added for
//! that unit -- for the real `test` tool, so the filtered-note detection is
//! exercised against real shell output, not a stub).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LoggedPatch, Model, ModelError, NullSink, PatchError, PatchStrategy, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore,
    StepSink, StoreError, ToolSet, Version,
};
use statie_tools::{ToolConfig, default_tools};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from
// `core/tests/green_checkpoint.rs`).
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
// (copied from `core/tests/green_checkpoint.rs`).
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
// Test workspace (copied from `core/tests/green_checkpoint.rs`).
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

/// Active tools `["edit_file", "write_file", "test"]`; terminal on
/// `done`; no commitment slot, no silent-read cap -- neither is relevant
/// here.
struct GreenSkill;

impl Skill for GreenSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "green"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("green", "a skill for green-filtered tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "edit_file".to_string(),
            "write_file".to_string(),
            "test".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }
}

fn session_config(root: &tempfile::TempDir, retries: u32) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 4000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn real_tools() -> ToolSet {
    default_tools(&ToolConfig {
        shell_timeout_secs: 30,
        allow_shell: true,
        max_output_bytes: 200_000,
    })
}

fn test_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "test", "args": {"cmd": cmd}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

/// A shell command that exits 0 and prints a cargo-style summary line with
/// a `filtered out` count, matching what `tools/src/test_tool.rs`'s
/// `filtered_out_note` looks for.
const FILTERED_PASS_CMD: &str = "sh -c 'echo \"test result: ok. 1 passed; 0 failed; 0 ignored; \
                                  0 measured; 17 filtered out; finished in 0.00s\"'";

/// A shell command that exits 0 with no filtered-out line.
const CLEAN_PASS_CMD: &str = "sh -c 'echo \"test result: ok. 1 passed; 0 failed; 0 ignored; 0 \
                               measured; 0 filtered out; finished in 0.00s\"'";

/// A shell command that exits 1 (fail), whose own output happens to
/// mention a filtered count -- must still clear both fields, not just
/// `green_step`.
const FAIL_FILTERED_CMD: &str = "sh -c 'echo \"test result: FAILED. 0 passed; 1 failed; 0 \
                                  ignored; 0 measured; 17 filtered out; finished in 0.00s\"; \
                                  exit 1'";

// =======================================================================
// A passing `test` run whose output notes a filtered count sets
// `green_filtered: true` alongside `green_step: Some(step)`.
// =======================================================================

#[tokio::test]
async fn filtered_pass_sets_green_filtered_true() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        test_action(FILTERED_PASS_CMD),
    )]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        real_tools(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 3),
    );
    let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, Some(1));
    assert!(
        state.harness.green_filtered,
        "a filtered passing run must set green_filtered: true"
    );
}

// =======================================================================
// A later passing `test` run WITHOUT the filtered note clears
// `green_filtered` back to `false` (green_step still updates normally).
// =======================================================================

#[tokio::test]
async fn later_clean_pass_clears_green_filtered() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action(FILTERED_PASS_CMD)),
        step_output(serde_json::json!({}), test_action(CLEAN_PASS_CMD)),
    ]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        real_tools(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 3),
    );
    let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.harness.green_filtered);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, Some(2));
    assert!(
        !state.harness.green_filtered,
        "an unfiltered passing run must clear green_filtered back to false"
    );
}

// =======================================================================
// A failing `test` run sets both `green_step: None` and
// `green_filtered: false`, even when the failing run's own output happens
// to mention a filtered count.
// =======================================================================

#[tokio::test]
async fn failing_test_clears_green_step_and_green_filtered() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action(FILTERED_PASS_CMD)),
        step_output(serde_json::json!({}), test_action(FAIL_FILTERED_CMD)),
    ]);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        real_tools(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 3),
    );
    let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, Some(1));
    assert!(state.harness.green_filtered);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step, None,
        "a failing test must clear green_step"
    );
    assert!(
        !state.harness.green_filtered,
        "a failing test must clear green_filtered, regardless of its own output"
    );
}

// =======================================================================
// The Harness View's `verification:` line carries the
// "(filtered; run the unfiltered target before approving)" suffix only
// when `green_filtered` is true.
// =======================================================================

#[test]
fn harness_view_marks_filtered_pass() {
    use statie_core::{HarnessView, Promptable};

    let budgets = Budgets {
        total: 1000,
        observation: 100,
        augmentation: 100,
        notes: 400,
        working_set: 6000,
    };

    let mut state = HarnessState {
        budgets,
        activation: "toy".to_string(),
        active_tools: vec![],
        inbox: vec![],
        step: 2,
        last_overflow: None,
        pinned: None,
        green_step: Some(2),
        green_filtered: true,
        tool_calls: 1,
        last_test_cmd: None,
        edits_since_test: 0,
    };

    let view = HarnessView::new(&state);
    let rendered = view.render();
    assert!(
        rendered.contains(
            "verification: passed at step 2 (filtered; run the unfiltered target before \
             approving)"
        ),
        "expected the filtered suffix, got: {rendered}"
    );

    state.green_filtered = false;
    let view = HarnessView::new(&state);
    let rendered = view.render();
    assert!(
        rendered.contains("verification: passed at step 2\n"),
        "expected the plain line with no filtered suffix, got: {rendered}"
    );
    assert!(
        !rendered.contains("filtered"),
        "the plain (non-filtered) case must not mention filtering at all, got: {rendered}"
    );
}
