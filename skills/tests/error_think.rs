//! `implement_repeated_error_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `implement_repeated_skip_thinks`
//! and before `implement_repeat_failure_thinks`): unit 23 part (b) ("a
//! repeated failing call becomes a `think`"). Core's own repeated-tool-error
//! guard (`HarnessState::last_tool_error`, `core/src/session.rs`) already
//! tracks a `tool_error` step repeating with the identical `(tool, reason)`
//! and prefixes the observation from the second occurrence on, but the model
//! can still keep sending the exact same failing call a third, fourth, ...
//! time before `CONTINUE_REPEAT_CAP` ever parks it. This trigger substitutes
//! a `think` call the SECOND time the model sends the SAME call that just
//! failed, so the model gets the reasoning subagent's answer in hand instead
//! of parking three identical errors deep.
//!
//! This is the end-to-end proof that the substitution actually drives a real
//! `think` call through `Session::execute_think` (`core/src/session.rs`),
//! which writes `CodingWorkspace::decision` (`common_after_tool`'s `think`
//! handling) from the mock thinker's scripted answer -- the unit tests in
//! `skills/src/coding/tests.rs` cover the `validate`-level substitution in
//! isolation, but cannot exercise `Session::with_thinker`/`execute_think` at
//! all.
//!
//! Also proves the correction to unit 23 part (b): a `think` step
//! (`is_think_action`, `core/src/session.rs`) does NOT clear
//! `HarnessState.last_tool_error` or reset `tool_error_repeat_count` -- a
//! think between two identical errors leaves the streak exactly as it was,
//! so an error/think/error/think/... cycle still reaches
//! `CONTINUE_REPEAT_CAP` and parks (at most two thinks, three errors),
//! instead of resetting on every think and running forever.
//!
//! Fixture style copied from `skills/tests/skip_think.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`, plus a `MockThinker` attached via `Session::with_thinker`.
//!
//! Deliberately not `mod common;`, for the same reason `skip_think.rs`
//! inlines its own `config`/`make_file_tools_session`: `skills/tests/
//! common/mod.rs` exports items this file has no use for, and `-D warnings`
//! fails a binary that leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
    WorkingSetConfig,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, REPEATED_ERROR_QUESTION};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `skip_think.rs`'s `config`.
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

/// Same shape as `skip_think.rs`'s `ArcModel`.
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

/// Same shape as `skip_think.rs`'s `make_file_tools_session`.
fn make_file_tools_session(
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
    .with_thinker(thinker)
    .with_working_set(WorkingSetConfig::default());
    (session, log)
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// Whether the given event has been logged as a System entry anywhere in the
/// session so far.
fn any_event(log: &Arc<dyn SessionLog>, event: &str) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some(event))
}

/// An `edit_file` turn with the given `old`/`new` text against `path`.
fn edit_turn(path: &str, old: &str, new: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "edit_file",
                "args": {"path": path, "old": old, "new": new},
            },
        })
        .to_string(),
    )
}

/// A turn that writes a file (a real edit, of a DIFFERENT shape than
/// `edit_file`, used to prove a different call after an error does not
/// fire).
fn write_file_turn(path: &str, content: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "write_file",
                "args": {"path": path, "content": content},
            },
        })
        .to_string(),
    )
}

/// A turn that sends a real (model-initiated, not harness-substituted)
/// `think` call.
fn think_turn(question: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "think", "args": {"question": question}},
        })
        .to_string(),
    )
}

/// Finds the most recent `Action` log entry's `name`/`args`. Same as
/// `skip_think.rs`'s `last_action`.
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

const THINKER_ANSWER: &str = "the anchor line moved; re-read a.txt and anchor on the new line 3";

/// The exact question text unit 23 part (b)'s brief specifies, with
/// `{tool}` filled as `edit_file` and `{reason}` filled from `reason` (the
/// exact string `edit_file`'s real "old text not found" error reported,
/// captured from `harness.last_tool_error.reason` rather than
/// reconstructed, since the tool's exact wording -- including its
/// best-effort "closest match" hint -- is an input to this assertion, not
/// the thing under test) -- pinned here so a change to either
/// `REPEATED_ERROR_QUESTION` or the substitution's fill-in breaks a visible
/// assertion, not just a `contains` check.
fn expected_question(reason: &str) -> String {
    format!(
        "Your last `edit_file` call failed with: {reason}. Repeating the same call will fail \
         the same way. Using the working set above, state a different exact next action: for an \
         edit, the path, the old text as it appears in the file, and the new text; otherwise the \
         file or range to read and why."
    )
}

/// Main scenario: an `edit_file` with old text not present in the file (a
/// `tool_error`, `HarnessState.last_tool_error` becomes `Some(count 1)`),
/// then the SAME call again -- this must be substituted for `think` with
/// `REPEATED_ERROR_QUESTION` filled exactly, the thinker called exactly
/// once, `decision` carries the answer, and no park.
#[tokio::test]
async fn second_identical_tool_error_substitutes_think() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        // Step 1: `edit_file` with `old` text that is not actually in the
        // file -- a `tool_error` (`ExecutionFailed`), `last_tool_error`
        // becomes `Some(count 1)`.
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 2: the EXACT SAME call again -- must be substituted for
        // `think` with `REPEATED_ERROR_QUESTION`.
        edit_turn("a.txt", "line not present", "replacement"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: the failing edit -- an ordinary tool_error, no substitution
    // yet (core's own guard only prefixes from the SECOND occurrence).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    assert!(
        any_event(&log, "tool_error"),
        "the bad old-text edit must report a tool_error"
    );
    let (name1, args1) = last_action(&log);
    assert_eq!(name1, "edit_file");
    let (state1, _v) = active.session().state().await.unwrap();
    let err1 = state1
        .harness
        .last_tool_error
        .clone()
        .expect("a tool_error step must set harness.last_tool_error");
    assert_eq!(err1.tool, "edit_file");
    assert_eq!(
        err1.args, args1,
        "the recorded args must be the failing call's own args"
    );
    assert_eq!(err1.count, 1, "the first error must record count 1");
    assert!(
        err1.reason
            .starts_with("tool execution failed: old text not found in a.txt"),
        "got reason: {}",
        err1.reason
    );

    // Step 2: the SAME call again -- must be substituted for `think` with
    // REPEATED_ERROR_QUESTION; the thinker must have been called exactly
    // once; `decision` must carry the question and the scripted answer; the
    // run must NOT park.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the second identical error must NOT park: got {outcome2:?}"
    );
    assert!(!any_rejected(&log));

    let (name2, args2) = last_action(&log);
    assert_eq!(
        name2, "think",
        "the second identical edit_file error must be substituted for think"
    );
    let expected = expected_question(&err1.reason);
    assert_eq!(args2, json!({"question": expected}));
    assert_eq!(
        expected.as_str(),
        REPEATED_ERROR_QUESTION
            .replace("{tool}", "edit_file")
            .replace("{reason}", &err1.reason)
    );

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, expected);

    let (state2, _v) = active.session().state().await.unwrap();
    let decision = state2
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, expected);
    assert_eq!(decision.answer, THINKER_ANSWER);
}

/// Second test: a DIFFERENT call after an error must not fire -- `last_tool_
/// error`'s `(tool, args)` no longer matches the model's next call, so it
/// commits as an ordinary `write_file`, not `think`, and the thinker is
/// never called.
#[tokio::test]
async fn a_different_call_after_an_error_does_not_fire() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        // Step 1: the failing edit -- tool_error, last_tool_error set.
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 2: a DIFFERENT call (write_file) -- must NOT be substituted.
        write_file_turn("a.txt", "brand new contents\n"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();
    let (state1, _v) = active.session().state().await.unwrap();
    assert!(
        state1.harness.last_tool_error.is_some(),
        "the failing edit must set last_tool_error"
    );

    // Step 2: a different call (write_file) -- must commit as an ordinary
    // write_file, not think, and the thinker must never be called.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    assert!(!any_rejected(&log));

    let (name2, args2) = last_action(&log);
    assert_eq!(
        name2, "write_file",
        "a different call after an error must commit as write_file, not be substituted"
    );
    assert_eq!(
        args2.get("path").and_then(Value::as_str),
        Some("a.txt"),
        "got {args2:?}"
    );
    assert_eq!(
        thinker.calls().len(),
        0,
        "the mock thinker must never have been called"
    );
}

/// Third test: the same failing call, then a real `think` call (not a
/// harness substitution) right after the first error -- this answers the
/// model's question, but, being a `think` step (`is_think_action`,
/// `core/src/session.rs`, unit 23's correction), does NOT break the
/// repeated-tool-error streak: `HarnessState.last_tool_error` stays exactly
/// as it was (`Some(count 1)`), the same non-reset rule `last_skip` now
/// follows. The SAME failing call then repeats: since `last_tool_error` was
/// NOT cleared, `implement_repeated_error_thinks`'s `last_think_step` gate
/// (`think_step >= err.step`) suppresses the trigger (a think already ran
/// since this exact error), so it commits as an ordinary `tool_error`,
/// advancing the SAME streak to count 2 -- proving a think does not reset
/// the streak that was already open before it ran, only intervenes once per
/// streak the way it always did.
#[tokio::test]
async fn a_think_since_the_error_does_not_clear_last_tool_error_so_the_streak_continues() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        // Step 1: the failing edit -- tool_error, last_tool_error set,
        // count 1.
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 2: the model's OWN real `think` call (not a substitution) --
        // sets last_think_step to this step, and (being a think step) does
        // NOT clear last_tool_error: it stays Some(count 1).
        think_turn("why did that edit fail?"),
        // Step 3: the SAME failing call again -- last_tool_error is still
        // Some(count 1) from step 1, and last_think_step (step 2) is AFTER
        // that error's own step, so implement_repeated_error_thinks's
        // last_think_step gate suppresses the trigger; this commits as an
        // ordinary edit_file tool_error, advancing the SAME streak to
        // count 2 (the observation gets the "same error as previous step"
        // prefix).
        edit_turn("a.txt", "line not present", "replacement"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer("manual think answer"),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: the failing edit.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();
    let (state1, _v) = active.session().state().await.unwrap();
    let err1 = state1
        .harness
        .last_tool_error
        .clone()
        .expect("step 1 must set last_tool_error");
    assert_eq!(err1.count, 1);

    // Step 2: the model's OWN real think call.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (name2, args2) = last_action(&log);
    assert_eq!(name2, "think", "step 2 must be the model's own think call");
    assert_eq!(
        args2.get("question").and_then(Value::as_str),
        Some("why did that edit fail?"),
        "step 2's think must be the model's own question, not a substitution"
    );
    let (state2, _v) = active.session().state().await.unwrap();
    assert!(
        state2.harness.last_think_step.is_some(),
        "the model's own think call must set last_think_step"
    );
    assert_eq!(
        state2.harness.last_tool_error,
        Some(err1.clone()),
        "a think step must NOT clear last_tool_error -- it stays exactly as it was, the same \
         non-reset rule last_skip now follows, or the model could alternate error/think forever \
         without ever reaching CONTINUE_REPEAT_CAP"
    );

    // Step 3: the SAME failing edit_file call again -- the trigger is
    // suppressed (a think already ran since err1's own step), so this
    // commits as an ordinary tool_error, advancing the SAME streak.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (name3, _args3) = last_action(&log);
    assert_eq!(
        name3, "edit_file",
        "step 3 must commit as an ordinary edit_file error, not a think substitution, since a \
         think already ran since err1's own step (the last_think_step gate suppresses a second \
         substitution for the same error)"
    );
    let (state3, _v) = active.session().state().await.unwrap();
    let err3 = state3
        .harness
        .last_tool_error
        .clone()
        .expect("step 3 must set last_tool_error");
    assert_eq!(
        err3.count, 2,
        "step 3 must advance the SAME streak to count 2, not start a fresh one, proving the \
         think at step 2 did not reset it"
    );
    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once, for the model's own think at \
         step 2 -- the trigger never fires in this trace"
    );
}

/// Fourth test (the bounded-sequence test the brief requires): the SAME
/// failing call re-issued after every step, with a `MockThinker` attached,
/// must produce EXACTLY two `think` actions and park at the third error
/// with `non_progress_cap` logged -- proving the harness now bounds the
/// error/think/error/think/... cycle instead of letting it run forever
/// (unit 23's correction: a think no longer resets `last_tool_error`/
/// `tool_error_repeat_count`, so the SAME streak survives across the
/// substituted thinks and still reaches `CONTINUE_REPEAT_CAP`).
///
/// Trace: error (step 1, count 1) -> same call substituted for think (step
/// 2, err.step=1 has no think yet) -> same call errors again (step 3, SAME
/// streak now count 2, since the think at step 2 did not clear it; the
/// trigger was suppressed because last_think_step=2 >= err.step=1) -> same
/// call substituted for think again (step 4, err.step is now 3, newer than
/// last_think_step=2, so the trigger fires again) -> same call errors again
/// (step 5, SAME streak now count 3 = CONTINUE_REPEAT_CAP, since
/// last_think_step=4 >= err.step=3 suppressed the trigger) -> parks via
/// `park_non_progress_cap` with a `non_progress_cap` System entry. Exactly
/// two thinks (steps 2 and 4), exactly three tool_errors (steps 1, 3, 5).
#[tokio::test]
async fn error_think_error_think_error_parks_at_the_cap_with_exactly_two_thinks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        edit_turn("a.txt", "line not present", "replacement"),
        edit_turn("a.txt", "line not present", "replacement"),
        edit_turn("a.txt", "line not present", "replacement"),
        edit_turn("a.txt", "line not present", "replacement"),
        edit_turn("a.txt", "line not present", "replacement"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![
        Ok(ThinkOutput::answer("first think answer")),
        Ok(ThinkOutput::answer("second think answer")),
    ]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: error, count 1.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (name1, _args1) = last_action(&log);
    assert_eq!(name1, "edit_file");
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.harness.last_tool_error.as_ref().map(|e| e.count),
        Some(1)
    );

    // Step 2: substituted think (first of exactly two).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (name2, _args2) = last_action(&log);
    assert_eq!(name2, "think", "step 2 must be the first substituted think");
    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.harness.last_tool_error.as_ref().map(|e| e.count),
        Some(1),
        "the think must not reset the streak: count stays 1"
    );

    // Step 3: same call errors again -- SAME streak, count 2 (not a fresh
    // streak), observation gets the "same error as previous step" prefix.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (name3, _args3) = last_action(&log);
    assert_eq!(
        name3, "edit_file",
        "step 3 must commit as an ordinary error, not a think substitution (a think already \
         ran since err.step=1)"
    );
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state3.harness.last_tool_error.as_ref().map(|e| e.count),
        Some(2),
        "step 3 must advance the SAME streak to count 2, proving the think at step 2 did not \
         reset it"
    );
    // Step 4: substituted think again (second of exactly two) -- the
    // trigger fires because err.step (3) is newer than last_think_step (2).
    // Run this step first so `model.prompts()` has the prompt built after
    // step 3's error available at index 3 (`prompts[i]` is the prompt fed
    // to step `i + 1`).
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, _args4) = last_action(&log);
    assert_eq!(
        name4, "think",
        "step 4 must be the second substituted think (the error step is newer than the think \
         step)"
    );
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.last_tool_error.as_ref().map(|e| e.count),
        Some(2),
        "the second think must not reset the streak either: count stays 2"
    );

    // `prompts[i]` is the prompt fed to step `i + 1`: `prompts[1]` is built
    // after step 1's FIRST error (count 1, no prefix yet), `prompts[3]` is
    // built after step 3's SECOND occurrence of the streak (count 2) and
    // must carry the repeated-tool-error prefix line verbatim.
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 4,
        "expected at least 4 prompts (one per step run so far): {}",
        prompts.len()
    );
    assert!(
        !prompts[1]
            .user
            .contains("This is the same `edit_file` error"),
        "prompts[1] (built after step 1, the FIRST error) must not carry the repeated-error \
         prefix yet:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[3].user.contains(
            "[harness] This is the same `edit_file` error as the previous step; repeating the \
             call will not change it."
        ),
        "prompts[3] (built after step 3, the SECOND occurrence of the streak) must carry the \
         repeated-tool-error prefix line verbatim:\n{}",
        prompts[3].user
    );

    // Step 5: same call errors a third time -- SAME streak reaches count 3
    // = CONTINUE_REPEAT_CAP, parks via park_non_progress_cap with a
    // non_progress_cap System entry logged.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let has_non_progress_cap = rec5.entries().iter().any(|e| {
        e.kind == LogKind::System
            && e.payload.get("event").and_then(Value::as_str) == Some("non_progress_cap")
    });
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(
        matches!(outcome5, Outcome::Parked(_)),
        "the third error of the streak must park: got {outcome5:?}"
    );
    assert!(
        has_non_progress_cap,
        "the park must log a non_progress_cap System entry"
    );

    assert_eq!(
        thinker.calls().len(),
        2,
        "exactly two think actions must have run across the whole bounded sequence"
    );
    let err1_reason = &state1
        .harness
        .last_tool_error
        .as_ref()
        .expect("step 1 must set last_tool_error")
        .reason;
    let err3_reason = &state3
        .harness
        .last_tool_error
        .as_ref()
        .expect("step 3 must set last_tool_error")
        .reason;
    assert_eq!(thinker.calls()[0].question, expected_question(err1_reason));
    assert_eq!(thinker.calls()[1].question, expected_question(err3_reason));
}
