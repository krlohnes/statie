//! Item 29: a rejected tool call names the skill that has the tool, and
//! repeats count.
//!
//! `core/src/session.rs`'s inactive-tool rejection (the `tool_rejected`
//! branch inside `Session::execute`'s `Action::Tool` arm) now produces one
//! of three exact observation texts, depending on `Skill::skill_with_tool`
//! (`core/src/skill.rs`): a skill in the caller's `allowed_transitions()`
//! that has the tool (the hand-off text), an empty `name` (the "put the
//! tool's name in `name`" text), or neither (the plain "not available"
//! text). A rejected call also joins the SAME identical-tool-error streak a
//! real `tool_error` does (`HarnessState.last_tool_error`,
//! `last_tool_error_key`/`tool_error_repeat_count`, `error_shape`), parking
//! at `CONTINUE_REPEAT_CAP` through `park_non_progress_cap` exactly as unit
//! 22/25's own tests prove for a real tool error.
//!
//! Core cannot depend on `statie-skills`, so this test builds its own
//! minimal `StateStore`, `Model`, and `Skill` stub -- fixture style copied
//! from `core/tests/it/non_progress.rs`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, ApplyFn, Budgets, Capabilities, CommittedState, Event, HarnessState, LogKind,
    LoggedPatch, Model, ModelError, NullSink, Outcome, Park, PatchError, PatchStrategy, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, SkillSpec,
    StateStore, StepSink, StoreError, ToolSet, Version,
};

// ---------------------------------------------------------------------
// A minimal in-memory `StateStore<W>` (copied from `non_progress.rs`).
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
// (copied from `non_progress.rs`).
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
// Test workspace + a JSON-merge-patch strategy (copied from
// `non_progress.rs`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct Workspace {
    #[serde(default)]
    value: i64,
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

/// A skill standing in for `plan`: has `["read_file"]` active, and
/// `skill_with_tool("edit_file") == Some("implement")` (mirroring
/// `coding_skill_with_tool`), but only when `implement` is one of its
/// `allowed_transitions()` -- `NO_HANDOFF` below has an empty transitions
/// list, to exercise the "unknown name, no skill has it" fallback even for
/// a name `skill_with_tool` COULD map, proving the "only skills in the
/// caller's `allowed_transitions()` are named" rule (item 29's spec).
struct RejectSkill {
    transitions: Vec<String>,
}

impl Skill for RejectSkill {
    type Workspace = Workspace;
    type Patch = MergePatch;

    fn name(&self) -> &str {
        "reject-test"
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::new(SkillSpec::new(
            "reject-test",
            "a skill for the tool_rejected tests",
        ))
    }

    fn initial_workspace(&self) -> Workspace {
        Workspace::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["read_file".to_string()]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        self.transitions.clone()
    }

    fn is_terminal(&self, s: &Workspace) -> bool {
        s.done
    }

    fn skill_with_tool(&self, tool: &str) -> Option<String> {
        if tool.is_empty() {
            return None;
        }
        if tool != "edit_file" {
            return None;
        }
        self.allowed_transitions()
            .iter()
            .any(|t| t == "implement")
            .then(|| "implement".to_string())
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 20_000,
            observation: 2000,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn edit_action(path: &str) -> Value {
    serde_json::json!({"type": "tool", "name": "edit_file", "args": {"path": path}})
}

fn empty_name_action() -> Value {
    serde_json::json!({"type": "tool", "name": ""})
}

fn unknown_action(name: &str) -> Value {
    serde_json::json!({"type": "tool", "name": name, "args": {}})
}

fn step_output(patch: Value, action: Value) -> Value {
    serde_json::json!({"patch": patch, "action": action, "ack_inbox": []})
}

fn tool_rejected_payload(rec: &statie_core::StepRecord) -> Option<Value> {
    rec.entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected")
        })
        .map(|e| e.payload.clone())
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

// =======================================================================
// (a) `skill_with_tool` returns `Some("implement")`: the exact hand-off
// observation text, and the log entry's `skill_with_tool` field.
// =======================================================================

#[tokio::test]
async fn rejected_call_naming_a_skill_with_the_tool_gets_the_handoff_text() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), edit_action("a.txt")),
        // A second turn only exists so `prompts()[1]` -- the prompt built
        // from THIS step's observation -- can be read back and checked for
        // the exact text verbatim, not just the logged `reason` field.
        step_output(serde_json::json!({}), edit_action("a.txt")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(RejectSkill {
        transitions: vec!["implement".to_string()],
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let payload = tool_rejected_payload(&rec).expect("expected a tool_rejected entry");
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    assert_eq!(
        payload.get("name").and_then(Value::as_str),
        Some("edit_file")
    );
    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        Some("implement"),
        "the tool_rejected entry must carry skill_with_tool: got {payload:?}"
    );
    let expected = "tool `edit_file` is not available in skill `reject-test`; `implement` has \
                    it. Finish this skill's job, then hand off: {\"patch\": {\"next_skill\": \
                    \"implement\"}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": \
                    {}}, \"ack_inbox\": []}";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected),
        "logged reason must equal the exact hand-off observation text"
    );

    // Run a second step (the streak's second identical rejection, which
    // also adds the "[harness] This is the same ... error" prefix ahead of
    // it) and confirm the exact observation text was actually delivered to
    // the model, not just logged.
    let _rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let prompts = model.prompts();
    assert!(prompts.len() >= 2, "expected at least 2 prompts");
    assert!(
        prompts[1].contains(expected),
        "the prompt built from step 0's observation must contain the exact hand-off text \
         verbatim:\n{}",
        prompts[1]
    );
}

// =======================================================================
// (b) Empty `name`: the exact "action.name is empty" text.
// =======================================================================

#[tokio::test]
async fn empty_name_rejected_call_gets_the_empty_name_text() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        empty_name_action(),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(RejectSkill {
        transitions: vec!["implement".to_string()],
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let payload = tool_rejected_payload(&rec).expect("expected a tool_rejected entry");
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    assert_eq!(payload.get("name").and_then(Value::as_str), Some(""));
    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        None,
        "an empty name must never carry a skill_with_tool: got {payload:?}"
    );
    assert!(
        payload.get("skill_with_tool").map(Value::is_null) == Some(true),
        "skill_with_tool must be logged as JSON null, not omitted: got {payload:?}"
    );
    let expected = "action.name is empty: put the tool's name in `name` and its arguments in \
                    `args`; available tools: [\"read_file\"]";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected),
        "logged reason must equal the exact empty-name observation text"
    );
}

// =======================================================================
// (c) Unknown name, no skill has it: the exact "no tool by that name" text.
// =======================================================================

#[tokio::test]
async fn unknown_name_rejected_call_gets_the_unknown_name_text() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        unknown_action("frobnicate"),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(RejectSkill {
        transitions: vec!["implement".to_string()],
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let payload = tool_rejected_payload(&rec).expect("expected a tool_rejected entry");
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    assert_eq!(
        payload.get("name").and_then(Value::as_str),
        Some("frobnicate")
    );
    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        None,
        "an unknown name with no skill that has it must not carry a skill_with_tool: \
         got {payload:?}"
    );
    let expected = "tool `frobnicate` is not available in skill `reject-test`; available \
                    tools: [\"read_file\"]. There is no tool by that name; if `frobnicate` is a \
                    path, it belongs in `args`.";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected),
        "logged reason must equal the exact unknown-name observation text"
    );
}

/// Same as (c), but `RejectSkill`'s `skill_with_tool` COULD map `edit_file`
/// to `implement` -- except `implement` is not in this skill's own
/// `allowed_transitions()` (empty here), so `skill_with_tool` must return
/// `None` and the rejection falls back to the plain "not available" text,
/// proving "only skills in the caller's `allowed_transitions()` are named".
#[tokio::test]
async fn skill_not_in_allowed_transitions_is_not_named() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![step_output(
        serde_json::json!({}),
        edit_action("a.txt"),
    )]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    // No transitions at all: `implement` is not reachable from here.
    let skill = Arc::new(RejectSkill {
        transitions: vec![],
    });
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let payload = tool_rejected_payload(&rec).expect("expected a tool_rejected entry");
    let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        None,
        "implement is not in allowed_transitions, so it must not be named: got {payload:?}"
    );
    let expected = "tool `edit_file` is not available in skill `reject-test`; available tools: \
                    [\"read_file\"]. There is no tool by that name; if `edit_file` is a path, \
                    it belongs in `args`.";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected)
    );
}

// =======================================================================
// (d) Three identical rejected calls park with non_progress_cap and the
// shape string; a different-args rejected call in between does not reset
// the count -- mirroring `non_progress.rs`'s own
// `three_consecutive_identical_tool_errors_park_at_the_third`/
// `three_edit_file_errors_with_the_same_shape_but_different_line_numbers_park_at_the_third`.
// =======================================================================

/// Three consecutive rejected calls to the same unknown tool name park at
/// the third, exactly as three identical `tool_error`s do: ordinary
/// `Continue` at steps 0/1, `non_progress_cap` with shape
/// "repeated tool error: {name}" at step 2, and `HarnessState.last_tool_
/// error.count` advancing 1, 2, 3.
#[tokio::test]
async fn three_identical_rejected_calls_park_at_the_third() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), unknown_action("frobnicate")),
        step_output(serde_json::json!({}), unknown_action("frobnicate")),
        step_output(serde_json::json!({}), unknown_action("frobnicate")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(RejectSkill {
        transitions: vec!["implement".to_string()],
    });
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    let mut cap_shapes: Vec<Option<String>> = Vec::new();
    let mut counts = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        cap_shapes.push(non_progress_cap_shapes(&rec));
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push(outcome);

        let (state, _v) = active.session().state().await.unwrap();
        let err = state
            .harness
            .last_tool_error
            .clone()
            .expect("each rejected call must set harness.last_tool_error");
        counts.push(err.count);
    }

    assert_eq!(
        counts,
        vec![1, 2, 3],
        "three identical rejected calls must advance the SAME streak, exactly as a real \
         tool_error does"
    );

    for (i, outcome) in outcomes.iter().enumerate().take(2) {
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}; the first two identical rejections must not park"
        );
    }
    assert!(cap_shapes[0].is_none());
    assert!(cap_shapes[1].is_none());

    let shape = cap_shapes[2]
        .as_ref()
        .expect("step 2 should log a non_progress_cap System entry with a shapes string");
    assert_eq!(shape, "repeated tool error: frobnicate");
    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("3 steps in a row without progress"),
                "park message should say 3 steps in a row without progress: {msg}"
            );
            assert!(
                msg.contains("repeated tool error: frobnicate"),
                "park message should contain the shape: {msg}"
            );
        }
        other => panic!("step 2: expected Parked(AskUser(_)), got {other:?}"),
    }
}

/// A different-args rejected call in between does not reset the count: the
/// streak is keyed on `(tool, error_shape(reason))`, and the reason text for
/// an unknown-name rejection never varies with `args` (it only names the
/// tool and the active list), so a second call to the SAME unknown tool
/// name with DIFFERENT args is still the SAME shape and the streak keeps
/// advancing -- mirroring `error_shape`'s own "same shape, different
/// wording" semantics (`non_progress.rs`'s
/// `three_edit_file_errors_with_the_same_shape_but_different_line_numbers_park_at_the_third`).
#[tokio::test]
async fn different_args_rejected_call_in_between_does_not_reset_the_count() {
    let root = tempfile::tempdir().unwrap();
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());

    let model = ScriptedModel::new(vec![
        step_output(serde_json::json!({}), unknown_action("frobnicate")),
        step_output(
            serde_json::json!({}),
            serde_json::json!({
                "type": "tool",
                "name": "frobnicate",
                "args": {"different": "args-do-not-matter-to-the-shape"},
            }),
        ),
        step_output(serde_json::json!({}), unknown_action("frobnicate")),
    ]);

    let mut session = Session::new(
        Box::new(TestStore::<Workspace>::new()),
        Arc::clone(&log),
        Arc::clone(&model) as Arc<dyn Model>,
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        session_config(&root),
    );

    let skill = Arc::new(RejectSkill {
        transitions: vec!["implement".to_string()],
    });
    let mut active = session.activate(skill).await.unwrap();

    let mut outcomes = Vec::new();
    let mut counts = Vec::new();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = SessionLogExt::append_record(log.as_ref(), rec).unwrap();
        outcomes.push(outcome);

        let (state, _v) = active.session().state().await.unwrap();
        let err = state
            .harness
            .last_tool_error
            .clone()
            .expect("each rejected call must set harness.last_tool_error");
        counts.push(err.count);
    }

    assert_eq!(
        counts,
        vec![1, 2, 3],
        "a different-args rejected call to the SAME unknown tool must not reset the streak, \
         since the identity is (tool, error_shape(reason)), and the reason never varies with \
         args for an unknown-name rejection"
    );

    assert!(
        matches!(outcomes[0], Outcome::Continue),
        "{:?}",
        outcomes[0]
    );
    assert!(
        matches!(outcomes[1], Outcome::Continue),
        "{:?}",
        outcomes[1]
    );
    match &outcomes[2] {
        Outcome::Parked(Park::AskUser(_)) => {}
        other => panic!("step 2: expected Parked(AskUser(_)), got {other:?}"),
    }
}
