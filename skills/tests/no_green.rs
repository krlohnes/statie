//! The no-green-run conversions in `skills/src/coding/mod.rs`
//! (`implement_no_green_note`/`review_no_green_note`, wired into
//! `ImplementSkill::validate`/`ReviewSkill::validate`): live run 59 --
//! `implement` handed off to `review` right after its own `test` call
//! failed (a non-exhaustive match), and `review` then ran the unfiltered
//! test, saw it fail, and approved anyway (`done: true`, `findings: []`).
//! The harness ended in `done` with a tree that does not compile. The
//! harness already knows whether the latest `test` call passed
//! (`cur.harness.green_step: Option<u64>`, `core/src/version.rs`: `Some`
//! on a pass, `None` on a fail or when no `test` has run) so both
//! approvals now convert to the harness-internal `Action::Continue`
//! instead of landing.
//!
//! Fixture style of `skills/tests/premature_done.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `premature_done.rs`
//! and `review_looks.rs` inline their own `config`/
//! `make_file_tools_session`: `skills/tests/common/mod.rs` exports items
//! this file has no use for, and `-D warnings` fails a binary that leaves
//! any of a shared module's items unused.
//!
//! Fix 3 (a later bench battery) reordered `ImplementSkill::validate`'s
//! chain so `implement_no_green_runs_test` runs BEFORE `premature_done_note`
//! and fires even with an open plan step, as long as `green_step: None` and
//! either something was edited since the last test or none has ever run --
//! see `implement_done_with_open_step_and_no_green_run_ever_runs_test_instead`
//! and `implement_done_with_open_step_and_failing_run_uses_premature_done_note`
//! below for the priority split this left behind.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, ReviewSkill};
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

/// A turn that runs action `test` with the given shell command and an
/// empty patch. Same as `premature_done.rs`'s `test_turn`.
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

/// A turn that reads a file (a harmless, real tool call).
fn read_file_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// A turn that sends action `done` with the given raw patch object.
fn done_turn(patch: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {"type": "done"},
        })
        .to_string(),
    )
}

const IMPLEMENT_NOTE: &str = "the most recent `test` run failed, or none has run yet; handing \
                               off needs a passing run. Run `test` now; if it fails, fix what \
                               it reports before `done`.";

const REVIEW_NOTE: &str = "the most recent test run failed, or none has run in this session; \
                            approval needs a passing unfiltered run. Run the tests; if they \
                            fail, send it back with `done: true` and the failure as a finding.";

const REVIEW_FILTERED_NOTE: &str = "the passing test run was filtered, so part of the suite \
                                     never ran; approval needs an unfiltered pass. Run the same \
                                     test command without the test-name filter; if it fails, \
                                     send it back with `done: true` and the failure as a \
                                     finding.";

/// A shell command that exits 0 and prints a cargo-style summary line with
/// a `filtered out` count, matching what `tools/src/test_tool.rs`'s
/// `filtered_out_note` looks for -- same trick `core/tests/green_filtered.rs`
/// uses for the real `test` tool.
const FILTERED_PASS_CMD: &str = "sh -c 'echo \"test result: ok. 1 passed; 0 failed; 0 ignored; \
                                  0 measured; 17 filtered out; finished in 0.00s\"'";

// ---------------------------------------------------------------------
// implement: plan fully done, green_step: None, no `test` call has ever run
// (last_test_cmd: None) -> `implement_no_green_runs_test` substitutes a real
// `test` tool call instead of just asking for one (mirrors
// `review_blind_approval_runs_test`'s substitution on the review side; see
// `skills/tests/no_green_runs_test.rs` for the auto-detect `args: {}` case
// and the edits-since-test case with a known `last_test_cmd`).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_no_green_run_ever_runs_test_instead_of_continue() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a one-step plan, already done (no open step), and set
        // recent_summary so we can assert it's kept when the done is
        // converted. No `test` call has run yet, so `green_step` stays
        // `None` and `last_test_cmd` stays `None`.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "only step", "status": "done"}],
                    "recent_summary": "finished the only step",
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: done, handing off to review, with no passing test run.
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

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step, None,
        "no `test` call has run yet: green_step must be None"
    );
    assert_eq!(state.harness.last_test_cmd, None);
    assert_eq!(state.harness.edits_since_test, 0);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; done with no passing test run must run the test, not hand off"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must have been stripped, not committed -- no handoff happened"
    );
    assert_eq!(
        state.agent.recent_summary, "finished the only step",
        "the rest of the patch from step 1 must be untouched"
    );
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted `test` call must have actually executed"
    );

    let notes = continued_notes(&log);
    assert!(
        notes.is_empty(),
        "the runs-test note is appended to the tool observation, not logged as its own \
         `continued` System entry: {notes:?}"
    );
}

// ---------------------------------------------------------------------
// implement: same, but green_step: Some(n) -> handoff to review proceeds.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_passing_test_still_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a one-step plan, already done.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: a passing test -> green_step gets set.
        test_turn("true"),
        // Step 3: done, handing off to review.
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
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be set after the passing test"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; done with a passing test run must hand off to review"
    );
    assert!(!any_rejected(&log));
}

// ---------------------------------------------------------------------
// implement: plan has an open step and green_step: None, no test call has
// ever run (last_test_cmd: None) -> fix 3 (`skills/src/coding/mod.rs`)
// reordered the chain so `implement_no_green_runs_test` now runs BEFORE
// `premature_done_note` and fires even with the open step: running the
// test is the right next move whatever plan step is open. See
// `implement_done_with_open_step_and_failing_run_uses_premature_done_note`
// below for the one case where the premature-done note keeps its old
// priority.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_open_step_and_no_green_run_ever_runs_test_instead() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a two-step plan (one open).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "done"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: premature done with an open step, no passing test run.
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
// implement: plan has an open step, green_step: None, but a FAILING test is
// the latest run and nothing has been edited since -> the test substitution
// declines (re-running the identical failing command would tell the model
// nothing new) and the premature-done note keeps its old priority, exactly
// as before fix 3.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_open_step_and_failing_run_uses_premature_done_note() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a two-step plan (one open).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "done"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: a failing test establishes last_test_cmd with green_step
        // staying None, and edits_since_test at 0.
        test_turn("false"),
        // Step 3: premature done with an open step.
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
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the test call failed");
    assert_eq!(state.harness.edits_since_test, 0);

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("plan step 2 is still open"),
        "the premature-done note (open step) must fire ahead of the no-green-run note: {note}"
    );
    assert_ne!(
        note, IMPLEMENT_NOTE,
        "the no-green-run note must not fire when a plan step is still open"
    );
}

// ---------------------------------------------------------------------
// implement: plan fully done, a FAILING test is the latest run, and nothing
// has been edited since (edits_since_test: 0) -> falls back to the old
// Continue-with-note behaviour instead of re-running the same failing
// command (see `skills/tests/no_green_runs_test.rs` for the case where
// edits_since_test > 0, which DOES run the test again).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_failing_run_and_no_edits_since_continues_with_note() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a one-step plan, already done.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: a failing test -> green_step stays None, last_test_cmd is
        // Some, edits_since_test is reset to 0 by the test call itself.
        test_turn("false"),
        // Step 3: done, handing off to review, with the failing run still
        // the latest and nothing edited since.
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
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the test call failed");
    assert_eq!(state.harness.last_test_cmd, Some("false".to_string()));
    assert_eq!(
        state.harness.edits_since_test, 0,
        "the test call itself resets edits_since_test, and nothing was edited after it"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; a failing run with nothing edited since must fall back to Continue"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must have been stripped, not committed -- no handoff happened"
    );
    assert_eq!(
        state.harness.tool_calls, 1,
        "the test must NOT have been re-run: only step 2's own test call executed"
    );

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, IMPLEMENT_NOTE,
        "the unchanged Continue-fallback note text must match exactly"
    );
}

// ---------------------------------------------------------------------
// review: tool_calls >= 1, green_step: None, done: true, no findings ->
// Continue with the exact review note; second identical approval gives the
// same note again.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_no_green_run_continues_instead_of_approving() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        read_file_turn("a.txt"),
        done_turn(json!({"done": true, "findings": []})),
        // Second identical approval: same note again.
        done_turn(json!({"done": true, "findings": []})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "the read itself is an ordinary tool-call step: got {outcome1:?}"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1);
    assert_eq!(state.harness.green_step, None, "no `test` call has run");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; an approval with no passing test run must convert to Continue, not \
         approve"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must still be false -- the approval must not land"
    );

    let notes = continued_notes(&log);
    let note1 = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(note1, REVIEW_NOTE, "note text must match exactly");

    // Third step: the second identical approval.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; the second identical approval must also convert to Continue"
    );
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note2 = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note2, REVIEW_NOTE,
        "the repeated approval must give the identical note, so the repeat cap can catch it"
    );
}

// ---------------------------------------------------------------------
// review: tool_calls >= 1, green_step: Some(n), approval -> done proceeds.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_passing_test_still_approves() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        test_turn("true"),
        done_turn(json!({"done": true, "findings": []})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be set after the passing test"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Done),
        "got {outcome2:?}; an approval after a passing test run must land as Done"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done, "the approval must have landed");
}

// ---------------------------------------------------------------------
// review: green_step: None, done: true WITH findings -> send-back handoff
// to implement as before (unaffected).
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_sendback_with_findings_and_no_green_run_is_unaffected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "files": [{"path": "a.txt", "note": "under review"}], "findings": ["a.txt:1 something is wrong"],
        "next_skill": "implement",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None);

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; sending work back with findings must hand off to implement even with \
         no passing test run this session -- it is not an approval"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done);
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 something is wrong".to_string()]
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
}

// ---------------------------------------------------------------------
// review: tool_calls == 0, green_step: None -> `review_blind_approval_runs_test`
// fires ahead of both the look-first note and the no-green-run note
// (priority): the harness substitutes a real `test` call instead of just
// asking for one. See `skills/tests/review_blind.rs` for this
// substitution's own dedicated tests.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_no_tool_call_and_no_green_run_runs_test_instead() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({"done": true, "findings": []}))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 0);
    assert_eq!(state.harness.green_step, None);

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted `test` call must have actually executed"
    );

    let notes = continued_notes(&log);
    assert!(
        notes.is_empty(),
        "the blind-approval note is appended to the tool observation, not logged as its own \
         `continued` System entry: {notes:?}"
    );
    assert_eq!(
        state.harness.green_step, None,
        "the substituted test failed (no command detectable in this bare sandbox), so \
         green_step stays None"
    );
}

// ---------------------------------------------------------------------
// review: green_step: Some(n), green_filtered: true, done: true, no
// findings -> Continue with the exact filtered-note text; `done` and
// `next_skill` stripped from the raw patch so the approval does not land.
// Live run 67: review ran the same filtered command implement used, saw it
// pass, and approved anyway.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_filtered_pass_continues_instead_of_approving() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        test_turn(FILTERED_PASS_CMD),
        done_turn(json!({"done": true, "findings": [], "next_skill": "implement"})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be set after the passing test"
    );
    assert!(
        state.harness.green_filtered,
        "the passing run's output noted a filtered count: green_filtered must be true"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; an approval after a FILTERED passing run must convert to Continue, \
         not approve"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must still be false -- the approval must not land"
    );
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must be stripped from the raw patch so no handoff lands"
    );

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(note, REVIEW_FILTERED_NOTE, "note text must match exactly");
}

// ---------------------------------------------------------------------
// review: green_step: Some(n), green_filtered: false (an UNFILTERED
// passing run), approval -> done proceeds as before.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_unfiltered_pass_still_approves() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        test_turn("true"),
        done_turn(json!({"done": true, "findings": []})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    // one past: the `tests` record commits its own version after every `test` call
    assert_eq!(state.harness.green_step, Some(state.harness.step - 1));
    assert!(
        !state.harness.green_filtered,
        "an unfiltered passing run must leave green_filtered false"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Done),
        "got {outcome2:?}; an approval after an unfiltered passing run must land as Done"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done, "the approval must have landed");
}

// ---------------------------------------------------------------------
// review: green_filtered: true, done: true WITH findings -> send-back
// handoff to implement as before (unaffected by the filtered-note rule,
// same as the no-green-run rule).
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_sendback_with_findings_and_filtered_pass_is_unaffected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        test_turn(FILTERED_PASS_CMD),
        done_turn(json!({
            "files": [{"path": "a.txt", "note": "under review"}], "findings": ["a.txt:1 something is wrong"],
            "next_skill": "implement",
        })),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.harness.green_filtered);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}; sending work back with findings must hand off to implement even with \
         a filtered passing run -- it is not an approval"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done);
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 something is wrong".to_string()]
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
}

// ---------------------------------------------------------------------
// implement: plan fully done, green_step: Some(n), green_filtered: true ->
// handoff proceeds (implement is NOT affected by the filtered-note rule --
// a filtered pass is still a green checkpoint for implement).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_with_filtered_pass_still_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed a one-step plan, already done.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 2: a FILTERED passing test -> green_step gets set, and
        // green_filtered: true.
        test_turn(FILTERED_PASS_CMD),
        // Step 3: done, handing off to review.
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
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be set after the passing test"
    );
    assert!(
        state.harness.green_filtered,
        "the passing run's output noted a filtered count: green_filtered must be true"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; implement's done must hand off to review even on a filtered pass -- \
         implement is not affected by the filtered-note rule"
    );
    assert!(!any_rejected(&log));
}
