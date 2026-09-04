//! Integration tests for the repeated-failure note on `test` tool output:
//! the harness should notice when the SAME `test` failure summary recurs
//! across consecutive `test` calls, and say so in the observation,
//! distinguishing whether an edit happened in between.
//!
//! Fixture style copied from `core/tests/green_checkpoint.rs` (core cannot
//! depend on `statie-skills`/`statie-store`/`statie-models` as a normal
//! dependency; this test uses a minimal in-memory `StateStore` and a
//! scripted `Model`, plus fake `test`/`edit_file` tools so the failure text
//! and edit timing are fully controlled rather than depending on real
//! sandboxed shell output).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, Model, ModelError, NullSink, PatchError, PatchStrategy, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet, Version,
};

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

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
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
// Test workspace: same shape as `core/tests/green_checkpoint.rs`'s.
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
/// slot, no silent-read cap -- neither is relevant to this guard.
struct RepeatSkill;

impl Skill for RepeatSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "repeat"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "repeat",
            "a skill for repeated-failure guard tests",
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
// it verbatim (the tool's own args are irrelevant to the guard, so a fixed
// stand-in action is fine).
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

/// A no-op `edit_file` that always "succeeds" -- fine for these tests since
/// none of them assert on file content, only on the repeated-failure note.
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

fn edit_action() -> Value {
    serde_json::json!({"type": "tool", "name": "edit_file", "args": {"path": "a.txt", "old": "x", "new": "y"}})
}

fn ask_user_action(message: &str) -> Value {
    serde_json::json!({"type": "ask_user", "message": message})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn find_event<'a>(rec: &'a statie_core::StepRecord, event: &str) -> Option<&'a Value> {
    rec.entries().iter().find_map(|e| {
        (e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event))
            .then_some(&e.payload)
    })
}

/// A fail output with the `--- failure summary ---` marker, mirroring
/// `tools/src/test_tool.rs::format_result`'s shape on a non-zero exit.
fn fail_output(summary: &str) -> String {
    format!(
        "command: cargo test\nresult: fail (exit 1)\n--- failure summary ---\n{summary}\n\
         --- stdout ---\nsome noise\n--- stderr ---\nmore noise\n"
    )
}

fn pass_output() -> String {
    "command: cargo test\nresult: pass\nexit: 0\n--- stdout ---\nok\n--- stderr ---\n".to_string()
}

const SAME_SUMMARY: &str = "unexpected closing delimiter: `}`";
const OTHER_SUMMARY: &str = "error[E0308]: mismatched types";

const NOTHING_EDITED_NOTE_2: &str = "note: this failure is identical to the previous test run's \
     (2 runs in a row) and nothing was edited in between. Read the exact lines the error points \
     to, then fix that.\n\n";

const EDITED_NOTE_2: &str = "note: this failure is identical to the previous test run's (2 runs \
     in a row); your edit(s) since did not change it. Before editing again, read the exact \
     lines the error points to and fix that, not something else.\n\n";

const EDITED_NOTE_3: &str = "note: this failure is identical to the previous test run's (3 runs \
     in a row); your edit(s) since did not change it. Before editing again, read the exact \
     lines the error points to and fix that, not something else.\n\n";

// =======================================================================
// (a) Two consecutive fails, same summary, no edit in between: second
// observation carries the "nothing was edited" note with count 2; a
// `repeated_failure` entry with count 2 / mutated_since_previous: false is
// logged; the FIRST fail logs no such entry and has no note.
// =======================================================================

#[tokio::test]
async fn two_identical_fails_no_edit_between_note_nothing_edited() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action("cargo test 1")),
        step_output(serde_json::json!({}), test_action("cargo test 2")),
        step_output(serde_json::json!({}), ask_user_action("done for now")),
    ]);
    let prompts_handle = Arc::clone(&model);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(vec![fail_output(SAME_SUMMARY), fail_output(SAME_SUMMARY)]),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(RepeatSkill)).await.unwrap();

    // Step 1: first fail -- no repeated_failure entry.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        find_event(&rec1, "repeated_failure").is_none(),
        "the first fail must not log a repeated_failure entry: {:?}",
        rec1.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: second, identical fail -- repeated_failure entry with count 2,
    // mutated_since_previous: false. The prompt used for THIS step (built
    // from step 1's observation, before step 2 ran) must NOT carry any
    // repeated-failure note -- only one fail had happened when it was
    // built.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_2 = prompts_handle.prompts()[1].clone();
    assert!(
        !prompt_for_step_2.contains("note: this failure is identical"),
        "the prompt after only one fail must carry no repeated-failure note: {prompt_for_step_2}"
    );
    let entry = find_event(&rec2, "repeated_failure").expect("expected a repeated_failure entry");
    assert_eq!(entry["step"], 2);
    assert_eq!(entry["count"], 2);
    assert_eq!(entry["mutated_since_previous"], false);
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3's prompt (built from step 2's observation) carries the exact
    // note text.
    let _ = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_3 = prompts_handle.prompts()[2].clone();
    assert!(
        prompt_for_step_3.contains(NOTHING_EDITED_NOTE_2),
        "expected the exact 'nothing was edited' note in the next prompt, got: {prompt_for_step_3}"
    );
}

// =======================================================================
// (b) Fail, successful edit_file, same fail again: note says "your edit(s)
// since did not change it", entry has mutated_since_previous: true, count
// 2. A third identical fail (with another edit in between) -> count 3.
// =======================================================================

#[tokio::test]
async fn fail_edit_same_fail_notes_edit_did_not_change_it_then_counts_up() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action("cargo test")), // step 1: fail
        step_output(serde_json::json!({}), edit_action()),             // step 2: edit
        step_output(serde_json::json!({}), test_action("cargo test")), // step 3: fail (same, count 2)
        step_output(serde_json::json!({}), edit_action()),             // step 4: edit
        step_output(serde_json::json!({}), test_action("cargo test")), // step 5: fail (same, count 3)
        step_output(serde_json::json!({}), ask_user_action("done for now")), // step 6: read prompt
    ]);
    let prompts_handle = Arc::clone(&model);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(vec![
            fail_output(SAME_SUMMARY),
            fail_output(SAME_SUMMARY),
            fail_output(SAME_SUMMARY),
        ]),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(RepeatSkill)).await.unwrap();

    // Step 1: first fail -- no entry.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(find_event(&rec1, "repeated_failure").is_none());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: edit_file (succeeds) -- sets mutated_since_test.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: same fail again -- mutated_since_previous: true, count 2.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entry3 =
        find_event(&rec3, "repeated_failure").expect("expected a repeated_failure entry at step 3");
    assert_eq!(entry3["count"], 2);
    assert_eq!(entry3["mutated_since_previous"], true);
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    // Step 4: edit_file again.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();

    // Step 5: same fail a third time -- mutated_since_previous: true,
    // count 3.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entry5 =
        find_event(&rec5, "repeated_failure").expect("expected a repeated_failure entry at step 5");
    assert_eq!(entry5["count"], 3);
    assert_eq!(entry5["mutated_since_previous"], true);
    SessionLogExt::append_record(log.as_ref(), rec5).unwrap();

    // Step 6: one more step so the model records the prompt built from
    // step 5's observation.
    let _ = active.step(Event::Tick, &mut NullSink).await.unwrap();

    let prompts = prompts_handle.prompts();
    // prompts[3] is the prompt for step 4 (built off step 3's observation);
    // prompts[5] is the prompt for step 6 (built off step 5's observation).
    assert!(
        prompts[3].contains(EDITED_NOTE_2),
        "expected the count-2 edited note in the prompt for step 4, got: {}",
        prompts[3]
    );
    assert!(
        prompts[5].contains(EDITED_NOTE_3),
        "expected the count-3 edited note in the prompt for step 6, got: {}",
        prompts[5]
    );
}

// =======================================================================
// (c) Fail A, then fail B with a different summary: no note on B, count
// resets to 1 (no entry). Then fail B again -> count 2, note.
// =======================================================================

#[tokio::test]
async fn different_summary_resets_count_then_repeats() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action("cargo test 1")), // step 1: fail A
        step_output(serde_json::json!({}), test_action("cargo test 2")), // step 2: fail B (different)
        step_output(serde_json::json!({}), test_action("cargo test 3")), // step 3: fail B again (same as 2)
        step_output(serde_json::json!({}), ask_user_action("done for now")),
    ]);
    let prompts_handle = Arc::clone(&model);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(vec![
            fail_output(SAME_SUMMARY),
            fail_output(OTHER_SUMMARY),
            fail_output(OTHER_SUMMARY),
        ]),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(RepeatSkill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(find_event(&rec1, "repeated_failure").is_none());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: different summary -- count resets to 1, no entry, no note.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        find_event(&rec2, "repeated_failure").is_none(),
        "a differing failure summary must not log a repeated_failure entry: {:?}",
        rec2.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: same as step 2's summary -- count 2, entry logged. The prompt
    // used for THIS step (built from step 2's observation) must carry no
    // note (step 2 was a fresh, differing failure).
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_3 = prompts_handle.prompts()[2].clone();
    assert!(
        !prompt_for_step_3.contains("note: this failure is identical"),
        "the prompt right after a differing failure must carry no note: {prompt_for_step_3}"
    );
    let entry3 = find_event(&rec3, "repeated_failure").expect("expected a repeated_failure entry");
    assert_eq!(entry3["count"], 2);
    assert_eq!(entry3["mutated_since_previous"], false);
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    let _ = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_4 = prompts_handle.prompts()[3].clone();
    assert!(
        prompt_for_step_4.contains(NOTHING_EDITED_NOTE_2),
        "expected the count-2 note after the repeated OTHER_SUMMARY fail, got: {prompt_for_step_4}"
    );
}

// =======================================================================
// (d) Fail, pass, then the same fail as the first: no note (pass cleared
// the memory); the pass still produced its normal green prefix.
// =======================================================================

#[tokio::test]
async fn pass_clears_repeated_failure_memory() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), test_action("cargo test 1")), // step 1: fail
        step_output(serde_json::json!({}), test_action("cargo test 2")), // step 2: pass
        step_output(serde_json::json!({}), test_action("cargo test 3")), // step 3: fail (same as step 1)
        step_output(serde_json::json!({}), ask_user_action("done for now")),
    ]);
    let prompts_handle = Arc::clone(&model);

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        tool_set(vec![
            fail_output(SAME_SUMMARY),
            pass_output(),
            fail_output(SAME_SUMMARY),
        ]),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );
    let mut active = session.activate(Arc::new(RepeatSkill)).await.unwrap();

    // Step 1: fail.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(find_event(&rec1, "repeated_failure").is_none());
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: pass -- must still log the normal green_checkpoint entry and
    // no repeated_failure entry.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(find_event(&rec2, "repeated_failure").is_none());
    assert!(
        find_event(&rec2, "green_checkpoint").is_some(),
        "the pass must still produce its normal green_checkpoint entry: {:?}",
        rec2.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: fail again, same summary as step 1 -- but the pass at step 2
    // cleared the memory, so this must NOT be treated as a repeat. The
    // prompt used for THIS step (built off step 2's pass observation) must
    // carry the green pass prefix, not a repeated-failure note.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_3 = prompts_handle.prompts()[2].clone();
    assert!(
        prompt_for_step_3.contains("verification passed at step 2"),
        "expected the normal green pass prefix, got: {prompt_for_step_3}"
    );
    assert!(!prompt_for_step_3.contains("note: this failure is identical"));
    assert!(
        find_event(&rec3, "repeated_failure").is_none(),
        "a fail right after a pass must not be treated as a repeat, even with the same summary \
         as a fail two steps back: {:?}",
        rec3.entries()
    );
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();

    let _ = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompt_for_step_4 = prompts_handle.prompts()[3].clone();
    assert!(
        !prompt_for_step_4.contains("note: this failure is identical"),
        "the prompt after step 3's fail must carry no repeated-failure note: {prompt_for_step_4}"
    );
}

// =======================================================================
// (e) Unit tests for the normalization helper are in `core/src/session.rs`
// itself (private function, not exported) -- see
// `normalize_failure_summary_*` in that file's `#[cfg(test)]` module.
// =======================================================================
