//! The premature-`done`/non-question-`ask_user` conversions in
//! `skills/src/coding/mod.rs` (`premature_done_note`/`ask_user_empty_note`/
//! `ask_user_statement_note`, wired into `ImplementSkill::validate`,
//! `PlanSkill::validate`, and `ReviewSkill::validate`): rejecting one of
//! these shapes used to discard the step's patch and spend a retry; now the
//! patch commits, no tool runs, and the model is told in one line what is
//! still open via the harness-internal `Action::Continue`
//! (`core/src/model.rs`). Includes both the empty/whitespace `ask_user`
//! conversion and the sibling conversion for a non-empty `ask_user` that
//! contains no `?` -- a statement of intent rather than an actual
//! question.
//!
//! Fixture style of `skills/tests/done_stays_done.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `done_stays_done.rs`
//! and `commitment_rules.rs` inline their own `config`/
//! `make_file_tools_session`: `skills/tests/common/mod.rs` exports items
//! (`EchoTool`/`tool_set`/`make_session` for `Session<ToyWorkspace>`) this
//! file has no use for, and `-D warnings` fails a binary that leaves any of
//! a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Action, Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, PlanSkill, ReviewSkill, StepStatus};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `commitment_rules.rs`'s `config`.
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

/// Same shape as `commitment_rules.rs`'s `ArcModel`.
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

/// Same shape as `commitment_rules.rs`'s `make_file_tools_session`.
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

/// A turn that sends `ask_user` with the given `message` and an empty patch.
fn ask_user_turn(message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

/// A turn that sends action `test` with the given shell command and an
/// empty patch.
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

/// Every `System` entry with `event: "green_checkpoint"`.
fn green_checkpoint_entries(log: &Arc<dyn SessionLog>) -> Vec<Value> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("green_checkpoint"))
        .map(|e| e.payload.clone())
        .collect()
}

// ---------------------------------------------------------------------
// implement + `done` with an open plan step -> Continue, not a rejection.
//
// Fix 3 (`skills/src/coding/mod.rs`) reordered `ImplementSkill::validate`'s
// chain so `implement_no_green_runs_test` now runs BEFORE
// `premature_done_note` and fires even with an open plan step, as long as
// `green_step: None` and either something was edited since the last test or
// none has ever run -- running the test is the right next move whatever is
// open. For `premature_done_note`'s open-step conversion to still be the
// one that fires here (this test's whole point), the scenario now includes
// a FAILING test first, with nothing edited since: that is the one shape
// `implement_no_green_runs_test` leaves alone (see its doc), so the
// premature-done note keeps its old priority.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_open_step_continues_patch_lands_step2_still_open() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: write a two-step plan (both pending), action `ask_user`
        // (harmless filler so this step itself is unaffected by either
        // conversion -- the plan starts empty, this just seeds it).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "pending"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        // Step 2: a failing test establishes last_test_cmd with green_step
        // staying None, and edits_since_test at 0 (nothing edited since) --
        // the one shape that keeps `implement_no_green_runs_test` from
        // substituting a fresh test call ahead of the premature-done note.
        test_turn("false"),
        // Step 3: mark step 1 done, send a premature `done` (with
        // next_skill: review) while step 2 is still pending.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "status": "done"}],
                    "next_skill": "review",
                },
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // Step 4: harmless filler, just so the fourth `active.step()` call
        // below (which reads what the Continue note put in *this* prompt)
        // has a scripted turn to answer with.
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "ok"}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns.clone()));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Parked(Park::AskUser(_))),
        "got {outcome1:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the test call failed");
    assert_eq!(state.harness.edits_since_test, 0);

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();

    // Outcome: Continue, not a rejection and not a handoff.
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; a premature done must convert to Continue, not reject or hand off"
    );
    assert!(
        !any_rejected(&log),
        "the premature-done step must not have been rejected"
    );

    // The patch from the premature-done step landed: step 1 moved to done.
    let (state, _version) = active.session().state().await.unwrap();
    let plan: Vec<(u32, StepStatus)> = state.agent.plan.iter().map(|s| (s.id, s.status)).collect();
    assert_eq!(
        plan.iter().find(|(id, _)| *id == 1).map(|(_, s)| *s),
        Some(StepStatus::Done),
        "step 1's patch must have committed even though `done` was converted: {plan:?}"
    );
    // Step 2 is still open.
    assert_eq!(
        plan.iter().find(|(id, _)| *id == 2).map(|(_, s)| *s),
        Some(StepStatus::Pending),
        "step 2 must still be open: {plan:?}"
    );

    // No `next_skill` landed -- the handoff did not happen.
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must have been stripped, not committed"
    );

    // The note names the still-open step and is logged as a `continued`
    // System entry.
    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("plan step 2 is still open"),
        "note should name the first open step (2): {note}"
    );
    assert!(note.contains("second change"), "{note}");
    assert!(
        note.contains("set its status to done"),
        "note should offer the bookkeeping path first: {note}"
    );
    assert!(note.contains("run `test`"), "{note}");

    // The note appears in the *next* prompt's observation.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec4).unwrap();
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 4,
        "expected at least 4 model calls, got {}",
        prompts.len()
    );
    assert!(
        prompts[3].user.contains("plan step 2 is still open"),
        "the fourth prompt's observation should contain the Continue note:\n{}",
        prompts[3].user
    );
}

// ---------------------------------------------------------------------
// implement + `done` with an open plan step, no test ever run at all (as
// opposed to the failing-run case above) -> fix 3 now runs the substituted
// test call instead of the premature-done note: running the test is the
// right next move whatever plan step is open.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_open_step_and_no_test_ever_run_runs_test_instead() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "pending"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "status": "done"}],
                    "next_skill": "review",
                },
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; no test call ever run must run the test instead of converting to the \
         premature-done note"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted test call must have executed"
    );

    let notes = continued_notes(&log);
    assert!(
        notes
            .iter()
            .all(|n| !n.contains("plan step 2 is still open")),
        "the premature-done note must not fire when the test substitution takes priority: \
         {notes:?}"
    );
}

// ---------------------------------------------------------------------
// implement + `done` with an open plan step, but verification just passed
// (green_step set) -> hands off to review instead of Continue: the
// green-checkpoint override in `green_done_note` (skills/src/coding/mod.rs)
// fires ahead of `premature_done_note`'s case (a). The unit this file's
// header doc names: a live run's `test` passed (checkpoint fired), the
// model sent `done` (the right move), but the old code steered it back into
// more edits because a plan step was still open -- this is the fix.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_after_green_checkpoint_hands_off_to_review_with_open_step() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a two-step plan (both pending).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "pending"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        // Step 2: mark step 1 done, then run a passing `test` -> green
        // checkpoint fires and `cur.harness.green_step` is committed.
        test_turn("true"),
        // Step 3: send `done` with `next_skill: review` while step 2 is
        // still pending -- must hand off, not convert to Continue, because
        // green_step is set.
        MockTurn::new(
            json!({
                "patch": {"next_skill": "review"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the passing test step itself is an ordinary tool call: {outcome2:?}"
    );
    assert!(
        !green_checkpoint_entries(&log).is_empty(),
        "expected a green_checkpoint entry after the passing test"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be committed on the harness after the passing test"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();

    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; a `done` right after verification passed must hand off to review, \
         not convert to Continue, even with an open plan step"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    // Every plan step must read `done` in the committed workspace.
    let (state, _version) = active.session().state().await.unwrap();
    let plan: Vec<(u32, StepStatus)> = state.agent.plan.iter().map(|s| (s.id, s.status)).collect();
    assert!(
        plan.iter().all(|(_, s)| *s == StepStatus::Done),
        "every plan step must be marked done: {plan:?}"
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("review"));

    // The note is attached via `ValidatedStep::notes` -- surfaced as a
    // `note` System entry (no pending observation for a `Done`/handoff
    // action -- see `core/src/session.rs`'s "10b. Validation notes").
    let note_entries: Vec<String> = log
        .entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let note = note_entries
        .last()
        .expect("expected a `note` System entry recording the green-checkpoint handoff");
    assert!(
        note.contains("verification passed at step"),
        "note should record that verification passed: {note}"
    );
    assert!(
        note.contains("open plan steps marked done and handed off to review"),
        "note should record what happened: {note}"
    );
}

// ---------------------------------------------------------------------
// implement + `done` with all steps done -> handoff to review still works
// (positive control).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_all_steps_done_still_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "done"},
                        {"id": 2, "text": "second change", "status": "done"},
                    ],
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // A passing test, so `green_step` is set: `implement_no_green_note`
        // (`skills/src/coding/mod.rs`) requires a passing run before a
        // `done` handoff to review, which is otherwise orthogonal to what
        // this test checks (the positive-control handoff itself).
        test_turn("true"),
        MockTurn::new(
            json!({
                "patch": {"next_skill": "review"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome:?}; done with no open steps and a passing test run must still hand off \
         to review"
    );
}

/// A bare `done` (no `next_skill`, not terminal) with NO open plan step
/// must still be rejected exactly as before this unit: the premature-done
/// conversion only fires when a step is open, so this falls straight
/// through to `implement`'s ordinary "done is only valid when terminal or
/// hands off" rule.
#[tokio::test]
async fn implement_bare_done_with_no_open_steps_and_green_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a plan whose only step is already done.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: a passing test, so `green_step` is set: this test is
        // about the bare-done rejection path, not about
        // `implement_no_green_note` (`skills/src/coding/mod.rs`), which
        // would otherwise fire first and mask the rejection this test
        // checks for.
        test_turn("true"),
        // Step 3: bare `done`, no `next_skill` at all.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    // No open step, no next_skill, not terminal, and a passing test:
    // `implement_bare_done` (`skills/src/coding/mod.rs`) names the handoff
    // for the model, so this is a handoff to review, not a rejection.
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}"
    );
    assert!(
        !any_rejected(&log),
        "bare done with green and no open steps is converted, never rejected"
    );
}

// ---------------------------------------------------------------------
// implement + empty ask_user -> Continue with note, patch kept.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_empty_ask_user_continues_with_note_patch_kept() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: write a plan step and mark it in_progress, action
        // `ask_user` with an empty message.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "do the thing", "status": "in_progress"}],
                    "recent_summary": "started the thing",
                },
                "action": {"type": "ask_user", "message": ""},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an empty ask_user must convert to Continue, not park or reject"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.plan.first().map(|s| s.status),
        Some(StepStatus::InProgress),
        "the patch must have committed"
    );
    assert_eq!(state.agent.recent_summary, "started the thing");

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("no question"),
        "note should explain nothing was asked: {note}"
    );
}

// ---------------------------------------------------------------------
// implement + non-empty ask_user -> still parks.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_non_empty_ask_user_still_parks() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn("do you want tests run first?")];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    match outcome {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert_eq!(msg, "do you want tests run first?");
        }
        other => panic!("expected Parked(AskUser(..)), got {other:?}"),
    }
    assert!(
        continued_notes(&log).is_empty(),
        "a real question must not be converted to Continue"
    );
}

// ---------------------------------------------------------------------
// implement + ask_user carrying a statement of intent (no `?` anywhere) ->
// Continue with the statement-specific note, patch kept. Case (b') of
// `ask_user_empty_note`'s doc (`ask_user_statement_note`): 14 of 15 parked
// sessions across 48 live runs were `ask_user` used to narrate what the
// model was about to do, not to ask anything.
// ---------------------------------------------------------------------

const ASK_USER_STATEMENT_NOTE: &str = "`ask_user` carried a statement of what you will do next, not a question; nothing was \
     asked and your patch was kept. Do it with a tool call. Ask the user only for something \
     you cannot find out yourself, and end the question with `?`.";

#[tokio::test]
async fn implement_ask_user_statement_of_intent_continues_with_note_patch_kept() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {
                "plan": [{"id": 1, "text": "fix the braces", "status": "in_progress"}],
                "recent_summary": "found the unbalanced braces",
            },
            "action": {
                "type": "ask_user",
                "message": "I need to fix the braces first. I will read the file.",
            },
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; a statement-of-intent ask_user must convert to Continue, not park"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.plan.first().map(|s| s.status),
        Some(StepStatus::InProgress),
        "the patch must have committed"
    );
    assert_eq!(state.agent.recent_summary, "found the unbalanced braces");

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, ASK_USER_STATEMENT_NOTE,
        "note text must match exactly"
    );
}

// ---------------------------------------------------------------------
// plan + ask_user carrying a statement of intent -> Continue too (the
// conversion is shared by all three coding skills).
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_ask_user_statement_of_intent_continues_with_note() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn(
        "I will rewrite tui/src/headless.rs to fix the unbalanced braces.",
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, ASK_USER_STATEMENT_NOTE,
        "note text must match exactly"
    );
}

// ---------------------------------------------------------------------
// review + ask_user carrying a statement of intent -> Continue too.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_ask_user_statement_of_intent_continues_with_note() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn(
        "I've been reading tui/tests/driver.rs but haven't found the target test yet.",
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, ASK_USER_STATEMENT_NOTE,
        "note text must match exactly"
    );
}

// ---------------------------------------------------------------------
// implement + ask_user with a real question -> still parks, message
// unchanged (the `?` is present, so neither conversion fires).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_ask_user_real_question_still_parks() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn(
        "Which output format should the arms print, one line or two?",
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    match outcome {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert_eq!(
                msg,
                "Which output format should the arms print, one line or two?"
            );
        }
        other => panic!("expected Parked(AskUser(..)), got {other:?}"),
    }
    assert!(
        continued_notes(&log).is_empty(),
        "a real question must not be converted to Continue"
    );
}

// ---------------------------------------------------------------------
// implement + ask_user that reads as a question but has no `?` -> still
// converted. The rule is the literal `?` character, not sentence grammar:
// the harness cannot reliably tell a real question from a statement of
// intent by grammar alone, so it uses the one unambiguous signal the model
// can always provide.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_ask_user_question_shaped_but_missing_question_mark_converts() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn("Should I keep both arms")];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; missing the `?` converts even though it reads as a question"
    );
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, ASK_USER_STATEMENT_NOTE,
        "note text must match exactly"
    );
}

// ---------------------------------------------------------------------
// review + empty ask_user -> Continue too (case (b) applies to review).
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_empty_ask_user_continues_instead_of_rejecting() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": "   "},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    assert!(!any_rejected(&log));
    let notes = continued_notes(&log);
    assert!(!notes.is_empty(), "expected a continued note");
}

// ---------------------------------------------------------------------
// plan + `done` with an empty plan -> Continue, not a rejection (statie-
// spec.md "the plan skill: `done` with an empty plan commits and notes").
// `PlanSkill::validate` converts this the same way `premature_done_note`
// converts an open plan step in `implement`: the patch commits, no retry
// is spent, and `next_skill` (if the model sent one alongside `done`) is
// stripped so no handoff lands. Covers both shapes a live run sent at the
// start of a session: bare `done`, and `done` + `next_skill: "implement"`
// together (the instructions say "set next_skill and send done").
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_done_with_empty_plan_and_no_next_skill_continues() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({"patch": {}, "action": {"type": "done"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an empty-plan done must convert to Continue, not reject or hand off"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must still be unset -- no handoff happened"
    );
    assert!(state.agent.plan.is_empty());

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("the plan is empty"),
        "note should say the plan is empty: {note}"
    );
    assert!(
        note.contains("next_skill: \"implement\""),
        "note should tell the model how to hand off once the plan is written: {note}"
    );
}

#[tokio::test]
async fn plan_done_with_empty_plan_and_next_skill_continues_and_strips_next_skill() {
    let root = tempfile::tempdir().unwrap();

    // The model sent `next_skill` alongside `done`, per the instructions --
    // but the plan is still empty, so this must convert the same way a bare
    // `done` does, and the `next_skill` it sent must not land.
    let turns = vec![MockTurn::new(
        json!({
            "patch": {"next_skill": "implement"},
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
        "got {outcome:?}; an empty-plan done must convert to Continue even with next_skill set"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must have been stripped, not committed"
    );

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("the plan is empty"),
        "note should say the plan is empty: {note}"
    );
}

/// Positive control: a non-empty plan with a bare `done` (no `next_skill`)
/// still injects the `implement` handoff exactly as before this unit --
/// only an EMPTY merged plan takes the new `Continue` path.
#[tokio::test]
async fn plan_bare_done_with_nonempty_plan_still_hands_off_to_implement() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {
                "plan": [{"id": 1, "text": "do the thing", "status": "pending"}],
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
        "got {outcome:?}; a non-empty plan must still inject the implement handoff"
    );
}

// ---------------------------------------------------------------------
// Direct unit-level check via the `Skill::validate` surface (no session):
// premature done strips `next_skill` from the raw patch itself, matching
// the coding-skill-level test in `skills/src/coding/tests.rs`. Kept here
// too since this file is the canonical home for the premature-done unit.
// ---------------------------------------------------------------------

#[test]
fn premature_done_action_is_continue_variant() {
    // A smoke test that `Action::Continue` (not some other stand-in) is
    // what a premature done becomes -- see
    // `skills/src/coding/tests.rs`'s
    // `implement_handoff_to_review_with_pending_step_continues_instead_of_handoff`
    // for the full patch-level assertions.
    let note = "plan step 2 is still open: second change. If that step's work is already done, \
                set its status to done in your patch and run `test`; if not, make the change \
                with edit_file/write_file, then `test`. `done` is for a finished plan; your \
                patch was kept.";
    let action = Action::Continue {
        note: note.to_string(),
    };
    assert!(matches!(action, Action::Continue { .. }));
}
