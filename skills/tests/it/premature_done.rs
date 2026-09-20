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
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{
    CodingWorkspace, ImplementSkill, OPEN_STEP_NOTE_FIRST, OPEN_STEP_NOTE_REPEAT, PlanSkill,
    REFUSAL_QUESTION, REFUSAL_THINK_NOTE_PREFIX, ReviewSkill, StepStatus,
};
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

/// Unit 47: same shape as `make_file_tools_session`, but with a
/// `MockThinker` attached (`Session::with_thinker`) -- the fixture pattern
/// `skills/tests/it/error_think.rs` uses, needed here to exercise
/// `refusal_thinks`'s substitution end-to-end (a real `think` action
/// driven through `Session::execute_think`, writing `CodingWorkspace::
/// decision` from the scripted answer).
fn make_file_tools_session_with_thinker(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    thinker: Arc<dyn statie_core::Thinker>,
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
    )
    .with_thinker(thinker);
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

/// Finds the most recent `Action` log entry's `name`/`args` for a `Tool`
/// action, or its `type` (`"continue"`, `"done"`, `"ask_user"`, ...) with a
/// null `args` for any other shape. Same as `early_park.rs`'s
/// `last_action` (unlike `error_think.rs`'s narrower version, which only
/// ever sees `Tool` actions in its own fixtures and reads `name` directly).
fn last_action(log: &Arc<dyn SessionLog>) -> (String, Value) {
    let entries = log.entries(Some(LogKind::Action), 10_000);
    let entry = entries.last().expect("an Action entry must exist");
    let action_type = entry
        .payload
        .get("type")
        .and_then(Value::as_str)
        .expect("action entry must have a type")
        .to_string();
    if action_type == "tool" {
        let name = entry
            .payload
            .get("name")
            .and_then(Value::as_str)
            .expect("a tool action entry must have a name")
            .to_string();
        (
            name,
            entry.payload.get("args").cloned().unwrap_or(Value::Null),
        )
    } else {
        (action_type, Value::Null)
    }
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
        note.contains("A step is finished by a tool call, not by marking it done"),
        "note should require a tool call, not just a status change: {note}"
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
// Unit 47 acceptance (b), the no-green Continue fallback:
// `implement_no_green_runs_test`'s fallback note ("the most recent `test`
// run failed ... handing off needs a passing run") is the refusal here --
// no open plan step (so `premature_done_note`'s case (a) never gets a
// look), a failing `test` on record, and nothing edited since. The SECOND
// consecutive `done` in this shape is substituted for `think` with the
// exact REFUSAL_QUESTION filled from that fallback note.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_no_green_continue_fallback_second_refusal_becomes_think() {
    let root = tempfile::tempdir().unwrap();

    let fallback_note = "the most recent `test` run failed, or none has run yet; handing off \
         needs a passing run. Run `test` now; if it fails, fix what it reports before `done`."
        .to_string();

    let turns = vec![
        // Seed a plan with its one step already done -- `has_open_step` is
        // false, so `implement_no_green_runs_test`'s fallback keeps
        // priority (see that function's doc).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "the only change", "status": "done"}]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        // A failing test: green_step stays None, edits_since_test is 0,
        // last_test_cmd is Some -- the exact shape the fallback (not the
        // real-test substitution) needs.
        test_turn("false"),
        // First `done`: the fallback fires, converting to Continue with
        // its own fixed note (first refusal: consecutive_continues was 0).
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
        // Second `done`: unit 47's refusal_thinks substitutes think.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer("the change is already there; run `test` again to confirm before done"),
    )]));
    let (mut session, log) = make_file_tools_session_with_thinker(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: seed the plan.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    // Step 2: the failing test.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None);
    assert_eq!(state.harness.edits_since_test, 0);

    // Step 3: first refusal -> Continue with the fallback note, exact.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let notes3 = continued_notes(&log);
    assert_eq!(
        notes3.last(),
        Some(&fallback_note),
        "step 3 must produce the exact no-green fallback note"
    );
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(state3.harness.consecutive_continues, 1);

    // Step 4: second refusal -> think, with the exact REFUSAL_QUESTION
    // filled from the fallback note, and the note carrying the prefix.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, args4) = last_action(&log);
    assert_eq!(name4, "think", "step 4 must be substituted for think");
    let expected_question = REFUSAL_QUESTION.replace("{note}", &fallback_note);
    assert_eq!(args4, json!({"question": expected_question}));
    assert_eq!(thinker.calls().len(), 1);
    assert_eq!(thinker.calls()[0].question, expected_question);
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.consecutive_continues, 1,
        "the carve-out must leave consecutive_continues unchanged by the think step"
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
// Unit 47 acceptance (b), two empty `ask_user`s: the SECOND consecutive
// empty `ask_user` in a row is substituted for `think` with the exact
// REFUSAL_QUESTION filled from `ask_user_empty_note`'s fixed text.
// ---------------------------------------------------------------------

const EMPTY_ASK_USER_NOTE: &str = "`ask_user` had no question, so nothing was asked and your \
     patch was kept. If you are blocked, put the question in `message`; otherwise keep working \
     with a tool call.";

#[tokio::test]
async fn plan_two_empty_ask_users_second_refusal_becomes_think() {
    // Uses `PlanSkill`, not `ImplementSkill`: under `implement`, the
    // SECOND consecutive non-question `ask_user` in a row (empty or not)
    // is always claimed first by `implement_early_park_thinks` (unit 25,
    // wired earlier in `ImplementSkill::validate` than `refusal_thinks`),
    // which substitutes `think` with its own `EARLY_PARK_QUESTION` before
    // `refusal_thinks` ever gets a look -- exercising this exact scenario
    // (two empty `ask_user`s, `ask_user_empty_note`'s refusal repeating)
    // under `implement` would test `implement_early_park_thinks`, not unit
    // 47's `refusal_thinks`. `plan` has no early-park-equivalent trigger,
    // so its own `ask_user_empty_note` conversion (called directly at the
    // top of `PlanSkill::validate`) is the one `refusal_thinks` actually
    // catches on the second refusal.
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": ""}}).to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": ""}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer("there is nothing left to ask; write the plan instead"),
    )]));
    let (mut session, log) = make_file_tools_session_with_thinker(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
    );

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: first empty ask_user -> Continue with the fixed note.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let notes1 = continued_notes(&log);
    assert_eq!(
        notes1.last(),
        Some(&EMPTY_ASK_USER_NOTE.to_string()),
        "step 1 must produce the exact empty-ask_user note"
    );
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(state1.harness.consecutive_continues, 1);

    // Step 2: second empty ask_user -> think, with the exact
    // REFUSAL_QUESTION filled from the fixed note, and the note carrying
    // the prefix.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (name2, args2) = last_action(&log);
    assert_eq!(name2, "think", "step 2 must be substituted for think");
    let expected_question = REFUSAL_QUESTION.replace("{note}", EMPTY_ASK_USER_NOTE);
    assert_eq!(args2, json!({"question": expected_question}));
    assert_eq!(thinker.calls().len(), 1);
    assert_eq!(thinker.calls()[0].question, expected_question);
    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.harness.consecutive_continues, 1,
        "the carve-out must leave consecutive_continues unchanged by the think step"
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
    let note = OPEN_STEP_NOTE_FIRST
        .replacen("{}", "2", 1)
        .replacen("{}", "second change", 1);
    let action = Action::Continue { note };
    assert!(matches!(action, Action::Continue { .. }));
}

// ---------------------------------------------------------------------
// Acceptance (d) (unit 37, updated by unit 47): a scripted five-step run --
// `done` with an open step (first refusal), `done` again marking that step
// done (unit 47: the SECOND consecutive refusal, substituted for `think`
// instead of the repeat note -- the status change is still dropped, the
// step still stays open, but the committed action is now `think`), `done`
// a third time (once-per-streak blocks a second `think` substitution, so
// this is the ordinary repeat-refusal Continue -- item 37's own
// `OPEN_STEP_NOTE_REPEAT`), `test` (a real tool call) -- shows the fourth
// step landing as an ordinary tool call and `HarnessState::
// consecutive_continues` reading 0 right after it.
// ---------------------------------------------------------------------

const THINKER_ANSWER: &str = "the plan step is already satisfied by the failing test above; run \
     `test` to prove it before sending `done` again";

/// `OPEN_STEP_NOTE_REPEAT` filled the same way `premature_done_note`
/// itself fills it for this test's plan step 1: id `1`, text "the first
/// change", and the `` `test` `` call hint (this test's failing-test setup
/// keeps `edits_since_test == 0` and `green_step: None`, the shape that
/// picks `` `test` `` over the read/edit hint -- see `premature_done_note`'s
/// doc).
fn fill_template_repeat() -> String {
    OPEN_STEP_NOTE_REPEAT
        .replacen("{}", "1", 1)
        .replacen("{}", "the first change", 1)
        .replacen("{}", "`test`", 1)
}

#[tokio::test]
async fn done_then_done_marking_done_then_test_resets_consecutive_continues_to_zero() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Seed a two-step plan (both pending): step 1 is the one this test
        // repeatedly tries to mark done; step 2 is never touched by any
        // patch below, so the plan always has an open step and
        // `premature_done_note`'s top-level gate stays satisfied whether
        // or not step 1's own status change lands.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "the first change", "status": "pending"},
                        {"id": 2, "text": "the second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        // A failing test establishes last_test_cmd with green_step staying
        // None and edits_since_test at 0 -- the shape that keeps
        // `premature_done_note` in priority over the test substitution.
        test_turn("false"),
        // Step 3: `done` with step 1 still open (first refusal:
        // consecutive_continues was 0 after the tool-call step above).
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
        // Step 4: `done` again, this time marking step 1 done in the patch
        // (unit 47: the SECOND consecutive refusal -- `refusal_thinks`
        // substitutes `think` instead of the repeat note; the status
        // change is still dropped from the kept patch, same as before).
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "status": "done"}]},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // Step 5: `done` a THIRD time, again marking step 1 done -- once
        // per streak blocks a second `think` substitution (the previous
        // committed step was itself the substituted `think`), so this
        // falls through to the ordinary repeat-refusal Continue (item 37's
        // `OPEN_STEP_NOTE_REPEAT`), which also drops the status change.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "status": "done"}]},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // Step 6: a real tool call -- breaks the Continue streak.
        test_turn("true"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) = make_file_tools_session_with_thinker(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: seed the plan.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    // Step 2: the failing test.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the test call failed");
    assert_eq!(state.harness.edits_since_test, 0);
    assert_eq!(
        state.harness.consecutive_continues, 0,
        "a real tool call must not be a Continue"
    );

    // Step 3: first refusal -> Continue, consecutive_continues becomes 1.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.consecutive_continues, 1);
    assert_eq!(
        state.agent.plan.first().map(|s| s.status),
        Some(StepStatus::Pending),
        "the step must still be open after the first refusal"
    );
    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    let expected_note =
        OPEN_STEP_NOTE_FIRST
            .replacen("{}", "1", 1)
            .replacen("{}", "the first change", 1);
    assert_eq!(
        note, &expected_note,
        "step 3 must produce the exact first-refusal note"
    );

    // Step 4: SECOND consecutive refusal -> unit 47's refusal_thinks
    // substitutes `think` instead of the repeat note. consecutive_continues
    // stays 1 (unit 47's core carve-out, 3a: a committed `think` step is
    // neutral for it), the status change is still dropped (the step stays
    // open), and the committed action is `think` with the exact
    // REFUSAL_QUESTION text and REFUSAL_THINK_NOTE_PREFIX + the first
    // refusal's own note.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, args4) = last_action(&log);
    assert_eq!(
        name4, "think",
        "step 4: the second consecutive refusal must be substituted for think"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.consecutive_continues, 1,
        "unit 47's core carve-out must leave consecutive_continues unchanged by a think step"
    );
    assert_eq!(
        state.agent.plan.first().map(|s| s.status),
        Some(StepStatus::Pending),
        "the think substitution must still have dropped the status change: {:?}",
        state.agent.plan
    );
    // At step 4, `premature_done_note` itself already sees
    // `consecutive_continues == 1` (the first refusal's own commit), so it
    // produces the REPEAT note (item 37's `OPEN_STEP_NOTE_REPEAT`) before
    // `refusal_thinks` gets a look and substitutes that note's text into
    // REFUSAL_QUESTION -- the refusal being answered here is the repeat
    // refusal, not the first one.
    let step4_refusal_note = fill_template_repeat();
    let expected_question = REFUSAL_QUESTION.replace("{note}", &step4_refusal_note);
    assert_eq!(args4, json!({"question": expected_question}));
    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, expected_question);
    let decision = state
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, expected_question);
    assert_eq!(decision.answer, THINKER_ANSWER);
    // A `think` step's own tool output already carries the observation
    // (unlike a plain refusal's `Action::Continue`, which logs a
    // `continued` System entry with just the note): `refusal_thinks`'s note
    // Step 5: THIRD consecutive refusal -> once per streak blocks a second
    // think substitution (the previous committed step was itself the
    // substituted think), so this is the ordinary repeat-refusal Continue,
    // item 37's OPEN_STEP_NOTE_REPEAT -- not another think.
    // consecutive_continues advances to 2 (from 1, unaffected by step 4's
    // think).
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");
    // A `think` step's own tool output already carries the observation
    // (unlike a plain refusal's `Action::Continue`, which logs a
    // `continued` System entry with just the note): `refusal_thinks`'s note
    // from step 4 is appended to that observation (`core/src/session.rs`'s
    // step 10.5 note-appending, since `execute_think` already set
    // `pending_observation`), so it shows up in the prompt fed to step 5
    // rather than a `continued` entry. `prompts[i]` is the prompt fed to
    // step `i + 1` (`error_think.rs`'s own convention): `prompts[4]` (now
    // available, step 5 having just run) is built after step 4 and must
    // carry the prefix plus the exact repeat-refusal note.
    let expected_note4 = format!("{REFUSAL_THINK_NOTE_PREFIX}{step4_refusal_note}");
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 5,
        "expected at least 5 prompts (one per step run so far): {}",
        prompts.len()
    );
    assert!(
        prompts[4].user.contains(&expected_note4),
        "step 4's think substitution must append the prefix plus the exact repeat-refusal note \
         to the prompt fed to step 5:\n{}",
        prompts[4].user
    );
    let (name5, _args5) = last_action(&log);
    assert_eq!(
        name5, "continue",
        "step 5: once-per-streak must block a second think substitution"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.consecutive_continues, 2);
    assert_eq!(
        state.agent.plan.first().map(|s| s.status),
        Some(StepStatus::Pending),
        "the repeat refusal must have dropped the status change: {:?}",
        state.agent.plan
    );
    assert_eq!(
        thinker.calls().len(),
        1,
        "step 5 must not have called the thinker again"
    );
    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    let expected_note = OPEN_STEP_NOTE_REPEAT
        .replacen("{}", "1", 1)
        .replacen("{}", "the first change", 1)
        .replacen("{}", "`test`", 1);
    assert_eq!(
        note, &expected_note,
        "step 5 must produce the exact repeat-refusal note"
    );

    // Step 6: a real tool call (`test`) -> not a Continue, so
    // consecutive_continues resets to 0.
    let rec6 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome6 = log.append_record(rec6).unwrap();
    assert!(
        matches!(outcome6, Outcome::Continue),
        "got {outcome6:?}; the ordinary tool-call outcome is still `Continue` at the `Outcome` \
         level, but the committed `Action` must be a real `Tool` call, not the harness-internal \
         `Action::Continue`"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 3,
        "the `test` call must have executed for real (the earlier failing test, this one, and \
         step 4's substituted think all count)"
    );
    assert_eq!(
        state.harness.consecutive_continues, 0,
        "a real tool call must reset consecutive_continues to 0"
    );
}

// ---------------------------------------------------------------------
// Unit 47 acceptance (c): a real tool call between two refusals resets the
// streak -- `consecutive_continues` goes back to 0, so the NEXT refusal is
// treated as a first refusal again (a plain OPEN_STEP_NOTE_FIRST note), not
// substituted for think.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_tool_call_between_two_refusals_resets_the_streak() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Seed a two-step plan (both pending).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "the first change", "status": "pending"},
                        {"id": 2, "text": "the second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
        // A failing test establishes last_test_cmd with green_step staying
        // None.
        test_turn("false"),
        // Step 3: first refusal -> Continue, consecutive_continues becomes
        // 1.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
        // Step 4: a real tool call (a no-op search) -- breaks the Continue
        // streak, consecutive_continues resets to 0.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "list_files", "args": {"path": "."}},
            })
            .to_string(),
        ),
        // Step 5: `done` again -> the streak was reset, so this is a FIRST
        // refusal again (OPEN_STEP_NOTE_FIRST), not a think substitution.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: seed the plan.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    // Step 2: the failing test.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    // Step 3: first refusal.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(state3.harness.consecutive_continues, 1);

    // Step 4: a real tool call -> not a Continue, resets the streak.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, _args4) = last_action(&log);
    assert_eq!(
        name4, "list_files",
        "step 4 must be the real tool call, unsubstituted"
    );
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.consecutive_continues, 0,
        "a real tool call must reset consecutive_continues to 0"
    );

    // Step 5: `done` again -> a FIRST refusal (fresh streak), not a think
    // substitution: refusal_thinks requires consecutive_continues == 1
    // going in, which is only true on the SECOND refusal of an unbroken
    // streak -- here the streak restarted at step 4, so this is that
    // streak's own first refusal.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");
    let (name5, _args5) = last_action(&log);
    assert_eq!(
        name5, "continue",
        "step 5: a fresh streak's first refusal must be a plain note, not think"
    );
    let notes5 = continued_notes(&log);
    let note5 = notes5
        .last()
        .expect("a `continued` System entry must exist");
    let expected_note5 =
        OPEN_STEP_NOTE_FIRST
            .replacen("{}", "1", 1)
            .replacen("{}", "the first change", 1);
    assert_eq!(
        note5, &expected_note5,
        "step 5 must produce the exact first-refusal note, not a repeat or think note"
    );
    let (state5, _v) = active.session().state().await.unwrap();
    assert_eq!(state5.harness.consecutive_continues, 1);
}
