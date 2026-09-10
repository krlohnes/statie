//! Review must look before it approves (`review_looked_note`,
//! `skills/src/coding/mod.rs`, wired into `ReviewSkill::validate`): a live
//! run showed review's first and only step approving a diff it never
//! read -- `{"patch": {"done": true, "findings": []}, "action": {"type":
//! "done"}}`, 71 completion tokens, without a single tool call.
//!
//! An approval (`done: true`) sent while `cur.harness.tool_calls == 0` (the
//! harness-owned count of tool calls actually executed in the current
//! activation, `core/src/version.rs`) is now caught FIRST by
//! `review_blind_approval_runs_test` (live runs 69/70: the ask-first note
//! this file used to document as the whole fix did not work on this model
//! -- it just repeated the blind approval until the repeat cap parked the
//! session), which substitutes a real `test` tool call for the action
//! instead of merely asking for a look; `review_looked_note` remains as the
//! fallback once `tool_calls` is no longer `0` (see `skills/tests/
//! review_blind.rs` for the substitution's own dedicated tests). Either
//! way the patch commits but `done` (and `next_skill`) is stripped from
//! it, so the approval itself does not commit on that step.
//!
//! Fixture style of `skills/tests/premature_done.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `done_stays_done.rs`,
//! `commitment_rules.rs`, and `premature_done.rs` inline their own `config`/
//! `make_file_tools_session`: `skills/tests/common/mod.rs` exports items
//! this file has no use for, and `-D warnings` fails a binary that leaves
//! any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ReviewSkill};
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

/// A turn that reads a file (a harmless, real tool call -- `a.txt` is
/// written to the sandbox root before the session starts in the tests that
/// use this).
fn read_file_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// A turn that runs action `test` with the given shell command and an
/// empty patch. Same shape as `premature_done.rs`'s `test_turn` -- used
/// here so a "look" also sets `cur.harness.green_step`
/// (`review_no_green_note`, `skills/src/coding/mod.rs`, gates an approval
/// on a passing run separately from the look-before-approve gate this file
/// is about).
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

/// The exact note `review_blind_approval_runs_test` attaches
/// (`skills/src/coding/mod.rs`).
const BLIND_APPROVAL_NOTE: &str = "you approved without looking at anything, so the harness ran \
                                    the tests for you (below). Read the changed files if the \
                                    result is not enough to judge, then approve with `done: \
                                    true` or send it back with findings.";

/// The `## Observation` section's body of the given recorded prompt
/// (`statie_models::RecordedPrompt::user`).
fn observation_body(prompt: &str) -> &str {
    let idx = prompt
        .find("## Observation")
        .expect("prompt should carry an Observation section");
    let heading = "## Observation\n";
    prompt[idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading")
}

// ---------------------------------------------------------------------
// review + immediate `done: true` with no tool call -> the harness
// substitutes a real `test` tool call instead of asking for a look
// (`review_blind_approval_runs_test`, `skills/src/coding/mod.rs`): the
// approval does not land, but a real tool call executes (tool_calls
// becomes 1), not a bare `Continue`, and the note is appended to the test
// tool's own observation.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_immediate_approval_with_no_tool_call_runs_test_instead_of_approving() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        done_turn(json!({"done": true})),
        // Second turn just so the prompt carrying the substituted test's
        // observation (with the blind-approval note appended) can be
        // captured from `model.prompts()`.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an approval with no tool call this activation must convert to a \
         substituted `test` call, not Done"
    );
    assert!(
        !any_rejected(&log),
        "must not have been rejected -- the patch is kept, just not the approval"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must still be false -- the approval must not land"
    );
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted `test` call must have actually executed"
    );

    let entries = log.entries(None, 10_000);
    let action_entry = entries
        .iter()
        .find(|e| e.kind == LogKind::Action)
        .expect("an Action entry must exist for the substituted tool call");
    assert_eq!(
        action_entry.payload.get("name").and_then(Value::as_str),
        Some("test"),
        "the substituted action must be a `test` tool call: {action_entry:?}"
    );

    let notes = continued_notes(&log);
    assert!(
        notes.is_empty(),
        "the blind-approval note is appended to the tool observation, not logged as its own \
         `continued` System entry: {notes:?}"
    );

    // Run one more step so the next prompt (carrying the substituted test's
    // observation) is captured.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let body = observation_body(&prompts[1].user);
    assert!(
        body.contains(BLIND_APPROVAL_NOTE),
        "the substituted test's observation must carry the blind-approval note verbatim: {body}"
    );
}

// ---------------------------------------------------------------------
// review + one `read_file`, then a passing `test`, then `done: true` ->
// Outcome::Done: the look satisfies the look-before-approve gate
// (tool_calls == 2), and the passing test satisfies the separate no-green-
// run gate (`review_no_green_note`, `skills/src/coding/mod.rs`), so the
// approval lands normally.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_after_one_read_file_call_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        read_file_turn("a.txt"),
        test_turn("true"),
        done_turn(json!({"done": true})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: read_file -- one executed tool call.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "got {outcome1:?}; the read itself is an ordinary tool-call step"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "tool_calls must be 1 after the executed read_file call"
    );

    // Step 2: a passing test -- tool_calls becomes 2, and green_step is set.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; the test itself is an ordinary tool-call step"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 2);
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "green_step must be set after the passing test"
    );

    // Step 3: done: true -- tool_calls >= 1 and green_step is set, so this
    // is a real approval.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Done),
        "got {outcome3:?}; an approval after a look and a passing test must land as Done"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.done,
        "the committed workspace's `done` must be true -- the approval landed"
    );
}

// ---------------------------------------------------------------------
// review + `done` with findings and `next_skill: implement`, no tool call
// -> handoff to implement, unaffected by the look-before-approve gate
// (this is not an approval).
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_send_back_with_findings_and_no_tool_call_is_unaffected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "findings": ["something is wrong"],
        "next_skill": "implement",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; sending work back with findings must hand off to implement even with \
         zero tool calls this activation -- it is not an approval"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done);
    assert_eq!(state.agent.findings, vec!["something is wrong".to_string()]);
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
}

// ---------------------------------------------------------------------
// review + `done: true` + empty `findings` + `next_skill: "implement"` +
// no tool call -> substituted `test` call (not a rejection):
// `review_blind_approval_runs_test` strips both `done` and `next_skill`
// from the raw patch and replaces the action with a `test` tool call, so
// the core rule "changing `next_skill` requires action `done`" (session.rs
// step 7.5) never sees a non-`done` action paired with a changed handoff
// target. Neither `next_skill` nor `done` lands.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_approval_with_next_skill_and_no_tool_call_runs_test_and_drops_next_skill() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "done": true,
        "findings": [],
        "next_skill": "implement",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an approval with a stray `next_skill` and no tool call must still \
         convert to a substituted `test` call, not be rejected by the \
         `next_skill`-requires-`done` rule"
    );
    assert!(
        !any_rejected(&log),
        "must not have been rejected -- the patch is kept, just not the approval/handoff"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must still be false -- the approval must not land"
    );
    assert_eq!(
        state.agent.next_skill, None,
        "`next_skill` must not have landed either -- it was stripped alongside `done`"
    );
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted `test` call must have actually executed"
    );

    let entries = log.entries(None, 10_000);
    let action_entry = entries
        .iter()
        .find(|e| e.kind == LogKind::Action)
        .expect("an Action entry must exist for the substituted tool call");
    assert_eq!(
        action_entry.payload.get("name").and_then(Value::as_str),
        Some("test"),
        "the substituted action must be a `test` tool call: {action_entry:?}"
    );
}

// ---------------------------------------------------------------------
// Four approvals in a row with no prior tool call and no project test
// command to auto-detect: the first is caught by
// `review_blind_approval_runs_test` and substitutes a `test` call, which
// fails (no command detected in this bare sandbox) without setting
// `green_step`; `tool_calls` is then 1, so the fix fires only that once
// (its own guard, see that function's doc) and every further identical
// approval instead falls to `review_no_green_note` (`green_step` is still
// `None`), converting to `Action::Continue` with the identical
// no-green-run note. The third occurrence of THAT note hits
// `CONTINUE_REPEAT_CAP` and parks as `Park::AskUser` instead of looping
// forever (the live-run failure mode this whole unit fixes -- runs 69/70
// sent this shape three times running, and runs 45-46 35 times, before the
// cap existed).
// ---------------------------------------------------------------------

#[tokio::test]
async fn four_consecutive_identical_approvals_with_no_tool_call_parks_after_the_substituted_test() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        done_turn(json!({"done": true})),
        done_turn(json!({"done": true})),
        done_turn(json!({"done": true})),
        done_turn(json!({"done": true})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: the blind-approval fix substitutes a `test` call (which
    // fails -- no command detectable -- but still executes and counts).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "step 1: got {outcome1:?}"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1, "step 1: substituted test ran");
    assert_eq!(
        state.harness.green_step, None,
        "step 1: the substituted test failed, so green_step stays None"
    );

    // Step 2: tool_calls >= 1 now, so the blind-approval fix does not fire
    // again; green_step is None, so review_no_green_note converts instead.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "step 2: got {outcome2:?}"
    );

    // Step 3: identical note again -- second occurrence.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "step 3: got {outcome3:?}"
    );

    // Step 4: third occurrence of the identical note -> repeat cap parks.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    match outcome4 {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("the most recent test run failed, or none has run in this session"),
                "park message should contain the repeated no-green-run note: {msg}"
            );
            assert!(
                msg.contains("3 times in a row"),
                "park message should say it repeated 3 times in a row: {msg}"
            );
        }
        other => panic!("step 4: expected Parked(AskUser(_)), got {other:?}"),
    }
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must still be false after the park"
    );
}
