//! Unit 11 ("options-and-verdict mode for `think`"): the optional `mode`
//! argument on `think` (`core/src/session.rs`'s `execute_think`). `mode:
//! "options"` prepends `statie_core::OPTIONS_MODE_INSTRUCTION` to the
//! question sent to the thinker; a bare `{question}` (no `mode`) and
//! `mode: "trace"` both send the bare question, matching today's default
//! behaviour. An unknown `mode` is a tool error (never a rejection) whose
//! message names the two valid modes.
//!
//! Session/store/model/skill scaffolding copied from `core/tests/think.rs`
//! (core cannot depend on `statie-skills`/`statie-store`/`statie-models`, so
//! each think-related test file builds its own minimal stubs).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::MockThinker;
use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, Commitment, CommittedState, Event, HarnessState,
    LogKind, LoggedPatch, Model, ModelError, NullSink, OPTIONS_MODE_INSTRUCTION, Outcome,
    PatchError, PatchStrategy, RawOutput, SandboxRoot, Schema, Session, SessionConfig, SessionLog,
    Skill, SkillSpec, StateStore, StepSink, StoreError, ThinkOutput, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `core/tests/think.rs`).
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
// A scripted `Model` that just plays back pre-baked turns (no need to
// record prompts for these tests: `MockThinker::calls()` is the assertion
// surface).
// ---------------------------------------------------------------------

struct ScriptedModel {
    turns: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(turns: Vec<String>) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel {
            turns: Mutex::new(turns),
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
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Backend("script exhausted".to_string()));
        }
        let content = turns.remove(0);
        Ok(RawOutput::new(prompt.version(), None, content))
    }
}

// ---------------------------------------------------------------------
// Test workspace: same shape as `core/tests/think.rs`'s.
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

/// A skill with `notes` harness-owned and `think` active.
struct ThinkingSkill;

impl Skill for ThinkingSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "thinking"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new("thinking", "a skill for think-mode tests"))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["think".to_string()]
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

/// `think` tool call with an explicit `mode`.
fn think_tool_call_with_mode(question: &str, mode: &str) -> Value {
    serde_json::json!({
        "type": "tool",
        "name": "think",
        "args": {"question": question, "mode": mode},
    })
}

/// `think` tool call with no `mode` key at all (today's bare shape).
fn think_tool_call_bare(question: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "think", "args": {"question": question}})
}

fn build_session(
    root: &tempfile::TempDir,
    model: Arc<ScriptedModel>,
    thinker: Arc<MockThinker>,
) -> (Session<Workspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        model as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(root),
    )
    .with_thinker(thinker as Arc<dyn statie_core::Thinker>);
    (session, log)
}

// ---------------------------------------------------------------------
// (a) `mode: "options"` sends `OPTIONS_MODE_INSTRUCTION + question` to the
// thinker.
// ---------------------------------------------------------------------

#[tokio::test]
async fn options_mode_prepends_instruction_to_question() {
    let root = tempfile::tempdir().unwrap();
    let question = "should we use a token bucket or a sliding window?";

    let model = ScriptedModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call_with_mode(question, "options"),
            "ack_inbox": []
        })
        .to_string(),
    ]);
    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer(
        "use a token bucket: verdict in one sentence.",
    ))]));

    let (mut session, log) = build_session(&root, model, Arc::clone(&thinker));
    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let calls = thinker.calls();
    assert_eq!(calls.len(), 1, "expected exactly one thinker call");
    let expected_question = format!("{OPTIONS_MODE_INSTRUCTION}{question}");
    assert_eq!(
        calls[0].question, expected_question,
        "options mode must send OPTIONS_MODE_INSTRUCTION prepended to the raw question"
    );
}

// ---------------------------------------------------------------------
// (b) a bare `{question}` (no `mode` key) sends the bare question, exactly
// as before this unit.
// ---------------------------------------------------------------------

#[tokio::test]
async fn no_mode_key_sends_bare_question() {
    let root = tempfile::tempdir().unwrap();
    let question = "why does the test still fail?";

    let model = ScriptedModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call_bare(question),
            "ack_inbox": []
        })
        .to_string(),
    ]);
    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer("fix it"))]));

    let (mut session, log) = build_session(&root, model, Arc::clone(&thinker));
    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let calls = thinker.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].question, question,
        "no `mode` key must send the bare question, unchanged from before this unit"
    );
}

// ---------------------------------------------------------------------
// (c) `mode: "trace"` explicitly also sends the bare question (same as
// today's default / no `mode` at all).
// ---------------------------------------------------------------------

#[tokio::test]
async fn trace_mode_sends_bare_question() {
    let root = tempfile::tempdir().unwrap();
    let question = "is this a race condition?";

    let model = ScriptedModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call_with_mode(question, "trace"),
            "ack_inbox": []
        })
        .to_string(),
    ]);
    let thinker = Arc::new(MockThinker::new(vec![Ok(ThinkOutput::answer("no"))]));

    let (mut session, log) = build_session(&root, model, Arc::clone(&thinker));
    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    let calls = thinker.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].question, question,
        "explicit mode: \"trace\" must send the bare question, same as no mode at all"
    );
}

// ---------------------------------------------------------------------
// (d) an unknown mode is a tool error (never a rejection): the thinker is
// never called, and the resulting observation names both valid modes.
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_mode_is_a_tool_error_naming_both_modes() {
    let root = tempfile::tempdir().unwrap();
    let question = "which approach?";

    let model = ScriptedModel::new(vec![
        serde_json::json!({
            "patch": {},
            "action": think_tool_call_with_mode(question, "verdict"),
            "ack_inbox": []
        })
        .to_string(),
    ]);
    let thinker = Arc::new(MockThinker::new(vec![]));

    let (mut session, log) = build_session(&root, model, Arc::clone(&thinker));
    let skill = Arc::new(ThinkingSkill);
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !rec.entries()
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "an unknown mode must not reject the step"
    );
    let outcome = statie_core::SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue));

    assert_eq!(
        thinker.calls().len(),
        0,
        "the thinker must not be called for an unknown mode"
    );

    let entries = log.entries(Some(LogKind::System), 100);
    let tool_error = entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .expect("expected a tool_error system entry");
    let reason = tool_error
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .expect("tool_error entry should have a reason");
    assert!(
        reason.contains("\"trace\"") && reason.contains("\"options\""),
        "the tool_error reason must name both valid modes: {reason}"
    );
}
