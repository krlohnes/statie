//! `review_blind_approval_runs_test` (`skills/src/coding/mod.rs`, wired
//! into `ReviewSkill::validate` ahead of `review_looked_note`): live runs
//! 69/70 showed `review` sending `done: true` with empty `findings` and no
//! tool call, three times in a row -- the existing look-first conversion
//! (`review_looked_note`) turned each into `Action::Continue` with a note
//! asking it to read the changed files and run the unfiltered tests, but
//! the model just repeated the blind approval until the repeat cap parked
//! the session. Run 71 showed what works instead: once the model has a
//! real test observation in front of it, it follows the harness's steer.
//! So an approval sent while `cur.harness.tool_calls == 0` now substitutes
//! a real `test` tool call for the action -- `{"cmd": <last_test_cmd>}`
//! when `HarnessState::last_test_cmd` (`core/src/version.rs`) is `Some`
//! (the command the most recent `test` call ran, set at the same site as
//! `green_step`, surviving a handoff), or `{}` (auto-detect) when it is
//! `None`.
//!
//! Fixture style of `skills/tests/premature_done.rs`/`review_looks.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `review_looks.rs`/
//! `no_green.rs` inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
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

/// A turn that runs action `test` with the given shell command and an
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

/// Finds the most recent `Action` log entry's `name`/`args`.
fn last_action(log: &Arc<dyn SessionLog>) -> (String, Value) {
    let entries = log.entries(Some(LogKind::Action), 10_000);
    let entry = entries.last().expect("an Action entry must exist");
    (
        entry
            .payload
            .get("name")
            .and_then(Value::as_str)
            .expect("action entry must have a name")
            .to_string(),
        entry.payload.get("args").cloned().unwrap_or(Value::Null),
    )
}

// ---------------------------------------------------------------------
// review, tool_calls == 0, last_test_cmd: Some(cmd) (carried over from
// implement's own passing `test` call, surviving the handoff) -> the
// substituted action is `test` with `{"cmd": cmd}`, not auto-detect; done
// and next_skill are stripped; the note lands in the next observation;
// tool_calls becomes 1 afterward.
// ---------------------------------------------------------------------

#[tokio::test]
async fn blind_approval_with_last_test_cmd_reruns_the_same_command() {
    let root = tempfile::tempdir().unwrap();
    let cmd = "true";

    // Drive `implement` to a handoff via a passing `test` call, so
    // `last_test_cmd` is set to `cmd` and survives into `review`'s
    // activation (tool_calls resets to 0, last_test_cmd does not).
    let implement_turns = vec![
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        test_turn(cmd),
        MockTurn::new(
            json!({
                "patch": {"next_skill": "review"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let review_turns = vec![
        done_turn(json!({"done": true})),
        // Second turn just so the next prompt (with the substituted test's
        // observation) can be captured.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let mut all_turns = implement_turns;
    all_turns.extend(review_turns);
    let model = Arc::new(MockModel::new(all_turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let implement: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&implement)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; setup: implement must hand off to review"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.last_test_cmd,
        Some(cmd.to_string()),
        "setup: implement's passing test call must have set last_test_cmd"
    );

    drop(active);
    let review: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&review)).await.unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 0,
        "review's new activation must start with tool_calls reset to 0"
    );
    assert_eq!(
        state.harness.last_test_cmd,
        Some(cmd.to_string()),
        "last_test_cmd must survive the handoff, unlike tool_calls"
    );

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; the blind approval must convert to a substituted `test` call"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done, "the approval must not have landed");
    // `next_skill` was already "review" from implement's own handoff patch
    // (a workspace field, committed before review activated) -- this
    // step's raw patch carries no `next_skill` at all, so there is nothing
    // to strip; the committed value is simply untouched.
    assert_eq!(state.agent.next_skill.as_deref(), Some("review"));
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted test call must have executed"
    );

    let (name, args) = last_action(&log);
    assert_eq!(name, "test");
    assert_eq!(
        args,
        json!({"cmd": cmd}),
        "the substituted call must rerun the SAME command implement last verified with"
    );

    // Second step: capture the prompt carrying the substituted test's
    // observation, and check the note landed verbatim.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    // 3 implement prompts + 2 review prompts so far.
    let last = prompts.last().expect("a prompt must have been recorded");
    let body = observation_body(&last.user);
    assert!(
        body.contains(BLIND_APPROVAL_NOTE),
        "the substituted test's observation must carry the blind-approval note verbatim: {body}"
    );
}

// ---------------------------------------------------------------------
// review, tool_calls == 0, last_test_cmd: None (no `test` call has run at
// all this session) -> the substituted action is `test` with `{}`
// (auto-detect).
// ---------------------------------------------------------------------

#[tokio::test]
async fn blind_approval_with_no_last_test_cmd_auto_detects() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({"done": true}))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 0);
    assert_eq!(state.harness.last_test_cmd, None);

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1);

    let (name, args) = last_action(&log);
    assert_eq!(name, "test");
    assert_eq!(
        args,
        json!({}),
        "with no last_test_cmd, the substituted call must auto-detect (empty args)"
    );
}

// ---------------------------------------------------------------------
// review, tool_calls == 0, last_test_cmd: Some(cmd) -- the substituted
// test call passes unfiltered -> the model's NEXT `done: true` (tool_calls
// == 1 now) is accepted as a real approval.
// ---------------------------------------------------------------------

#[tokio::test]
async fn blind_approval_followed_by_clean_pass_then_approval_lands() {
    let root = tempfile::tempdir().unwrap();

    // Drive `implement` to a handoff via a passing `test true`, so
    // `last_test_cmd` is `Some("true")` and survives into review's
    // activation -- giving the substituted call something that actually
    // succeeds, the same setup as
    // `blind_approval_with_last_test_cmd_reruns_the_same_command` above.
    let implement_turns = vec![
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        test_turn("true"),
        MockTurn::new(
            json!({
                "patch": {"next_skill": "review"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let review_turns = vec![
        done_turn(json!({"done": true})),
        // After the substituted `test true` passes, the model approves
        // again.
        done_turn(json!({"done": true, "findings": []})),
    ];
    let mut all_turns = implement_turns;
    all_turns.extend(review_turns);
    let model = Arc::new(MockModel::new(all_turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let implement: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&implement)).await.unwrap();
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Handoff(ref t) if t == "review"));

    drop(active);
    let review: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&review)).await.unwrap();

    // Step: blind approval -> substituted `test true` (passes cleanly,
    // unfiltered).
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1);
    assert_eq!(
        state.harness.green_step,
        Some(state.harness.step),
        "the substituted test passed: green_step must be set"
    );
    assert!(!state.harness.green_filtered);

    // Step: the model approves again -- tool_calls >= 1 and an unfiltered
    // pass, so this is a real approval.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Done),
        "got {outcome2:?}; a real approval after the substituted clean pass must land"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done);
}

// ---------------------------------------------------------------------
// review, tool_calls == 0, last_test_cmd: Some(cmd) -- the substituted
// test call FAILS -> the model's next `done: true` hits the existing
// no-green Continue (review_no_green_note), not a fresh blind-approval
// substitution (tool_calls is 1 by then).
// ---------------------------------------------------------------------

#[tokio::test]
async fn blind_approval_followed_by_failure_then_no_green_continue() {
    let root = tempfile::tempdir().unwrap();

    // No implement activation: review's own first substituted call fails
    // (no cmd detectable in this bare sandbox, which is still a "look" per
    // the doc), so the SECOND identical blind approval falls to
    // review_no_green_note (green_step stays None), not a second
    // substitution.
    let turns = vec![
        done_turn(json!({"done": true})),
        done_turn(json!({"done": true, "findings": []})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: blind approval -> substituted test call fails (no command
    // detectable in this bare sandbox).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.tool_calls, 1);
    assert_eq!(
        state.harness.green_step, None,
        "the substituted test failed, so green_step stays None"
    );

    // Step 2: tool_calls >= 1 now, so no fresh substitution -- falls to
    // review_no_green_note instead.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; the second approval after a failed substituted test must hit the \
         existing no-green Continue, not run yet another substituted test"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done);
    assert_eq!(
        state.harness.tool_calls, 1,
        "the second step must not have executed a new tool call"
    );
}

// ---------------------------------------------------------------------
// review, `done: true` WITH findings, no tool call -> unaffected: this is
// a send-back, not an approval, so `review_blind_approval_runs_test` never
// fires (matches `review_looked_note`'s own `findings`-empty requirement).
// ---------------------------------------------------------------------

#[tokio::test]
async fn done_with_findings_and_no_tool_call_is_unaffected_by_blind_approval_fix() {
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
         zero tool calls this activation -- it is not an approval, so the blind-approval fix \
         must not touch it"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done);
    assert_eq!(state.agent.findings, vec!["something is wrong".to_string()]);
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
    assert_eq!(
        state.harness.tool_calls, 0,
        "a send-back never executes a tool call"
    );
}

// ---------------------------------------------------------------------
// The redundant-verification guard case (spec point 2's carve-out): the
// substituted `test` call happens to be identical to a call already
// recorded (same command, nothing edited since) -- guard 7.7/7.8 may skip
// re-running it and instead return an observation quoting the earlier
// result. That is acceptable: the skip observation is still a real look,
// so no special-casing is needed in `review_blind_approval_runs_test`.
// This test documents that shape: implement's own passing `test` call
// leaves that exact command recorded; review's blind approval substitutes
// the SAME command, and the guard may serve the cached result instead of
// re-running the shell command, but the call still counts (tool_calls
// becomes 1) and the note still lands.
// ---------------------------------------------------------------------

#[tokio::test]
async fn blind_approval_substituted_call_may_hit_redundant_verification_guard() {
    let root = tempfile::tempdir().unwrap();
    let cmd = "true";

    let implement_turns = vec![
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        test_turn(cmd),
        MockTurn::new(
            json!({
                "patch": {"next_skill": "review"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let review_turns = vec![done_turn(json!({"done": true}))];
    let mut all_turns = implement_turns;
    all_turns.extend(review_turns);
    let model = Arc::new(MockModel::new(all_turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let implement: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&implement)).await.unwrap();
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Handoff(ref t) if t == "review"));

    drop(active);
    let review: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&review)).await.unwrap();

    // review's blind approval substitutes `test true` -- identical to the
    // command implement itself last ran, with nothing edited in between.
    // Whether the redundant-verification guard serves a cached result or
    // the tool actually re-runs, the call still executes and counts.
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; the substituted call must still execute (or be served from the guard) \
         as a normal tool-call step, not be rejected"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted call counts as executed even if the guard served a cached result"
    );
    assert!(!state.agent.done, "the approval must not have landed");
}
