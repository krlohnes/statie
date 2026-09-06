//! Integration tests for the `think` tool (statie-spec.md's `think` tool):
//! `Session::with_thinker`, the session's interception of `Action::Tool {
//! name: "think", .. }` before dispatch to the tool registry, and the
//! harness-owned `HarnessState::last_think_step` field. Core cannot depend
//! on `statie-skills`/`statie-store`/`statie-models`, so this test builds
//! its own minimal `StateStore`, `Model`, and `Skill` stubs, copied in
//! spirit from `core/tests/maintainer.rs`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::MockThinker;
use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, Model, ModelError, NullSink, Outcome, Patch, PatchError, PatchStrategy,
    RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill,
    SkillSpec, StateStore, StepSink, StoreError, ThinkError, ThinkOutput, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/maintainer.rs`;
// core has no test-only shared crate).
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
// A scripted `Model` that records the prompt text it was given, so tests
// can assert on the rendered prompt the `think` call receives and on what
// the NEXT prompt looks like.
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

// ---------------------------------------------------------------------
// Test workspace: `value: i64`, `notes: String` (harness-owned), `next:
// Option<Commitment>`. A real JSON-merge-patch strategy over top-level
// keys (reimplemented here since core cannot depend on `statie-patch`).
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
    #[serde(default)]
    decision_seen: bool,
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

/// A skill with `notes` harness-owned, `next` as the commitment slot, and
/// `think` active alongside `bump`.
struct ThinkingSkill;

impl Skill for ThinkingSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "thinking"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("thinking", "a skill for think tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["bump".to_string(), "think".to_string()]
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

// ---------------------------------------------------------------------
// A recorded `after_tool` call: the exact (name, args, output) the hook was
// invoked with, for (e)/(g) below. Copied in spirit from
// `core/tests/after_tool.rs`'s `HookSkill`, but recording rather than just
// tagging, since these tests need to assert on the call's `output` text
// (not just that a patch landed).
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedAfterToolCall {
    name: String,
    args: Value,
    output: String,
}

/// Same shape as `ThinkingSkill` (`notes` harness-owned, `next` the
/// commitment slot, `think` active alongside `bump`), but with an
/// `after_tool` override that records every call it receives and returns
/// `{"decision_seen": true}` whenever `name == "think"`.
struct RecordingHookSkill {
    calls: Mutex<Vec<RecordedAfterToolCall>>,
}

impl RecordingHookSkill {
    fn new() -> Arc<RecordingHookSkill> {
        Arc::new(RecordingHookSkill {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<RecordedAfterToolCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl Skill for RecordingHookSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "thinking"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "thinking",
            "a skill for think after_tool tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["bump".to_string(), "think".to_string()]
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

    fn after_tool(
        &self,
        _ws: &Workspace,
        _harness: &HarnessState,
        _step: u64,
        name: &str,
        args: &Value,
        output: &str,
    ) -> Option<Patch> {
        self.calls.lock().unwrap().push(RecordedAfterToolCall {
            name: name.to_string(),
            args: args.clone(),
            output: output.to_string(),
        });
        if name == "think" {
            Some(serde_json::json!({"decision_seen": true}))
        } else {
            None
        }
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

fn think_tool_call(question: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "think", "args": {"question": question}})
}

fn yield_turn() -> String {
    serde_json::json!({
        "patch": {},
        "action": {"type": "yield"},
        "ack_inbox": []
    })
    .to_string()
}

async fn harness_state(active: &mut statie_core::Active<'_, ThinkingSkill>) -> HarnessState {
    let (state, _version) = active.session().state().await.unwrap();
    state.harness
}

// ---------------------------------------------------------------------
// (a) thinker configured: the step is Outcome::Continue, the mock
// thinker's recorded call carries the right question/user text, the
// System log has a `think` entry with configured: true and correct
// answer_chars, `last_think_step` is set, and the NEXT prompt contains the
// answer.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_configured_records_call_and_next_prompt_has_answer() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call("why does the test still fail?"),
            "ack_inbox": []
        })
        .to_string(),
        yield_turn(),
    ]);

    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer(
        "the anchor line moved; fix tax.rs line 42",
    ))]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step_number = rec
        .entries()
        .iter()
        .map(|e| e.step)
        .max()
        .unwrap_or_default();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Continue),
        "a think call must never reject the step"
    );

    // The mock thinker's recorded call.
    let calls = thinker.calls();
    assert_eq!(calls.len(), 1, "expected exactly one thinker call");
    assert_eq!(calls[0].question, "why does the test still fail?");
    assert!(
        calls[0].user.contains("\"activation\"") || calls[0].user.contains("thinking"),
        "the thinker's user prompt should be the step's own rendered prompt: {:?}",
        calls[0].user
    );

    // The System log entry.
    let entries = log.entries(None, 100);
    let think_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("think"))
        .expect("expected a think system entry");
    assert_eq!(
        think_event
            .payload
            .get("configured")
            .and_then(Value::as_bool),
        Some(true)
    );
    let expected_answer_chars = "the anchor line moved; fix tax.rs line 42".chars().count() as u64;
    assert_eq!(
        think_event
            .payload
            .get("answer_chars")
            .and_then(Value::as_u64),
        Some(expected_answer_chars)
    );

    // `last_think_step`.
    let harness = harness_state(&mut active).await;
    assert_eq!(harness.last_think_step, Some(step_number));

    // The NEXT prompt contains the answer.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second_prompt = &prompts[1];
    assert!(
        second_prompt.contains("answer:\nthe anchor line moved; fix tax.rs line 42"),
        "expected the answer in the next prompt:\n{second_prompt}"
    );
}

// ---------------------------------------------------------------------
// (b) no thinker configured: still Outcome::Continue, next prompt contains
// "not configured", last_think_step is set, log entry has configured: false.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_unconfigured_reports_not_configured_and_still_commits() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call("is this a race condition?"),
            "ack_inbox": []
        })
        .to_string(),
        yield_turn(),
    ]);

    // No `.with_thinker(...)` call.
    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step_number = rec
        .entries()
        .iter()
        .map(|e| e.step)
        .max()
        .unwrap_or_default();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let entries = log.entries(None, 100);
    let think_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("think"))
        .expect("expected a think system entry");
    assert_eq!(
        think_event
            .payload
            .get("configured")
            .and_then(Value::as_bool),
        Some(false)
    );

    let harness = harness_state(&mut active).await;
    assert_eq!(harness.last_think_step, Some(step_number));

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    let second_prompt = &prompts[1];
    assert!(
        second_prompt.contains("`think` is not configured"),
        "expected the not-configured message in the next prompt:\n{second_prompt}"
    );
}

// ---------------------------------------------------------------------
// (c) thinker returns Err(ThinkError::Timeout): next prompt contains
// "think failed", not a rejection, log entry has error set.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_backend_error_reports_failure_and_still_commits() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call("what changed since the last pass?"),
            "ack_inbox": []
        })
        .to_string(),
        yield_turn(),
    ]);

    let thinker = Arc::new(MockThinker::new(vec![Err(ThinkError::Timeout)]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Continue),
        "a thinker backend error must never reject the step"
    );

    let entries = log.entries(None, 100);
    let think_event = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("think"))
        .expect("expected a think system entry");
    assert!(
        think_event
            .payload
            .get("error")
            .and_then(Value::as_str)
            .is_some(),
        "expected an error field: {think_event:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    let second_prompt = &prompts[1];
    assert!(
        second_prompt.contains("think failed"),
        "expected a think-failed message in the next prompt:\n{second_prompt}"
    );
}

// ---------------------------------------------------------------------
// (d) empty question: the error path of invalid args, still not a
// rejection.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_empty_question_is_invalid_args_not_a_rejection() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call(""),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let thinker = Arc::new(MockThinker::new(vec![]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !rec.entries()
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "an empty question must not reject the step"
    );
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    assert_eq!(
        thinker.calls().len(),
        0,
        "the thinker must not be called for an empty question"
    );
}

// ---------------------------------------------------------------------
// (e) with a thinker configured and the model calling `think`, the skill's
// `after_tool` is invoked once with name == "think", the same args, and
// output equal to "question: <q>\nanswer:\n<answer>"; the patch it returns
// is committed and visible in the next state read.
//
// (f) the session log contains a LogKind::ToolOutput entry for that step
// with payload.name == "think" and payload.output equal to the same text.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_configured_invokes_after_tool_and_logs_tool_output() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let question = "why does the test still fail?";
    let answer = "the anchor line moved; fix tax.rs line 42";

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call(question),
            "ack_inbox": []
        })
        .to_string(),
        yield_turn(),
    ]);

    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer(answer))]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = RecordingHookSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let step_number = rec
        .entries()
        .iter()
        .map(|e| e.step)
        .max()
        .unwrap_or_default();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let expected_output = format!("question: {question}\nanswer:\n{answer}");

    // (e) `after_tool` invoked exactly once, with the right name/args/output,
    // and its returned patch committed.
    let calls = skill.calls();
    assert_eq!(calls.len(), 1, "expected exactly one after_tool call");
    assert_eq!(calls[0].name, "think");
    assert_eq!(calls[0].args, serde_json::json!({"question": question}));
    assert_eq!(calls[0].output, expected_output);

    let (cur_after, _version) = active.session().state().await.unwrap();
    assert!(
        cur_after.agent.decision_seen,
        "the after_tool patch should have landed in the committed workspace"
    );

    // (f) a ToolOutput entry for this step with the same name/output.
    let entries = log.entries(Some(LogKind::ToolOutput), 100);
    let tool_output = entries
        .iter()
        .find(|e| {
            e.step == step_number && e.payload.get("name").and_then(Value::as_str) == Some("think")
        })
        .expect("expected a ToolOutput entry for the think call");
    assert_eq!(
        tool_output.payload.get("output").and_then(Value::as_str),
        Some(expected_output.as_str())
    );
}

// ---------------------------------------------------------------------
// (g) the empty-question path does NOT invoke `after_tool`.
// ---------------------------------------------------------------------

#[tokio::test]
async fn think_empty_question_does_not_invoke_after_tool() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = RecordingModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call(""),
            "ack_inbox": []
        })
        .to_string(),
    ]);

    let thinker = Arc::new(MockThinker::new(vec![]));

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    )
    .with_thinker(Arc::clone(&thinker) as Arc<dyn statie_core::Thinker>);

    let skill = RecordingHookSkill::new();
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    assert_eq!(
        skill.calls().len(),
        0,
        "after_tool must not be invoked for the empty-question invalid-args path"
    );
}
