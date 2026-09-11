//! `plan`'s `done: true`-stripping fix (`skills/src/coding/mod.rs`,
//! `plan_done_not_ours_note`/`done_not_ours_note`): a bench log showed
//! `plan`'s first step after activation send `{"patch": {"done": true},
//! "action": {"type": "done"}}` with an empty plan -- the existing
//! empty-plan conversion in `PlanSkill::validate` replaced the action with
//! `Action::Continue` but kept the patch, so `done: true` committed, the
//! workspace became terminal (`common_is_terminal` is `ws.done`), and the
//! session ended as `done` after three steps with no edit and no test. A
//! false completion.
//!
//! The fix has two independent parts, both exercised here:
//! - `PlanSkill::is_terminal` is now unconditionally `false` -- `plan` can
//!   never end the session, whatever `ws.done` says.
//! - `PlanSkill::validate` now strips `done` from the raw patch whenever the
//!   *merged* workspace would read `done: true`, converting a bare `done`
//!   (no `next_skill`) to `Action::Continue` with a note, or leaving a named
//!   handoff alone (`next_skill` set) with the note attached to the step
//!   instead.
//!
//! Fixture style of `skills/tests/premature_done.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `premature_done.rs`
//! and `done_stays_done.rs` inline their own `config`/
//! `make_file_tools_session`: `skills/tests/common/mod.rs` exports items
//! this file has no use for, and `-D warnings` fails a binary that leaves
//! any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, PlanSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `premature_done.rs`'s `config`.
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

/// Same shape as `premature_done.rs`'s `ArcModel`.
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

/// Same shape as `premature_done.rs`'s `make_file_tools_session`.
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

/// Every `System` entry with `event: "continued"`, most recent last.
fn continued_notes(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("continued"))
        .filter_map(|e| e.payload.get("note").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// A turn with an empty patch and action `ask_user`, used as harmless
/// filler so a following scripted step has a prompt to answer.
fn ask_user_turn(message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------
// The exact live sequence from the bug report: router hands off to plan
// (modeled here by activating `PlanSkill` directly -- a fresh activation is
// exactly what a router handoff produces), and plan's first step sends
// `{"patch": {"done": true}, "action": {"type": "done"}}` with an empty
// plan.
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_first_step_done_true_empty_plan_continues_not_terminal() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: the exact defect shape -- `done: true` in the patch, an
        // empty plan, action `done`.
        MockTurn::new(
            json!({
                "patch": {"done": true},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // Step 2: harmless filler so a following step still has something
        // to answer -- proves the session did not end after step 1.
        ask_user_turn("still going?"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();

    assert!(
        matches!(outcome1, Outcome::Continue),
        "got {outcome1:?}; done:true with an empty plan must convert to Continue, not Done"
    );
    assert!(
        !any_rejected(&log),
        "the empty-plan done:true step must not have been rejected"
    );

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("the plan is empty"),
        "the empty-plan note must win for an empty plan: {note}"
    );

    // The committed workspace must NOT be done, and the session must NOT be
    // terminal -- the whole point of the fix.
    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "done must not have committed as true: {:?}",
        state.agent
    );
    assert!(
        !skill.is_terminal(&state.agent),
        "plan must never be terminal, even if done were somehow true"
    );

    // A following step still runs -- the session did not silently end.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(statie_core::Park::AskUser(_))),
        "got {outcome2:?}; a following step must still run after the false-done step"
    );
}

// ---------------------------------------------------------------------
// A non-empty, valid plan with `done: true` and no `next_skill`: the new
// `plan_done_not_ours_note` conversion, not the old auto-inject-next_skill
// shortcut (that shortcut only applies when the raw patch carries no `done`
// field at all -- see `plan_bare_done_with_nonempty_plan_still_hands_off_
// to_implement` in `premature_done.rs` for that positive control).
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_done_true_with_valid_plan_and_no_next_skill_continues_not_done() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {
                "done": true,
                "plan": [
                    {"id": 1, "text": "first change", "status": "pending"},
                    {"id": 2, "text": "run test", "status": "pending"},
                ],
            },
            "action": {"type": "done"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; done:true with no next_skill must convert to Continue, not auto-handoff"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("not the plan skill's to set"),
        "the new not-ours note must fire, not the empty-plan note: {note}"
    );

    // The plan itself landed (the patch was kept, minus `done`), but `done`
    // stayed false and no handoff was injected.
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.plan.len(), 2, "the plan patch must have landed");
    assert!(!state.agent.done, "done must not have committed as true");
    assert_eq!(
        state.agent.next_skill, None,
        "no next_skill was sent and none must have been injected"
    );
}

// ---------------------------------------------------------------------
// A proper handoff -- `next_skill` explicitly named alongside a valid plan
// and action `done`, no `done` field in the patch at all -- proceeds
// unchanged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_proper_handoff_to_implement_unaffected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {
                "next_skill": "implement",
                "plan": [
                    {"id": 1, "text": "first change", "status": "pending"},
                    {"id": 2, "text": "run test", "status": "pending"},
                ],
            },
            "action": {"type": "done"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; a proper next_skill handoff must proceed unchanged"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the handoff never set done:true in the first place -- must stay false"
    );
}
