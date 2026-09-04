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
    Outcome, Park, PatchError, PatchStrategy, RawOutput, SandboxRoot, Schema, Session,
    SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec, StateStore, StepSink, StoreError,
    ToolSet, Version,
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

    let model = ReasoningModel::new(vec![(
        "decided to bump the counter because X".to_string(),
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    )]);

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
    let model = ReasoningModel::new(vec![(
        reasoning_text.to_string(),
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    )]);

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

    let calls = maintainer.calls();
    let call = &calls[0];
    let expected_observation_chars = call.observation.len();

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
// 2. The maintainer's notes appear in the next prompt under the "notes
//    (kept by the harness ...)" header.
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

    // Second step: the prompt the model now receives should carry the
    // maintainer's notes from the previous step.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second_prompt = &prompts[1];
    assert!(
        second_prompt.contains("notes (kept by the harness from your previous reasoning)"),
        "expected a notes header in the second prompt:\n{second_prompt}"
    );
    assert!(
        second_prompt.contains("kept: the edit target was line 42"),
        "expected the maintainer's notes text in the second prompt:\n{second_prompt}"
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
// 4. Over-budget notes: `maintainer_error` logged with "over budget",
//    previous notes kept.
// ---------------------------------------------------------------------

#[tokio::test]
async fn over_budget_notes_logs_maintainer_error_and_keeps_previous_notes() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {"value": 1},
            "action": bump_tool_call(),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    // Notes budget of 1 token: any nonempty text overflows it.
    let maintainer = Arc::new(MockMaintainer::new(vec![Ok(MaintainOutput {
        notes: "x".repeat(2000),
        next: None,
    })]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root, 1),
    )
    .with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);

    let skill = Arc::new(MaintainedSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.notes, "", "previous (empty) notes must be kept");

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
        reason.contains("over budget"),
        "reason should mention over budget: {reason}"
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("maintainer")),
        "no successful maintainer entry should be logged"
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
