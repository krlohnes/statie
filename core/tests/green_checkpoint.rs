//! Integration tests for the green checkpoint (statie-spec.md "10. Working
//! set (recent observations)", subsection "Green checkpoint"): a passing
//! `test` call snapshots the activation's edited files, and a park with a
//! stale (non-passing) most-recent `test` result restores them.
//!
//! Fixture style copied from `core/tests/guards.rs` (core cannot depend on
//! `statie-skills`/`statie-store`/`statie-models` as a normal dependency;
//! this test uses `statie-tools` -- a dev-dependency added for this unit --
//! for the real `edit_file`/`write_file`/`test` tools, so checkpointing is
//! exercised against real sandboxed file I/O, not a stub).

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
    SkillSpec, StateStore, StepSink, StoreError, ToolSet, Version,
};
use statie_tools::{ToolConfig, default_tools};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/guards.rs`).
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
// (copied from `core/tests/guards.rs`).
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

    /// Same as `new`, but the last entries are raw (already-`String`)
    /// content instead of a `Value` to be serialized -- used to script a
    /// deliberately malformed (non-JSON) model output that fails to parse,
    /// to drive a rejection (and, past the retry cap, `Park::Failure`).
    fn new_with_raw_tail(turns: Vec<Value>, raw_tail: Vec<&str>) -> Arc<ScriptedModel> {
        let mut all: Vec<String> = turns.into_iter().map(|v| v.to_string()).collect();
        all.extend(raw_tail.into_iter().map(str::to_string));
        Arc::new(ScriptedModel {
            turns: Mutex::new(all),
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
// Test workspace: same shape as `core/tests/guards.rs`'s (`value`,
// `notes`, `next`, `done`), reused so `MergePatch` behaves identically.
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
/// to the green checkpoint.
struct GreenSkill;

impl Skill for GreenSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "green"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "green",
            "a skill for green-checkpoint tests",
        ))
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

fn write_action(path: &str, content: &str) -> Value {
    serde_json::json!({
        "type": "tool",
        "name": "write_file",
        "args": {"path": path, "content": content}
    })
}

fn edit_action(path: &str, old: &str, new: &str) -> Value {
    serde_json::json!({
        "type": "tool",
        "name": "edit_file",
        "args": {"path": path, "old": old, "new": new}
    })
}

fn test_action(cmd: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "test", "args": {"cmd": cmd}})
}

fn ask_user_action(message: &str) -> Value {
    serde_json::json!({"type": "ask_user", "message": message})
}

fn done_action() -> Value {
    serde_json::json!({"type": "done"})
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

const EXPECTED_PASS_PREFIX: &str = "verification passed at step 2; the files you edited are \
     checkpointed. Finish the plan's bookkeeping and hand off (`done` with \
     `next_skill: review`); cleanup beyond the task is review's call.\n\n";

// =======================================================================
// (a) edit a file, run a passing test -> green_checkpoint entry + exact
// observation prefix on the *next* prompt.
// =======================================================================

#[tokio::test]
async fn passing_test_checkpoints_edited_files_and_prefixes_next_observation() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
        step_output(serde_json::json!({"done": true}), done_action()),
    ]);
    let prompts_handle = Arc::clone(&model);

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

    // Step 1: write_file a.txt.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: test (passes) -> checkpoint.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let checkpoint_entry =
        find_event(&rec2, "green_checkpoint").expect("expected a green_checkpoint entry");
    assert_eq!(checkpoint_entry["step"], 2);
    assert_eq!(
        checkpoint_entry["paths"],
        serde_json::json!(["a.txt"]),
        "{checkpoint_entry:?}"
    );
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: done -- but first, confirm the prompt built for THIS step
    // (the one right after the pass) carries the exact prefix.
    let _rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompts = prompts_handle.prompts();
    let last_prompt = prompts.last().expect("expected a prompt for step 3");
    assert!(
        last_prompt.contains(EXPECTED_PASS_PREFIX),
        "expected the exact prefix in the next prompt, got: {last_prompt}"
    );
}

// =======================================================================
// (b) bad edit + failing test, drive to a retry-cap failure -> file
// restored to checkpointed content, restored_green entry, park message
// carries the restore sentence.
// =======================================================================

#[tokio::test]
async fn retry_cap_failure_restores_checkpoint_when_last_test_failed() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new_with_raw_tail(
        vec![
            step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
            step_output(
                serde_json::json!({}),
                test_action("sh -c 'grep -q PASS a.txt'"),
            ),
            step_output(
                serde_json::json!({}),
                edit_action("a.txt", "PASS", "BROKEN"),
            ),
            step_output(
                serde_json::json!({}),
                test_action("sh -c 'grep -q PASS a.txt'"),
            ),
        ],
        // Malformed JSON: fails to parse, rejects, and with retries: 0
        // immediately hits the retry cap.
        vec!["not valid json at all"],
    );

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        real_tools(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 0),
    );
    let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();

    // Step 1: write_file a.txt = "PASS\n".
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    // Step 2: test passes -> checkpoint content "PASS\n".
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(find_event(&rec2, "green_checkpoint").is_some());
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // Step 3: edit_file breaks the file.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "BROKEN\n"
    );

    // Step 4: test fails -> last_test_result = false.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();

    // Step 5: malformed model output -> rejected -> retry cap (retries: 0)
    // -> Park::Failure, with the checkpoint restored.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let restored_entry =
        find_event(&rec5, "restored_green").expect("expected a restored_green entry");
    assert_eq!(restored_entry["from_step"], 2);
    assert_eq!(restored_entry["paths"], serde_json::json!(["a.txt"]));

    match SessionLogExt::append_record(log.as_ref(), rec5).unwrap() {
        Outcome::Parked(Park::Failure(reason)) => {
            assert!(
                reason.contains(
                    "Restored the files to the state where the test passed at step 2; the \
                     edits since are in the log."
                ),
                "park reason missing restore sentence: {reason}"
            );
        }
        other => panic!("expected Park::Failure, got {other:?}"),
    }

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS\n",
        "file must be restored to the checkpointed (byte-identical) content"
    );
}

// =======================================================================
// (c) same as (b), but the last test passes -> no restore, newer content
// kept.
// =======================================================================

#[tokio::test]
async fn ask_user_does_not_restore_when_last_test_passed() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
        step_output(serde_json::json!({}), write_action("a.txt", "PASS TOO\n")),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
        step_output(serde_json::json!({}), ask_user_action("done, need input")),
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

    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }

    // Step 5: ask_user, with the most recent test (step 4) a pass ->
    // no restore.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        find_event(&rec5, "restored_green").is_none(),
        "must not restore when the last test passed: {:?}",
        rec5.entries()
    );
    match SessionLogExt::append_record(log.as_ref(), rec5).unwrap() {
        Outcome::Parked(Park::AskUser(message)) => {
            assert!(
                !message.contains("Restored the files"),
                "park message must not carry the restore sentence: {message}"
            );
        }
        other => panic!("expected Park::AskUser, got {other:?}"),
    }

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS TOO\n",
        "the newer (post-checkpoint) content must be kept"
    );
}

// =======================================================================
// (d) a file created by write_file AFTER the checkpoint (absent at
// checkpoint time) is deleted on restore.
// =======================================================================

#[tokio::test]
async fn restore_deletes_a_file_created_after_the_checkpoint() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new_with_raw_tail(
        vec![
            step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
            step_output(
                serde_json::json!({}),
                test_action("sh -c 'grep -q PASS a.txt'"),
            ),
            step_output(
                serde_json::json!({}),
                write_action("b.txt", "created after checkpoint\n"),
            ),
            step_output(
                serde_json::json!({}),
                test_action("sh -c 'grep -q NOPE a.txt'"), // fails: a.txt has no NOPE
            ),
        ],
        vec!["not valid json at all"],
    );

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        real_tools(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 0),
    );
    let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();

    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    assert!(root.path().join("b.txt").exists());

    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let restored_entry =
        find_event(&rec5, "restored_green").expect("expected a restored_green entry");
    let paths: Vec<String> = restored_entry["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(paths.contains(&"b.txt".to_string()), "{paths:?}");
    let _ = SessionLogExt::append_record(log.as_ref(), rec5).unwrap();

    assert!(
        !root.path().join("b.txt").exists(),
        "b.txt did not exist at checkpoint time and must be deleted on restore"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "PASS\n"
    );
}

// =======================================================================
// (e) a file over the cap is listed under `skipped` and not restored.
// =======================================================================

#[tokio::test]
async fn oversize_file_is_skipped_not_checkpointed() {
    let root = tempfile::tempdir().unwrap();
    // Pre-populate an oversize file directly on disk (writing this much
    // through write_file's own content argument would work too, but this
    // is simpler and exercises the same snapshot path). A unique marker at
    // the end gives `edit_file` a single-occurrence anchor to replace.
    let big_path = root.path().join("big.txt");
    let mut big_content = vec![b'x'; (1 << 20) + 1];
    big_content.extend_from_slice(b"MARKER");
    std::fs::write(&big_path, &big_content).unwrap();

    let model = ScriptedModel::new(vec![
        // edit_file on the pre-existing big file: a trivial no-op-ish
        // edit that still counts as a successful edit_file action.
        step_output(
            serde_json::json!({}),
            edit_action("big.txt", "MARKER", "PASS"),
        ),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS big.txt'"),
        ),
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

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let checkpoint_entry =
        find_event(&rec2, "green_checkpoint").expect("expected a green_checkpoint entry");
    assert_eq!(
        checkpoint_entry["paths"],
        serde_json::json!([]),
        "the oversize file must not appear in the checkpointed paths: {checkpoint_entry:?}"
    );
    assert_eq!(
        checkpoint_entry["skipped"],
        serde_json::json!(["big.txt"]),
        "{checkpoint_entry:?}"
    );
}

// =======================================================================
// (f) `HarnessState.green_step` (harness-owned, committed alongside `step`/
// `active_tools`): set to the checkpointing step on a passing `test`, and
// cleared back to `None` on a later failing `test`. This is what skills see
// as `cur.harness.green_step` in `Skill::validate`/`rules`.
// =======================================================================

#[tokio::test]
async fn green_step_is_set_on_committed_harness_after_a_passing_test() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
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

    // Step 1: write_file a.txt.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step, None,
        "green_step must still be unset before any test has run"
    );

    // Step 2: test (passes) -> harness.green_step must now be Some(2).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        Some(2),
        "the committed harness state must record the checkpointing step"
    );
}

#[tokio::test]
async fn green_step_is_cleared_on_committed_harness_after_a_later_failing_test() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), write_action("a.txt", "PASS\n")),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
        step_output(
            serde_json::json!({}),
            edit_action("a.txt", "PASS", "BROKEN"),
        ),
        step_output(
            serde_json::json!({}),
            test_action("sh -c 'grep -q PASS a.txt'"),
        ),
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

    for _ in 0..2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    }
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, Some(2));

    // Step 3: edit_file breaks the file (green_step must be untouched by a
    // non-`test` tool call).
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec3).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        Some(2),
        "a non-test tool call must not touch green_step"
    );

    // Step 4: test fails -> green_step must be cleared back to None.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec4).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step, None,
        "a later failing test must clear green_step"
    );
}

// =======================================================================
// (g) `HarnessState.tool_calls` (harness-owned, committed alongside
// `step`/`green_step`): the number of tool calls that actually executed in
// the current activation. 0 after activation, 1 after one executed tool
// call, still 1 after a rejected step (a rejection never commits, so
// nothing executed), and 0 again after a handoff to a new activation.
// =======================================================================

#[tokio::test]
async fn tool_calls_is_zero_after_activation_one_after_an_executed_call() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        write_action("a.txt", "PASS\n"),
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

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 0,
        "tool_calls must be 0 right after activation, before any step has run"
    );

    // Step 1: write_file a.txt -- one executed tool call.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "tool_calls must be 1 after one executed tool call"
    );
}

#[tokio::test]
async fn tool_calls_is_unchanged_by_a_rejected_step() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new_with_raw_tail(
        vec![step_output(
            serde_json::json!({}),
            write_action("a.txt", "PASS\n"),
        )],
        // Malformed JSON: fails to parse, so this step is rejected and
        // never commits -- nothing executes.
        vec!["not valid json at all"],
    );

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

    // Step 1: write_file a.txt -- one executed tool call.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1);

    // Step 2: malformed model output -> rejected, never commits.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        find_event(&rec2, "rejected").is_some(),
        "expected the malformed output to be rejected: {:?}",
        rec2.entries()
    );
    let outcome2 = SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "expected the rejected step to still return Continue (under the retry cap): {outcome2:?}"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "tool_calls must be unchanged by a rejected step (it never commits)"
    );
}

#[tokio::test]
async fn tool_calls_resets_to_zero_on_a_new_activation() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        write_action("a.txt", "PASS\n"),
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
    {
        let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();
        let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
        let (state, _version) = active.session().state().await.unwrap();
        assert_eq!(state.harness.tool_calls, 1);
    }

    // A fresh activation (a "handoff" in spirit -- `activate_inner` runs
    // again, the same reset path a real skill-to-skill handoff takes) must
    // reset the counter back to 0, even though the harness state otherwise
    // persists across activations.
    let mut active2 = session.activate(Arc::new(GreenSkill)).await.unwrap();
    let (state, _version) = active2.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 0,
        "tool_calls must reset to 0 on a new activation"
    );
}

// =======================================================================
// (h) `HarnessState.last_test_cmd` (harness-owned, committed alongside
// `green_step`/`green_filtered` at the same `test`-call site,
// `core/src/session.rs`): the `cmd` the most recent `test` call ran, as
// given in its args -- `Some` when the call gave a `cmd`, `None` when it
// omitted one (auto-detect) or when no `test` call has run yet. Unlike
// `tool_calls`, it survives a handoff to a new activation, the same as
// `green_step`.
// =======================================================================

#[tokio::test]
async fn last_test_cmd_is_set_from_a_test_call_with_cmd() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        test_action("sh -c 'exit 0'"),
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

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.last_test_cmd, None,
        "last_test_cmd must be unset before any test has run"
    );

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    SessionLogExt::append_record(log.as_ref(), rec1).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.last_test_cmd,
        Some("sh -c 'exit 0'".to_string()),
        "last_test_cmd must record the cmd the test call gave"
    );
}

#[tokio::test]
async fn last_test_cmd_stays_none_after_a_test_call_with_no_cmd() {
    let root = tempfile::tempdir().unwrap();

    // No `cmd` key at all -- the tool auto-detects (and, with no project
    // markers in this bare sandbox, fails to find one; that failure is
    // irrelevant here, only `last_test_cmd` staying `None` is checked).
    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        serde_json::json!({"type": "tool", "name": "test", "args": {}}),
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
    assert_eq!(
        state.harness.last_test_cmd, None,
        "a `test` call with no `cmd` (auto-detect) must leave last_test_cmd None"
    );
}

#[tokio::test]
async fn last_test_cmd_survives_a_new_activation() {
    let root = tempfile::tempdir().unwrap();

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        test_action("sh -c 'exit 0'"),
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
    {
        let mut active = session.activate(Arc::new(GreenSkill)).await.unwrap();
        let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
        SessionLogExt::append_record(log.as_ref(), rec1).unwrap();
        let (state, _version) = active.session().state().await.unwrap();
        assert_eq!(
            state.harness.last_test_cmd,
            Some("sh -c 'exit 0'".to_string())
        );
        assert_eq!(state.harness.tool_calls, 1);
    }

    // A fresh activation resets `tool_calls` back to 0 (see the sibling
    // test above), but `last_test_cmd` -- like `green_step` -- is not
    // reset: it persists across the handoff.
    let mut active2 = session.activate(Arc::new(GreenSkill)).await.unwrap();
    let (state, _version) = active2.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 0,
        "tool_calls must reset to 0 on a new activation"
    );
    assert_eq!(
        state.harness.last_test_cmd,
        Some("sh -c 'exit 0'".to_string()),
        "last_test_cmd must survive the new activation, unlike tool_calls"
    );
}
