//! `implement_early_park_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `implement_repeat_failure_thinks`):
//! DECISIONS.md D4's park-rate study over 96 bench runs found 6 of 9 parks
//! happen before model call 15, most of them the model sending an empty or
//! non-question `ask_user` three times running until `CONTINUE_REPEAT_CAP`
//! (`core/src/session.rs`) parks the run -- with no test failure ever open,
//! so `implement_repeat_failure_thinks` never sees it. This trigger catches
//! the SECOND consecutive non-question `ask_user` and substitutes a `think`
//! call instead, one strike before the cap would otherwise park the run on
//! the third.
//!
//! This is the end-to-end proof that the substitution actually drives a
//! real `think` call through `Session::execute_think`
//! (`core/src/session.rs`), and that `HarnessState::consecutive_non_question_
//! asks` (`core/src/version.rs`) is actually threaded through the session's
//! commit path -- the unit tests in `skills/src/coding/tests.rs` cover the
//! `validate`-level substitution against a hand-built `HarnessState`, but
//! cannot exercise the counter's own increment/reset logic in
//! `core/src/session.rs`'s `step`.
//!
//! Fixture style copied from `skills/tests/think_trigger.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`, plus a `MockThinker` attached via `Session::with_thinker`.
//!
//! Deliberately not `mod common;`, for the same reason `think_trigger.rs`
//! inlines its own `config`/`make_file_tools_session`: `skills/tests/
//! common/mod.rs` exports items this file has no use for, and `-D warnings`
//! fails a binary that leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `think_trigger.rs`'s `config`.
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

/// Same shape as `think_trigger.rs`'s `ArcModel`.
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

/// Same shape as `think_trigger.rs`'s `make_file_tools_session`, but takes
/// an `Option<Arc<dyn Thinker>>` so the no-thinker-configured test can omit
/// it entirely (mirroring how a real session with no `thinker:` config
/// section behaves).
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    thinker: Option<Arc<dyn statie_core::Thinker>>,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tools = default_tools(&ToolConfig::default());
    let mut session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    if let Some(thinker) = thinker {
        session = session.with_thinker(thinker);
    }
    (session, log)
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// Whether any `think` System entry has been logged.
fn any_think_event(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("think"))
}

/// A turn that reads a file (a real tool call, so `seen` gets an entry and
/// `consecutive_non_question_asks` resets to 0).
fn read_file_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// A turn that sends `ask_user` with the given `message` and an empty
/// patch.
fn ask_user_turn(message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

/// Finds the most recent `Action` log entry's kind: for `Action::Tool` (the
/// `Action` enum's `#[serde(tag = "type", rename_all = "snake_case")]`
/// gives `"type": "tool"`), returns `("tool_name", args)`; for any other
/// variant (`"continue"`, `"ask_user"`, `"done"`, ...), returns
/// `(that_type_string, Value::Null)`. Same as `think_trigger.rs`'s
/// `last_action`, generalised: that file's turns are all `Action::Tool`, so
/// it can assume a `name` field always exists, but this file's turns also
/// commit `Action::Continue` (an `ask_user` the skill converts), which has
/// no `name` at all.
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

/// The exact question `implement_early_park_thinks` substitutes.
const EARLY_PARK_QUESTION: &str = "You sent ask_user twice with nothing to ask. Given the task, \
     the plan, and the last tool output, what is the next concrete tool call and why? Name the \
     tool, its arguments, and what you expect to see.";

const THINKER_ANSWER: &str = "read the plan step first, then edit_file src/tax.rs to fix the rate";

/// Model turns: one `read_file` (a real tool call, no effect on the
/// streak), then `ask_user ""` twice. The SECOND non-question `ask_user`
/// must be substituted for `think` with `EARLY_PARK_QUESTION`: the run
/// must not park, the mock thinker must have been called exactly once with
/// that question, the session log must carry a `think` event, and the next
/// prompt's observation must carry the substitution note.
#[tokio::test]
async fn second_consecutive_empty_ask_user_becomes_think_and_the_run_does_not_park() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let turns = vec![
        // Step 1: a real tool call -- consecutive_non_question_asks stays 0.
        read_file_turn("a.txt"),
        // Step 2: first empty ask_user -- converted to Continue by
        // `ask_user_empty_note`; consecutive_non_question_asks becomes 1
        // for the NEXT step (set from this step's raw action before
        // `validate` runs, committed alongside this step).
        ask_user_turn(""),
        // Step 3: second empty ask_user in a row -- must be substituted for
        // `think` instead of converting to Continue again (which would be
        // the second occurrence of an identical note, one short of the
        // repeat cap's third-strike park).
        ask_user_turn(""),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) = make_file_tools_session(
        Arc::clone(&model),
        config(&root),
        Some(Arc::clone(&thinker) as _),
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: read_file (real tool call).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.consecutive_non_question_asks, 0,
        "a real tool call must not start (or extend) the streak"
    );

    // Step 2: first empty ask_user -- converts to Continue with a note, not
    // think (this is the FIRST occurrence, not the second).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    assert!(!any_rejected(&log));
    let (name2, _args2) = last_action(&log);
    assert_eq!(
        name2, "continue",
        "the first empty ask_user must convert to Continue, not think"
    );
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.consecutive_non_question_asks, 1,
        "the first empty ask_user must set the streak to 1 for the next step"
    );
    assert_eq!(
        thinker.calls().len(),
        0,
        "the thinker must not have been called yet"
    );

    // Step 3: second empty ask_user in a row -- must be substituted for
    // `think` with EARLY_PARK_QUESTION.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; the run must not park -- the think substitution keeps it going"
    );
    assert!(!any_rejected(&log));

    let (name3, args3) = last_action(&log);
    assert_eq!(
        name3, "think",
        "the second consecutive empty ask_user must be substituted for think"
    );
    assert_eq!(args3, json!({"question": EARLY_PARK_QUESTION}));

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, EARLY_PARK_QUESTION);

    assert!(
        any_think_event(&log),
        "a `think` System event must be logged"
    );

    let (state, _v) = active.session().state().await.unwrap();
    let decision = state
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, EARLY_PARK_QUESTION);
    assert_eq!(decision.answer, THINKER_ANSWER);

    // The substituted think call's own commit sets last_think_step, so
    // (per the "once per streak" rule) the trigger cannot fire again until
    // a real tool call lands. `state.harness.step` is the counter for the
    // NEXT step to run, one ahead of the step just committed, so
    // `last_think_step` is one behind it, not equal to it.
    assert_eq!(state.harness.last_think_step, Some(state.harness.step - 1));
}

/// Without a thinker configured (the default -- no `thinker:` config
/// section, mirrored here by passing `None` to `make_file_tools_session`),
/// the trigger is thinker-agnostic exactly like `implement_repeat_failure_
/// thinks`: neither function can see whether a thinker is configured at
/// all (`Skill::validate` only ever sees `CommittedState`, never session
/// config), so both always substitute `think` when their shape matches and
/// rely on `Session::execute_think`'s own graceful "`think` is not
/// configured: ... Decide from what you have." fallback -- a real,
/// successfully executed tool step either way, never a rejection or a
/// crash.
///
/// Concretely, with the model sending the SAME non-question `ask_user`
/// message every step and no thinker configured:
/// - Step 1: first occurrence -- `ask_user_statement_note` converts to
///   `Action::Continue`; `CONTINUE_REPEAT_CAP`'s streak count
///   (`continue_repeat_count`) is 1, `consecutive_continues` is 1.
/// - Step 2: `consecutive_non_question_asks` is now 1, so
///   `implement_early_park_thinks` fires and substitutes `think`. Unit 47's
///   core carve-out (`core/src/session.rs`, 3a) makes a committed `think`
///   step NEUTRAL for `consecutive_continues`/`continue_repeat_count`/
///   `non_progress_count` -- unlike before that carve-out existed, none of
///   the three resets here: all three stay exactly what they were after
///   step 1.
/// - Step 3: the "once per streak" gate (`last_think_step` set at step 2)
///   blocks a second substitution (`implement_early_park_thinks`'s own
///   `tool_call_since` check, AND unit 47's `refusal_thinks`, which never
///   even gets a chance to look -- `consecutive_continues` is already 1
///   going in, not the `== 1` this step would need after a real
///   `Continue`), so this falls through to the ordinary `ask_user_
///   statement_note` conversion again: `Continue`, with the SAME note text
///   as step 1 -- `continue_repeat_count` (preserved through step 2's
///   think, per the carve-out) advances from 1 to 2, `consecutive_
///   continues` from 1 to 2.
/// - Step 4: a third occurrence of the identical note -- `continue_repeat_
///   count` reaches 3 = `CONTINUE_REPEAT_CAP`, and the run parks.
///
/// So the run still safely reaches a park (a `Park::AskUser` carrying the
/// repeated-note message) -- nothing hangs, nothing is rejected, no
/// `think` call is ever left dangling -- one step EARLIER than before the
/// carve-out (step 4 instead of step 5): the think at step 2 no longer
/// dilutes the identical-note streak the way it used to, so only one more
/// occurrence of the note (step 3) is needed after it, not three fresh
/// ones.
#[tokio::test]
async fn non_question_asks_with_no_thinker_configured_still_park_eventually() {
    let root = tempfile::tempdir().unwrap();
    let message = "I will look at the code next";

    let turns = vec![
        ask_user_turn(message),
        ask_user_turn(message),
        ask_user_turn(message),
        ask_user_turn(message),
        ask_user_turn(message),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root), None);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: first non-question ask_user -- converts to Continue with the
    // "statement, not a question" note; not the early-park trigger yet
    // (consecutive_non_question_asks was 0 going in).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "step 1: got {outcome1:?}"
    );
    let (name1, _args1) = last_action(&log);
    assert_eq!(name1, "continue", "step 1: must not substitute think yet");
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(state.harness.consecutive_non_question_asks, 1);

    // Step 2: second in a row -- the early-park trigger fires and
    // substitutes `think`, even with no thinker configured.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "step 2: got {outcome2:?}"
    );
    let (name2, _args2) = last_action(&log);
    assert_eq!(
        name2, "think",
        "step 2: the second consecutive non-question ask_user must still be substituted for \
         think, configured or not"
    );
    assert!(
        any_think_event(&log),
        "step 2: a `think` System event must be logged even with no thinker configured"
    );
    assert!(
        !any_rejected(&log),
        "step 2: an unconfigured think must not be rejected"
    );

    // Step 3: the "once per streak" gate blocks a second substitution (no
    // tool call landed since step 2's think call), so this falls through to
    // the ordinary Continue conversion -- a plain `continue`, not `think`.
    // Unit 47's core carve-out (3a) means `continue_repeat_count` was
    // preserved through step 2's think (1, not reset to 0), so this step
    // advances it to 2, not back to a fresh 1.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "step 3: got {outcome3:?}"
    );
    let (name3, _args3) = last_action(&log);
    assert_eq!(
        name3, "continue",
        "step 3: once-per-streak must block a second think substitution"
    );
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state3.harness.consecutive_continues, 2,
        "step 3: the carve-out must have preserved consecutive_continues through step 2's think"
    );

    // Step 4: a THIRD occurrence of the identical note -- continue_repeat_
    // count reaches CONTINUE_REPEAT_CAP (3) and the run parks, one step
    // earlier than before the carve-out existed (step 4, not step 5): the
    // think at step 2 no longer dilutes the identical-note streak.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    match outcome4 {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("statement of what you will do next"),
                "park message should carry the repeated ask_user-statement note: {msg}"
            );
            assert!(
                msg.contains("3 times in a row"),
                "park message should say it repeated 3 times in a row: {msg}"
            );
        }
        other => panic!("step 4: expected Parked(AskUser(_)), got {other:?}"),
    }
    assert!(!any_rejected(&log));
}
