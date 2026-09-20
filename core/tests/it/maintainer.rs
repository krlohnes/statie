//! Integration tests for context maintenance (statie-spec.md "9. Context
//! maintenance"): `Session::with_maintainer`, the `Maintainer` call after a
//! committed step, and the harness-owned `notes` field it maintains. Core
//! cannot depend on `statie-skills`/`statie-store`/`statie-models`, so this
//! test builds its own minimal `StateStore`, `Model`, and `Skill` stubs, in
//! the same spirit as `core/tests/delegate.rs` and `core/tests/user_patch.rs`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::MockMaintainer;
use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, MaintainError, MaintainOutput, Maintainer, Model, ModelError, NullSink,
    Outcome, Park, ParsedOutput, PatchError, PatchStrategy, RawOutput, SandboxRoot, Schema,
    Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore, StepSink,
    StoreError, ToolSet, ValidatedStep, Version, validate_default,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied in spirit from
// `core/tests/user_patch.rs`; core has no test-only shared crate).
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
// A scripted `Model` that also records the prompt text it was given, so
// tests can assert on the rendered Notes section.
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

/// Same as `RecordingModel::step`, but also attaches `Reasoning` text so
/// the maintainer receives a non-empty `reasoning` input.
struct ReasoningModel {
    turns: Mutex<Vec<(String, String)>>,
}

impl ReasoningModel {
    fn new(turns: Vec<(String, String)>) -> Arc<ReasoningModel> {
        Arc::new(ReasoningModel {
            turns: Mutex::new(turns),
        })
    }
}

#[async_trait]
impl Model for ReasoningModel {
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
        let (reasoning, content) = turns.remove(0);
        Ok(RawOutput::new(
            prompt.version(),
            Some(statie_core::Reasoning::new(reasoning)),
            content,
        ))
    }
}

// ---------------------------------------------------------------------
// Test workspace: `value: i64`, `notes: String` (harness-owned), `next:
// Option<Commitment>`. A real JSON-merge-patch strategy over top-level
// keys, like `statie_patch::JsonMergePatch` (reimplemented here since core
// cannot depend on that crate), so a partial patch (e.g. `{"notes": ...}`)
// only touches the key(s) it names.
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

/// A skill with `notes` harness-owned and `next` as the commitment slot,
/// active tools `["bump"]`.
struct MaintainedSkill;

impl Skill for MaintainedSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "maintained"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("maintained", "a skill for maintainer tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "bump".to_string(),
            "edit_file".to_string(),
            "write_file".to_string(),
        ]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn notes(&self, ws: &Workspace) -> Option<String> {
        if ws.notes.is_empty() {
            None
        } else {
            Some(ws.notes.clone())
        }
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }
}

fn session_config(root: &tempfile::TempDir, notes_budget: usize) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 1000,
            augmentation: 500,
            notes: notes_budget,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

/// A `Tool` action that always "succeeds" trivially: `bump` with no args.
fn bump_tool_call() -> Value {
    serde_json::json!({"type": "tool", "name": "bump", "args": {}})
}

/// Unit 43: the maintainer call spawned for a step is only awaited and
/// committed at the END of the FOLLOWING step's own `step()` call (spec
/// item 1 -- "at most one maintainer call in flight; every prompt sees
/// notes at most one step stale"). Every test in this file that asserts on
/// a maintainer call's committed effects (the `maintainer`/`maintainer_error`
/// log entries, the committed `notes`/`next`, `StepSink::maintainer_done`/
/// `maintainer_failed`) therefore needs a second, otherwise-uninteresting
/// step to force that drain -- this helper runs one (a plain `bump` turn,
/// scripted on `model` as the NEXT turn) and returns its `StepRecord`.
async fn drain_step<K>(active: &mut statie_core::Active<'_, K>) -> statie_core::StepRecord
where
    K: statie_core::Skill<Workspace = Workspace> + ?Sized,
{
    active.step(Event::Tick, &mut NullSink).await.unwrap()
}

fn commitment_json(tool: &str, path: Option<&str>, what: &str) -> Value {
    serde_json::json!({"tool": tool, "path": path, "what": what})
}

// ---------------------------------------------------------------------
// 1. After a committed tool step, the maintainer is called once with the
//    reasoning text, the observation text, the raw patch, and the
//    committed agent state.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_called_once_after_committed_tool_step_with_expected_inputs() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ReasoningModel::new(vec![
        (
            "decided to bump the counter because X".to_string(),
            serde_json::json!({
                "patch": {"value": 1},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string(),
        ),
        // Unit 43: the maintainer call step 1 spawns is only awaited and
        // committed at the end of THIS (step 2's) `step()` call -- a plain
        // second bump turn, whose own effects this test doesn't otherwise
        // care about, forces that drain.
        (
            String::new(),
            serde_json::json!({
                "patch": {"value": 2},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string(),
        ),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "bumped value to 1".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let calls = maintainer.calls();
    assert_eq!(calls.len(), 1, "expected exactly one maintainer call");
    let call = &calls[0];
    assert_eq!(
        call.reasoning.as_deref(),
        Some("decided to bump the counter because X")
    );
    assert_eq!(call.patch, serde_json::json!({"value": 1}));
    assert!(matches!(&call.action, Action::Tool { name, .. } if name == "bump"));
    assert!(
        call.observation.contains("bump") || !call.observation.trim().is_empty(),
        "observation should be non-empty tool output text: {:?}",
        call.observation
    );
    assert_eq!(
        call.agent.get("value").and_then(Value::as_i64),
        Some(1),
        "agent snapshot should be the committed post-step state: {:?}",
        call.agent
    );
    assert_eq!(call.commitment, None);
    assert_eq!(call.notes, "");
    assert_eq!(call.budget, 400);
    assert_eq!(
        call.active_tools,
        vec![
            "bump".to_string(),
            "edit_file".to_string(),
            "write_file".to_string(),
        ],
        "the maintainer should receive the harness's active tool list"
    );
}

// ---------------------------------------------------------------------
// 1b. The `maintainer` System entry carries `reasoning_chars`/
//     `observation_chars` -- the maintainer's own output says nothing about
//     what it was given, so this is a run's only way to tell whether the
//     maintainer ever actually saw any reasoning.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_entry_records_reasoning_chars_when_reasoning_given() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let reasoning_text = "decided to bump the counter because X";
    let model = ReasoningModel::new(vec![
        (
            reasoning_text.to_string(),
            serde_json::json!({
                "patch": {"value": 1},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string(),
        ),
        // Unit 43: forces the drain of step 1's maintainer call.
        (
            String::new(),
            serde_json::json!({
                "patch": {"value": 2},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string(),
        ),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "bumped value to 1".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // The spawned call only actually runs once `drain_step` above has
    // awaited it, so `calls()` is only meaningful from this point on (unit
    // 43: `spawn_maintainer` never awaits the task it starts).
    let calls = maintainer.calls();
    let expected_observation_chars = calls[0].observation.len();

    let entries = log.entries(None, 100);
    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry");
    assert_eq!(
        maintainer_event
            .payload
            .get("reasoning_chars")
            .and_then(Value::as_u64),
        Some(reasoning_text.len() as u64),
        "reasoning_chars must equal the given reasoning's length: {maintainer_event:?}"
    );
    assert_eq!(
        maintainer_event
            .payload
            .get("observation_chars")
            .and_then(Value::as_u64),
        Some(expected_observation_chars as u64),
        "observation_chars must equal the observation text's length: {maintainer_event:?}"
    );
}

#[tokio::test]
async fn maintainer_entry_records_zero_reasoning_chars_when_reasoning_absent() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "bumped value to 1".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    // The spawned call only actually runs once `drain_step` above has
    // awaited it, so `calls()` is only meaningful from this point on (unit
    // 43: `spawn_maintainer` never awaits the task it starts).
    let calls = maintainer.calls();
    assert_eq!(calls[0].reasoning, None, "this model gives no reasoning");

    let entries = log.entries(None, 100);
    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry");
    assert_eq!(
        maintainer_event
            .payload
            .get("reasoning_chars")
            .and_then(Value::as_u64),
        Some(0),
        "reasoning_chars must be 0 when no reasoning was given: {maintainer_event:?}"
    );
}

// ---------------------------------------------------------------------
// 2. The maintainer's notes appear in the notes header, one step stale
//    (unit 43, spec item 1): the maintainer spawned right after step 1
//    commits is only awaited and committed at the end of step 2's own
//    `step()` call, so step 2's own prompt (already built before that
//    point) never carries it -- only step 3's does.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_notes_appear_in_next_prompt_under_notes_header() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        serde_json::json!({
            "patch": {},
            "action": {"type": "yield"},
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept: the edit target was line 42".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    // Step 2: its own prompt (already recorded by the time this call
    // returns) is built BEFORE this call's end-of-step drain runs, so it
    // must carry no notes header at all yet.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    // Step 3: its prompt is built after step 2's own `step()` call drained
    // and committed step 1's maintainer result -- this is the first prompt
    // that can carry the notes header.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    let second_prompt = &prompts[1];
    assert!(
        !second_prompt.contains("notes (kept by the harness from your previous reasoning)"),
        "step 2's prompt is built before any maintainer result has been \
         drained, so it must carry no notes header at all:\n{second_prompt}"
    );
    let third_prompt = &prompts[2];
    assert!(
        third_prompt
            .contains("notes (kept by the harness from your previous reasoning) (as of step 1)"),
        "expected a notes header naming step 1 in the third prompt:\n{third_prompt}"
    );
    assert!(
        third_prompt.contains("kept: the edit target was line 42"),
        "expected the maintainer's notes text in the third prompt:\n{third_prompt}"
    );
}

// ---------------------------------------------------------------------
// 3. A proposed `next` is applied when the workspace's `next` is null, and
//    NOT applied when the model already set one.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_proposed_next_applied_when_workspace_commitment_is_null() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let proposed = Commitment {
        tool: "edit_file".to_string(),
        path: Some("a.rs".to_string()),
        what: "delete the duplicated block".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "n".to_string(),
        next: Some(proposed.clone()),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.next, Some(proposed));

    let entries = log.entries(None, 100);
    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(true)
    );
}

// ---------------------------------------------------------------------
// 3b. A proposed `next` whose tool is not in the harness's active tool
//    list is dropped (never applied), the notes still land, and a
//    `maintainer_error` entry names the inactive tool.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_proposed_next_with_inactive_tool_is_dropped_but_notes_land() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    // `shell` is not in `MaintainedSkill::tools()` (only `bump`, `edit_file`,
    // `write_file` are active), so this proposal must be dropped.
    let proposed = Commitment {
        tool: "shell".to_string(),
        path: Some("tui/tests/driver.rs".to_string()),
        what: "cat tui/tests/driver.rs".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the observation summary".to_string(),
        next: Some(proposed),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next, None,
        "a proposed next using an inactive tool must never be applied"
    );
    assert_eq!(
        state.agent.notes, "kept the observation summary",
        "the notes must still land even when next is dropped"
    );

    let entries = log.entries(None, 100);
    let err_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error"))
        .expect("expected a maintainer_error entry for the inactive-tool proposal");
    let reason = err_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("shell"),
        "reason should name the inactive tool: {reason}"
    );

    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry recording the notes update");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false),
        "next_set must be false: the proposed next was dropped"
    );
}

// ---------------------------------------------------------------------
// 3c. A proposed `next` that is malformed the same ways
//    `strip_malformed_next` (skills/src/coding/mod.rs) checks a
//    worker-written `next` -- not a mutating call, an empty `what`, or a
//    missing `path` on edit_file/write_file -- is dropped, the notes still
//    land, and a `maintainer_error` entry names the reason. This closes the
//    live-run bug where the maintainer proposed `next = {tool:
//    "read_file", ...}`: `read_file` is active, so the old inactive-tool
//    check let it through, and the session's commitment read cap then
//    rejected the model to the retry cap against a commitment no call
//    could ever fulfil.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_proposed_next_non_mutating_tool_is_dropped_but_notes_land() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    // `bump` *is* an active tool for `MaintainedSkill` (unlike `read_file`
    // in the live-run bug this guards), so the inactive-tool check above
    // does not catch it -- only the malformed-next check (not in
    // `MUTATING_TOOLS`) can drop it, which is the point of this test: a
    // read (or any non-mutating call) is never a commitment even when it's
    // an active tool.
    let proposed = Commitment {
        tool: "bump".to_string(),
        path: None,
        what: "bump the counter again".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the observation summary".to_string(),
        next: Some(proposed),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next, None,
        "a proposed next that isn't a mutating call must never be applied"
    );
    assert_eq!(
        state.agent.notes, "kept the observation summary",
        "the notes must still land even when next is dropped"
    );

    let entries = log.entries(None, 100);
    let err_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error"))
        .expect("expected a maintainer_error entry for the non-mutating proposal");
    let reason = err_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason, "proposed next is not a mutating call: `bump`",
        "reason should match strip_malformed_next's wording"
    );

    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry recording the notes update");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false),
        "next_set must be false: the proposed next was dropped"
    );
}

#[tokio::test]
async fn maintainer_proposed_next_empty_what_is_dropped_but_notes_land() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let proposed = Commitment {
        tool: "edit_file".to_string(),
        path: Some("a.rs".to_string()),
        what: "".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the observation summary".to_string(),
        next: Some(proposed),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next, None,
        "a proposed next with an empty what must never be applied"
    );
    assert_eq!(
        state.agent.notes, "kept the observation summary",
        "the notes must still land even when next is dropped"
    );

    let entries = log.entries(None, 100);
    let err_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error"))
        .expect("expected a maintainer_error entry for the empty-what proposal");
    let reason = err_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason, "proposed next has an empty `what`",
        "reason should match strip_malformed_next's wording"
    );

    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry recording the notes update");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false),
        "next_set must be false: the proposed next was dropped"
    );
}

#[tokio::test]
async fn maintainer_proposed_next_missing_path_is_dropped_but_notes_land() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let proposed = Commitment {
        tool: "edit_file".to_string(),
        path: None,
        what: "x".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "kept the observation summary".to_string(),
        next: Some(proposed),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next, None,
        "a proposed edit_file next with no path must never be applied"
    );
    assert_eq!(
        state.agent.notes, "kept the observation summary",
        "the notes must still land even when next is dropped"
    );

    let entries = log.entries(None, 100);
    let err_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error"))
        .expect("expected a maintainer_error entry for the missing-path proposal");
    let reason = err_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason, "proposed next `edit_file` has no `path`",
        "reason should match strip_malformed_next's wording"
    );

    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry recording the notes update");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false),
        "next_set must be false: the proposed next was dropped"
    );
}

#[tokio::test]
async fn maintainer_proposed_next_not_applied_when_worker_already_set_one() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let worker_commitment = commitment_json("write_file", Some("b.rs"), "write the new helper");
    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1, "next": worker_commitment},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer_proposed = Commitment {
        tool: "edit_file".to_string(),
        path: Some("a.rs".to_string()),
        what: "delete the duplicated block".to_string(),
    };
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "n".to_string(),
        next: Some(maintainer_proposed),
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next,
        Some(Commitment {
            tool: "write_file".to_string(),
            path: Some("b.rs".to_string()),
            what: "write the new helper".to_string(),
        }),
        "the worker's own commitment must not be overridden by the maintainer's"
    );

    let entries = log.entries(None, 100);
    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a maintainer system entry");
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false),
        "next_set must be false: the workspace already had a commitment"
    );
}

// ---------------------------------------------------------------------
// 4. Over-budget notes (unit 45): cut to the budget instead of discarded.
//    Live bench evidence: qwen_json_short_b26 discarded 51 of 310
//    maintainer calls for notes over budget; qwen_json_long_b24, 170 of
//    368; most overshoots were 3 to 60 tokens over 400. 4a/4b below pin the
//    exact cut text for a sentence-end cut and a token-boundary cut;
//    both use `HeuristicTokenizer` (ceil(chars/4)) and `budget = 400`, so
//    "notes of exactly budget + 30 tokens" is 1720 chars (430 tokens).
// ---------------------------------------------------------------------

/// 4a. A sentence end (`.`) inside the budget, past the half-budget mark:
/// the committed notes are cut there exactly, not at the raw token
/// boundary. `head` is 800 chars ending in `.` (200 tokens -- exactly half
/// of the 400-token budget, so it qualifies as "leaves at least half the
/// budget"); `tail` pads the total to 1720 chars (430 tokens, i.e.
/// `budget + 30`). The raw token boundary sits at 1600 chars (400 tokens);
/// the last `.` at or before that boundary is the one ending `head`, so
/// the expected cut is `head` verbatim.
#[tokio::test]
async fn over_budget_notes_are_cut_at_the_last_sentence_end_in_budget() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let head = format!("{}.", "A".repeat(799));
    assert_eq!(head.len(), 800);
    let notes = format!("{head}{}", "B".repeat(1720 - head.len()));
    assert_eq!(notes.len(), 1720);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: notes.clone(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.notes, head,
        "notes must be cut exactly at the sentence end, not the raw token boundary"
    );

    let entries = log.entries(None, 100);
    let cut_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_notes_cut"))
        .expect("expected a maintainer_notes_cut system entry");
    assert_eq!(
        cut_entry.payload.get("from").and_then(Value::as_u64),
        Some(430)
    );
    assert_eq!(
        cut_entry.payload.get("to").and_then(Value::as_u64),
        Some(200)
    );

    assert!(
        !entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error")),
        "a cut must not log maintainer_error"
    );
    let maintainer_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .expect("a cut result must be logged as the maintainer event, same as an in-budget result");
    assert_eq!(
        maintainer_event
            .payload
            .get("notes_tokens")
            .and_then(Value::as_u64),
        Some(200),
        "the maintainer event's notes_tokens must be the post-cut count"
    );
    assert_eq!(
        maintainer_event
            .payload
            .get("next_set")
            .and_then(Value::as_bool),
        Some(false)
    );
}

/// 4b. No sentence end anywhere in the notes: the cut falls back to the
/// plain token boundary the tokenizer gives (`largest_prefix_within`),
/// which is 1600 chars (400 tokens) for a 1720-char, all-`C` input.
#[tokio::test]
async fn over_budget_notes_with_no_sentence_end_are_cut_at_the_token_boundary() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let notes = "C".repeat(1720);
    let expected_cut = "C".repeat(1600);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: notes.clone(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.notes, expected_cut,
        "with no sentence end, the cut must fall back to the raw token boundary"
    );

    let entries = log.entries(None, 100);
    let cut_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_notes_cut"))
        .expect("expected a maintainer_notes_cut system entry");
    assert_eq!(
        cut_entry.payload.get("from").and_then(Value::as_u64),
        Some(430)
    );
    assert_eq!(
        cut_entry.payload.get("to").and_then(Value::as_u64),
        Some(400)
    );

    assert!(
        !entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error")),
        "a cut must not log maintainer_error"
    );
}

// ---------------------------------------------------------------------
// 5. `Err(Backend)` from the maintainer: `maintainer_error` logged, step
//    outcome unchanged, previous notes kept.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_backend_error_logs_error_and_does_not_change_step_outcome() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Err(MaintainError::Backend(
        "the maintainer backend is down".to_string(),
    ))]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Continue),
        "a maintainer failure must never change the step outcome"
    );

    let rec2 = drain_step(&mut active).await;
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.notes, "");

    let entries = log.entries(None, 100);
    let err_entry = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer_error"))
        .expect("expected a maintainer_error system entry");
    let reason = err_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("the maintainer backend is down"),
        "{reason}"
    );
}

// ---------------------------------------------------------------------
// 6. Not called on a rejected step; not called after `done`; called after
//    an `ask_user` park.
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_not_called_on_rejected_step() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    // Malformed JSON -> parse error -> rejection, no commit.
    let model = RecordingModel::new(vec!["not json at all {{{".to_string()]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "should never be produced".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        rec.entries()
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "expected a rejected system entry"
    );
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    assert_eq!(
        maintainer.calls().len(),
        0,
        "the maintainer must not be called on a rejected step"
    );
}

#[tokio::test]
async fn maintainer_not_called_after_done() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"done": true},
            "action": {"type": "done"},
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "should never be produced".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Done));

    assert_eq!(
        maintainer.calls().len(),
        0,
        "the maintainer must not be called after done"
    );
}

#[tokio::test]
async fn maintainer_called_after_ask_user_park() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": {"type": "ask_user", "message": "which file?"},
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call -- the
        // session stays parked, so this second turn is delivered via a
        // `User` event the same way a real driver would resume it.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "asked which file".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active
        .step(Event::User("here".to_string()), &mut NullSink)
        .await
        .unwrap();
    let _ = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();

    assert_eq!(
        maintainer.calls().len(),
        1,
        "the maintainer must be called after an ask_user park"
    );
}

// ---------------------------------------------------------------------
// 7. The log has a Patch entry with "source":"maintainer".
// ---------------------------------------------------------------------

#[tokio::test]
async fn maintainer_patch_log_entry_has_source_maintainer() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        // Unit 43: forces the drain of step 1's maintainer call.
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "n".to_string(),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();

    let rec2 = drain_step(&mut active).await;
    let _ = log.append_record(rec2).unwrap();

    let entries = log.entries(Some(LogKind::Patch), 100);
    let maintainer_patch = entries
        .iter()
        .find(|e| e.payload.get("source").and_then(Value::as_str) == Some("maintainer"))
        .expect("expected a Patch entry with source: maintainer");
    assert_eq!(
        maintainer_patch
            .payload
            .get("patch")
            .and_then(|p| p.get("notes")),
        Some(&Value::String("n".to_string()))
    );
}

// ---------------------------------------------------------------------
// 8. A model patch that sets `notes` is rejected with the harness-owned
//    message; the `MockMaintainer` script-exhausted path returns the
//    Backend error.
// ---------------------------------------------------------------------

#[tokio::test]
async fn model_patch_touching_notes_is_rejected_as_harness_owned() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1, "notes": "tampered"},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    );

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let rejected = rec
        .entries()
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(reason.contains("`notes`"), "{reason}");
    assert!(reason.contains("harness-owned"), "{reason}");
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));
}

#[tokio::test]
async fn mock_maintainer_script_exhausted_returns_backend_error() {
    let maintainer = MockMaintainer::new(vec![]);
    let agent = serde_json::json!({});
    let patch = serde_json::json!({});
    let action = Action::Yield;
    let input = statie_core::MaintainInput {
        reasoning: None,
        patch: &patch,
        action: &action,
        observation: "",
        agent: &agent,
        commitment: None,
        notes: "",
        budget: statie_core::TokenBudget(400),
        active_tools: &[],
    };
    let err = maintainer
        .maintain(input)
        .await
        .expect_err("an exhausted script should return an error");
    assert!(matches!(err, MaintainError::Backend(ref m) if m == "script exhausted"));
}

// ---------------------------------------------------------------------
// Unit 46 (D29): the maintainer IS started after a step whose committed
// action was `Action::Continue` -- unit 43's skip rule that withheld the
// spawn for exactly this shape is gone, since build 27 showed it starving
// the notes during gemma's refusal walks. `ContinuingMaintainedSkill`
// mirrors `continue_action.rs`'s own `ContinueSkill` (its `validate`
// override always converts the model's action to the harness-internal
// `Action::Continue`), grafted onto `MaintainedSkill`'s `notes`/
// `commitment` wiring so a maintainer can be attached at all.
// ---------------------------------------------------------------------

struct ContinuingMaintainedSkill;

impl Skill for ContinuingMaintainedSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "continuing-maintained"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "continuing-maintained",
            "a skill that always continues, for the maintainer continue-step tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["bump".to_string()]
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn harness_owned_fields(&self) -> Vec<String> {
        vec!["notes".to_string()]
    }

    fn notes(&self, ws: &Workspace) -> Option<String> {
        if ws.notes.is_empty() {
            None
        } else {
            Some(ws.notes.clone())
        }
    }

    fn commitment(&self, ws: &Workspace) -> Option<Commitment> {
        ws.next.clone()
    }

    fn validate(
        &self,
        cur: &CommittedState<Workspace>,
        mut out: ParsedOutput<Workspace>,
    ) -> Result<ValidatedStep<Workspace>, PatchError> {
        out.set_action(Action::Continue {
            note: "nothing new: keep going".to_string(),
        });
        validate_default::<Workspace, MergePatch>(self, cur, out)
    }
}

#[tokio::test]
async fn maintainer_called_after_a_harness_continue_step() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    // `ContinuingMaintainedSkill::validate` always overrides the action to
    // `Action::Continue` regardless of what the model sends here. Two turns:
    // unit 43's spawn/drain split means the first turn's spawned maintainer
    // call is only awaited and committed at the end of the second turn's
    // own `step()` call (same pattern as `drain_step` elsewhere in this
    // file) -- so a second Continue turn is needed to observe the call.
    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        serde_json::json!({
            "patch": {"value": 2},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let maintainer = Arc::new(MockMaintainer::new(vec![
        Ok(MaintainOutput {
            notes: "notes from the continue step".to_string(),
            next: None,
        }),
        Ok(MaintainOutput {
            notes: "notes from the second continue step".to_string(),
            next: None,
        }),
    ]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(ContinuingMaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let calls_before = maintainer.calls().len();
    assert_eq!(
        calls_before, 0,
        "no maintainer call has been drained yet after only one step"
    );

    let rec2 = drain_step(&mut active).await;
    let outcome2 = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue));

    assert_eq!(
        maintainer.calls().len(),
        calls_before + 1,
        "the maintainer must be called (drained) after a harness Continue step"
    );
}

// ---------------------------------------------------------------------
// Unit 43 acceptance: a five-step scripted session with a maintainer that
// records the step it was called for and returns distinct notes per call.
// Asserts, per step, exactly which notes text the prompt carried and the
// exact `notes_step` in the committed harness state, and that the session
// log's `maintainer` events appear in step order with the same payload
// shape as today.
// ---------------------------------------------------------------------

/// A scripted `Maintainer` that returns `"notes from step {n}"`, where `n`
/// is the 1-based call count (NOT the harness step -- unit 43's whole point
/// is that these two numbers are read from different places: the maintainer
/// itself is never told which harness step it's summarising, only
/// `Active::spawn_maintainer`/`drain_pending_maintainer` track that).
struct SequentialMaintainer {
    calls: Mutex<u64>,
}

impl SequentialMaintainer {
    fn new() -> Arc<SequentialMaintainer> {
        Arc::new(SequentialMaintainer {
            calls: Mutex::new(0),
        })
    }
}

#[async_trait]
impl Maintainer for SequentialMaintainer {
    async fn maintain(
        &self,
        _input: statie_core::MaintainInput<'_>,
    ) -> Result<MaintainOutput, MaintainError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        Ok(MaintainOutput {
            notes: format!("notes from step {calls}"),
            next: None,
        })
    }
}

#[tokio::test]
async fn five_step_session_notes_are_at_most_one_step_stale() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let turns: Vec<String> = (1..=5)
        .map(|v| {
            serde_json::json!({
                "patch": {"value": v},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string()
        })
        .collect();
    let model = RecordingModel::new(turns);

    let maintainer = SequentialMaintainer::new();

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut notes_steps: Vec<Option<u64>> = Vec::new();
    for _ in 0..5 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
        let (state, _version) = active.session().state().await.unwrap();
        notes_steps.push(state.harness.notes_step);
    }

    // `notes_step` is the harness step whose maintainer call was drained --
    // NOT a simple 1-per-turn counter, since a drained maintainer's own
    // notes patch (`apply_trusted_patch`) itself advances `HarnessState.
    // step`, so the raw step counter runs ahead of the worker's own turn
    // count once a maintainer is configured: turn N's own worker commit
    // lands at harness step 1, 2, 4, 6, 8 for N = 1..5 (turn 1 costs one
    // step; every later turn costs two, once its predecessor's drain starts
    // adding its own commit into the mix). `notes_step` names the PREVIOUS
    // turn's own (already-advanced) harness step once drained.
    assert_eq!(notes_steps[0], None, "step 1: no notes committed yet");
    assert_eq!(
        notes_steps[1],
        Some(1),
        "step 2: its own step() call drains and commits step 1's maintainer result (harness step 1) at its end"
    );
    assert_eq!(
        notes_steps[2],
        Some(2),
        "step 3: its own step() call drains the maintainer spawned for harness step 2"
    );
    assert_eq!(
        notes_steps[3],
        Some(4),
        "step 4: its own step() call drains the maintainer spawned for harness step 4"
    );
    assert_eq!(
        notes_steps[4],
        Some(6),
        "step 5: its own step() call drains the maintainer spawned for harness step 6"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 5);
    // Step 1 and step 2's prompts carry no notes header at all (no notes
    // have been committed yet when either is built).
    assert!(
        !prompts[0].contains("notes (kept by the harness from your previous reasoning)"),
        "step 1's prompt must carry no notes header:\n{}",
        prompts[0]
    );
    assert!(
        !prompts[1].contains("notes (kept by the harness from your previous reasoning)"),
        "step 2's prompt must carry no notes header:\n{}",
        prompts[1]
    );
    // Step 3's prompt carries step 1's notes (the first call, so "notes
    // from step 1" -- `SequentialMaintainer`'s own call counter, not the
    // harness step).
    assert!(
        prompts[2]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 1)"),
        "step 3's prompt must carry the ` (as of step 1)` heading:\n{}",
        prompts[2]
    );
    assert!(
        prompts[2].contains("notes from step 1"),
        "step 3's prompt must carry the first maintainer call's notes text:\n{}",
        prompts[2]
    );
    // Step 4's prompt carries step 2's notes (the second call).
    assert!(
        prompts[3]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 2)"),
        "step 4's prompt must carry the ` (as of step 2)` heading:\n{}",
        prompts[3]
    );
    assert!(
        prompts[3].contains("notes from step 2"),
        "step 4's prompt must carry the second maintainer call's notes text:\n{}",
        prompts[3]
    );
    // Step 5's prompt carries the third maintainer call's notes, committed
    // as of harness step 4 (`notes_steps[3]` above).
    assert!(
        prompts[4]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 4)"),
        "step 5's prompt must carry the ` (as of step 4)` heading:\n{}",
        prompts[4]
    );
    assert!(
        prompts[4].contains("notes from step 3"),
        "step 5's prompt must carry the third maintainer call's notes text:\n{}",
        prompts[4]
    );

    // The session log's `maintainer` events appear in step order with the
    // same payload shape as today (`notes_tokens`/`next_set`/
    // `reasoning_chars`/`observation_chars`).
    let entries = log.entries(None, 100);
    let maintainer_events: Vec<&statie_core::LogEntry> = entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .collect();
    assert_eq!(
        maintainer_events.len(),
        4,
        "four maintainer calls (spawned for turns 1-4) were drained across \
         turns 2-5's own step() calls; turn 5's own spawned call is never \
         drained within this five-step run (there is no turn 6 to drain it)"
    );
    for event in &maintainer_events {
        let payload = &event.payload;
        assert!(
            payload
                .get("notes_tokens")
                .and_then(Value::as_u64)
                .is_some()
        );
        assert!(payload.get("next_set").and_then(Value::as_bool).is_some());
        assert!(
            payload
                .get("reasoning_chars")
                .and_then(Value::as_u64)
                .is_some()
        );
        assert!(
            payload
                .get("observation_chars")
                .and_then(Value::as_u64)
                .is_some()
        );
    }
    // Step order: the events' own `step` field (the harness step the drain
    // committed at) is strictly increasing.
    let steps: Vec<u64> = maintainer_events.iter().map(|e| e.step).collect();
    assert!(
        steps.windows(2).all(|w| w[0] < w[1]),
        "maintainer events must appear in step order: {steps:?}"
    );
}

// ---------------------------------------------------------------------
// Unit 48 (D24: per-model behaviour is a config choice, not a code
// default): with `Session::with_maintainer_overlap(false)`, the maintainer
// call spawned for a step is drained immediately -- same spawn, same drain,
// same log entries, same `notes_step` -- so the NEXT step's prompt already
// carries THIS step's notes (the synchronous behaviour of builds through
// 26), unlike the overlapped default where it's one step stale.
// ---------------------------------------------------------------------

#[tokio::test]
async fn overlap_off_next_prompt_carries_immediately_prior_step_notes() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let turns: Vec<String> = (1..=3)
        .map(|v| {
            serde_json::json!({
                "patch": {"value": v},
                "action": bump_tool_call(),
                "ack_inbox": []
            })
            .to_string()
        })
        .collect();
    let model = RecordingModel::new(turns);

    let maintainer = SequentialMaintainer::new();

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn Maintainer>)
    .with_maintainer_overlap(false);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let mut notes_steps: Vec<Option<u64>> = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
        let (state, _version) = active.session().state().await.unwrap();
        notes_steps.push(state.harness.notes_step);
    }

    // Step 1's own step() call spawns AND drains its own maintainer call
    // before returning, so `notes_step` is already set right after step 1
    // commits -- not `None`, the way the overlapped test sees it. The
    // drain's own `apply_trusted_patch` commit advances `HarnessState.step`
    // again (same accounting the overlapped five-step test documents), so
    // with every turn costing two harness steps (its own worker commit,
    // then its own immediately-drained maintainer commit), turn N's own
    // worker commit lands at harness step 2N-1 -- and `notes_step` names
    // THAT step, since it's the one just drained.
    assert_eq!(
        notes_steps[0],
        Some(1),
        "step 1: overlap off drains its own spawned call before step() returns, so notes_step is 1 immediately"
    );
    assert_eq!(
        notes_steps[1],
        Some(3),
        "step 2: its own maintainer call (spawned for its own worker commit, harness step 3) is drained synchronously too"
    );
    assert_eq!(
        notes_steps[2],
        Some(5),
        "step 3: same synchronous spawn-then-drain, worker commit at harness step 5"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    // Step 1's prompt carries no notes header (nothing committed yet when it
    // was built).
    assert!(
        !prompts[0].contains("notes (kept by the harness from your previous reasoning)"),
        "step 1's prompt must carry no notes header:\n{}",
        prompts[0]
    );
    // Step 2's prompt already carries step 1's notes (exact text), not one
    // step stale the way the overlapped default would leave it. The
    // heading names `notes_step` (harness step 1, `notes_steps[0]` above),
    // not the turn number.
    assert!(
        prompts[1]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 1)"),
        "step 2's prompt must carry the ` (as of step 1)` heading:\n{}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("notes from step 1"),
        "step 2's prompt must carry step 1's maintainer call's exact notes text:\n{}",
        prompts[1]
    );
    // Step 3's prompt carries step 2's notes, immediately (not step 1's, the
    // way an overlapped run's step 3 would still be showing). The heading
    // names `notes_step` (harness step 3, `notes_steps[1]` above).
    assert!(
        prompts[2]
            .contains("notes (kept by the harness from your previous reasoning) (as of step 3)"),
        "step 3's prompt must carry the ` (as of step 3)` heading:\n{}",
        prompts[2]
    );
    assert!(
        prompts[2].contains("notes from step 2"),
        "step 3's prompt must carry step 2's maintainer call's exact notes text:\n{}",
        prompts[2]
    );

    // The session log's `maintainer` events: one per step (all three are
    // drained within this run, unlike the overlapped five-step test where
    // the last spawn is never drained).
    let entries = log.entries(None, 100);
    let maintainer_events: Vec<&statie_core::LogEntry> = entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer"))
        .collect();
    assert_eq!(
        maintainer_events.len(),
        3,
        "all three maintainer calls are drained synchronously with overlap off"
    );
    // The event's own `step` field is the DRAIN commit's step (`applied.
    // step` in `drain_pending_maintainer`, unchanged from the overlapped
    // path) -- one past the worker commit `notes_step` names, since the
    // notes patch itself advances the harness step again.
    let steps: Vec<u64> = maintainer_events.iter().map(|e| e.step).collect();
    assert_eq!(
        steps,
        vec![2, 4, 6],
        "maintainer log events land at the drain's own harness step (2, 4, 6), in order"
    );
}

// ---------------------------------------------------------------------
// Unit 43 spec item 4: a maintainer task in flight is always awaited
// (bounded by the maintainer's own timeout) before any terminal outcome is
// returned. `SleepingMaintainer` sleeps on its LAST scripted call; the
// session's final `Done` outcome is only returned after that call's notes
// are committed.
// ---------------------------------------------------------------------

struct SleepingMaintainer {
    notes: Mutex<Vec<String>>,
}

impl SleepingMaintainer {
    fn new(notes: Vec<String>) -> Arc<SleepingMaintainer> {
        Arc::new(SleepingMaintainer {
            notes: Mutex::new(notes),
        })
    }
}

#[async_trait]
impl Maintainer for SleepingMaintainer {
    async fn maintain(
        &self,
        _input: statie_core::MaintainInput<'_>,
    ) -> Result<MaintainOutput, MaintainError> {
        let (text, is_last) = {
            let mut notes = self.notes.lock().unwrap();
            let is_last = notes.len() == 1;
            (notes.remove(0), is_last)
        };
        if is_last {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok(MaintainOutput {
            notes: text,
            next: None,
        })
    }
}

#[tokio::test]
async fn done_outcome_waits_for_the_in_flight_maintainer_to_commit() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
        serde_json::json!({
            "patch": {"done": true},
            "action": {"type": "done"},
            "ack_inbox": []
        })
        .to_string(),
    ]);

    // Step 1's own call returns immediately; step 2's park/`Done` handling
    // must await THIS call (spawned right after step 2 commits, since
    // `Done` still reaches the context-maintenance gate's `else` branch --
    // no, `Done` is not maintainer-eligible, so nothing is spawned for step
    // 2 itself; what's in flight when `Done` is returned is step 1's own
    // call, which the drain at the START of the `Done`-producing step must
    // still catch). Only one entry is scripted for step 1; step 2 spawns no
    // call of its own (its outcome is `Done`), so the sleeping entry here
    // is step 1's.
    let maintainer = SleepingMaintainer::new(vec!["notes from step 1".to_string()]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 400),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec2).unwrap();
    assert!(matches!(outcome, Outcome::Done));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.notes, "notes from step 1",
        "the sleeping maintainer's notes must be committed before Done is returned"
    );
    assert_eq!(state.harness.notes_step, Some(1));
}
