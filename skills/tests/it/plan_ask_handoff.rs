//! Item 30: a question asked after the plan is written is the hand-off.
//!
//! `PlanSkill::validate` (`skills/src/coding/mod.rs`), wired in right after
//! `ask_user_empty_note`'s handling and before the `is_done` block: when the
//! model's action is `ask_user` with a real question (the empty/statement
//! cases already converted anything else) and the merged plan -- the patch
//! applied to `cur.agent`, via the new `merged_plan_nonempty` helper the
//! `is_done` block now also uses -- is non-empty, the step is converted
//! straight to the plan-is-written hand-off instead of parking: `next_skill`
//! is set to `"implement"` in the raw patch when the patch names none, the
//! action becomes `Action::Done`, and `PLAN_ASK_HANDOFF_NOTE_PREFIX` plus the
//! first 200 characters of the question (chars, not bytes; `…` if truncated)
//! is attached as the step's note.
//!
//! An `ask_user` with an EMPTY merged plan is untouched: it still parks as
//! `Park::AskUser`.
//!
//! Fixture style copied from `skills/tests/it/bare_done.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `bare_done.rs`/
//! `plan_done.rs` inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, PLAN_ASK_HANDOFF_NOTE_PREFIX, PlanSkill, StepStatus};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `bare_done.rs`'s `config`.
fn config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

/// Same shape as `bare_done.rs`'s `ArcModel`.
struct ArcModel(Arc<MockModel>);

#[async_trait]
impl statie_core::Model for ArcModel {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.0.step(prompt, schema, sink, cancel).await
    }
}

/// Same shape as `bare_done.rs`'s `make_file_tools_session`.
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tools = default_tools(&ToolConfig::default());
    let session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// The `text` of every System entry with `event: "note"`.
fn note_texts(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// A turn that sends action `ask_user` with the given patch and message.
fn ask_user_turn(patch: Value, message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

/// The four-step plan used by cases (a) and (b).
fn four_step_plan() -> Value {
    json!([
        {"id": 1, "text": "first change", "status": "done"},
        {"id": 2, "text": "second change", "status": "done"},
        {"id": 3, "text": "third change", "status": "done"},
        {"id": 4, "text": "run test", "status": "pending"},
    ])
}

// ---------------------------------------------------------------------
// (a) plan active, patch carries a 4-step plan plus ask_user "Should I
// hand off to the implement skill now?": the step's outcome is the
// hand-off; the committed state has next_skill == Some("implement") and
// the 4-step plan; the step's notes contain the exact note text with the
// question.
// ---------------------------------------------------------------------

#[tokio::test]
async fn ask_user_with_written_plan_hands_off_to_implement() {
    let root = tempfile::tempdir().unwrap();

    let question = "Should I hand off to the implement skill now?";
    let turns = vec![ask_user_turn(json!({"plan": four_step_plan()}), question)];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; a written plan must convert ask_user into the hand-off"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
    assert_eq!(
        state.agent.plan.len(),
        4,
        "the 4-step plan must have landed"
    );
    assert_eq!(
        state
            .agent
            .plan
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .count(),
        3
    );
    assert_eq!(state.agent.plan[3].status, StepStatus::Pending);

    let expected_note = format!("{PLAN_ASK_HANDOFF_NOTE_PREFIX}{question}");
    let notes = note_texts(&log);
    assert!(
        notes.iter().any(|n| n == &expected_note),
        "expected the exact hand-off note in the log: {notes:?}"
    );
}

// ---------------------------------------------------------------------
// (b) same, but the patch already carries an explicit next_skill:
// next_skill stays whatever the patch named -- `PlanSkill::
// allowed_transitions` is `["implement", "router"]` (`plan` cannot hand off
// to `review` directly), so this uses `"router"` as the already-named
// target.
// ---------------------------------------------------------------------

#[tokio::test]
async fn ask_user_with_written_plan_and_explicit_next_skill_keeps_it() {
    let root = tempfile::tempdir().unwrap();

    let question = "Should I hand off to the implement skill now?";
    let turns = vec![ask_user_turn(
        json!({"plan": four_step_plan(), "next_skill": "router"}),
        question,
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "router"),
        "got {outcome:?}; an explicit next_skill in the patch must be kept, not overwritten"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill.as_deref(),
        Some("router"),
        "the explicit next_skill must survive unchanged"
    );
    assert_eq!(
        state.agent.plan.len(),
        4,
        "the 4-step plan must have landed"
    );

    let expected_note = format!("{PLAN_ASK_HANDOFF_NOTE_PREFIX}{question}");
    let notes = note_texts(&log);
    assert!(
        notes.iter().any(|n| n == &expected_note),
        "expected the exact hand-off note in the log: {notes:?}"
    );
}

// ---------------------------------------------------------------------
// (c) empty plan in state and no plan in the patch, ask_user "Which
// file?": parks as Park::AskUser with that message, unchanged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn ask_user_with_empty_plan_still_parks() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn(json!({}), "Which file?")];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(
            outcome,
            Outcome::Parked(Park::AskUser(ref m)) if m == "Which file?"
        ),
        "got {outcome:?}; an empty merged plan must leave ask_user parking unchanged"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.next_skill, None, "no handoff must have landed");
    assert!(state.agent.plan.is_empty(), "the plan must still be empty");
}

// ---------------------------------------------------------------------
// (d) a question of 250 characters is quoted as its first 200 characters
// plus "...".
// ---------------------------------------------------------------------

#[tokio::test]
async fn long_question_is_truncated_to_200_chars_with_ellipsis() {
    let root = tempfile::tempdir().unwrap();

    let question: String = format!("{}?", "a".repeat(249));
    assert_eq!(question.chars().count(), 250);

    let turns = vec![ask_user_turn(json!({"plan": four_step_plan()}), &question)];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let truncated: String = "a".repeat(200);
    let expected_note = format!("{PLAN_ASK_HANDOFF_NOTE_PREFIX}{truncated}\u{2026}");
    let notes = note_texts(&log);
    assert!(
        notes.iter().any(|n| n == &expected_note),
        "expected the note truncated to 200 chars plus an ellipsis: {notes:?}"
    );
    assert!(
        notes.iter().all(|n| !n.contains(&question)),
        "the full 250-char question must not appear untruncated: {notes:?}"
    );
}
