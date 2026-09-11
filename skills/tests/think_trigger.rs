//! `implement_repeat_failure_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `strip_malformed_next`): on a
//! bench trap fixture the acting model was unanimous and wrong -- every
//! edit it tried addressed the wrong function, the test failed identically
//! after each one, and the model never called `think` on its own. So the
//! harness substitutes a `think` call for the model's next edit right after
//! a `test` call's failure has recurred identically, once per failure
//! streak.
//!
//! This is the end-to-end proof that the substitution actually drives a
//! real `think` call through `Session::execute_think`
//! (`core/src/session.rs`), which writes `CodingWorkspace::decision`
//! (`common_after_tool`'s `think` handling) from the mock thinker's scripted
//! answer -- the unit tests in `skills/src/coding/tests.rs` cover the
//! `validate`-level substitution in isolation, but cannot exercise
//! `Session::with_thinker`/`execute_think` at all.
//!
//! Fixture style copied from `skills/tests/no_green_runs_test.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`, plus a `MockThinker` attached via `Session::with_thinker`.
//!
//! Deliberately not `mod common;`, for the same reason `no_green_runs_test.rs`
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
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, REPEAT_FAILURE_QUESTION};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `no_green_runs_test.rs`'s `config`.
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

/// Same shape as `no_green_runs_test.rs`'s `ArcModel`.
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

/// Same shape as `no_green_runs_test.rs`'s `make_file_tools_session`, but
/// also attaches `thinker` via `Session::with_thinker`.
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
    .with_thinker(thinker);
    (session, log)
}

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
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

/// A turn that writes a file (a real edit, so the mutated flag is set and
/// `edits_since_test` climbs).
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

/// Finds the most recent `Action` log entry's `name`/`args`. Same as
/// `no_green_runs_test.rs`'s `last_action`.
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

const THINKER_ANSWER: &str = "the bug is in tax.rs: use 5000 not 500";

#[tokio::test]
async fn repeat_failure_substitutes_think_once_then_lets_the_next_edit_through() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    // The failing command prints an assertion-shaped summary (rather than
    // a bare nonzero exit) so `classify_failure` classifies it as
    // `FailureKind::Assertion` and the substituted `think` call still
    // carries `REPEAT_FAILURE_QUESTION`, same as before `classify_failure`
    // existed (BACKLOG.md item 18). Each occurrence is preceded by a
    // `write_file` (mutated_since_verify), so the redundant-verification
    // guard (7.7, `core/src/session.rs`) never skips the repeat here even
    // though the command string is identical every time.
    const ASSERTION_CMD: &str = "printf 'assertion failed: left: 1 right: 2\\n'; exit 1";

    let turns = vec![
        // Step 1: fail (fresh failure).
        test_turn(ASSERTION_CMD),
        // Step 2: a real write (mutated flag set).
        write_file_turn("a.txt", "still wrong\n"),
        // Step 3: fail again, identical -- repeated_failure_step gets set
        // to this step.
        test_turn(ASSERTION_CMD),
        // Step 4: the model tries another edit -- must be substituted for
        // `think`.
        write_file_turn("a.txt", "second attempt\n"),
        // Step 5: the model re-runs the SAME failing test with nothing
        // edited since step 4's think call (edits_since_test is still 0 --
        // a `think` call does not count as an edit) -- the identical
        // failure bumps repeated_failure_step again, to this step. This is
        // the idle-test shape, and it must NOT be substituted: the
        // edit-since-last-think gate (added after a second live battery
        // showed the ungated version fire on every stuck repeat, calling
        // the reasoning subagent five times running with nothing new to
        // say -- all five came back empty, see `ModelThinker::think`'s
        // `done_reason: "length"` handling) blocks it because `seen` has no
        // edit recorded after the think call at step 4.
        test_turn(ASSERTION_CMD),
        // Step 6: a real edit -- this IS new information for the reasoning
        // subagent, so it re-arms the trigger for the next stuck action.
        write_file_turn("a.txt", "fourth attempt\n"),
        // Step 7: fail again, identical to steps 3/5 -- a fresh
        // repeated_failure_step, with an edit (step 6) now recorded after
        // the last think call (step 4).
        test_turn(ASSERTION_CMD),
        // Step 8: the model tries another edit -- must be substituted for
        // `think` AGAIN: the edit at step 6 happened after the last think
        // call (step 4), so the trigger re-arms. The mock thinker must have
        // been called exactly twice in total by here.
        write_file_turn("a.txt", "fifth attempt\n"),
        // Step 9: a passing test -- clears decision and
        // repeated_failure_step.
        test_turn("true"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![
        Ok(ThinkOutput::answer(THINKER_ANSWER)),
        Ok(ThinkOutput::answer(THINKER_ANSWER)),
    ]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: fail (fresh).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.repeated_failure_step, None,
        "the first fail must not set repeated_failure_step"
    );

    // Step 2: write_file (real edit).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (name2, _args2) = last_action(&log);
    assert_eq!(name2, "write_file");

    // Step 3: fail again, identical -- repeated_failure_step must be set.
    // (Not asserted equal to `state.harness.step` here: a harness-authored
    // `after_tool` patch -- e.g. `CodingWorkspace::tests`, BACKLOG.md item
    // 5 -- commits as its own extra step via `apply_after_tool_patch`, so
    // the harness step counter can already have moved past the step that
    // set `repeated_failure_step` by the time state is read back.)
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state, _v) = active.session().state().await.unwrap();
    // The failing `test` step sets it to its own number; the harness then
    // commits one more version for the `tests` record `common_after_tool`
    // writes on every `test` call (BACKLOG.md item 5), so the counter read
    // back here is exactly one past it.
    let repeat_step = state
        .harness
        .repeated_failure_step
        .expect("the second identical fail must set repeated_failure_step");
    assert_eq!(repeat_step + 1, state.harness.step);
    assert!(!any_rejected(&log));

    // Step 4: the model sends another write_file -- must be substituted for
    // `think` with REPEAT_FAILURE_QUESTION; the mock thinker must have been
    // called exactly once; `decision` must carry the question and the
    // scripted answer.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    assert!(!any_rejected(&log));

    let (name4, args4) = last_action(&log);
    assert_eq!(
        name4, "think",
        "step 4's write_file must be substituted for think"
    );
    assert_eq!(args4, json!({"question": REPEAT_FAILURE_QUESTION}));

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, REPEAT_FAILURE_QUESTION);

    let (state, _v) = active.session().state().await.unwrap();
    let decision = state
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, REPEAT_FAILURE_QUESTION);
    assert_eq!(decision.answer, THINKER_ANSWER);

    // Step 5: the model re-runs the same failing test with nothing edited
    // since the think call at step 4 (edits_since_test is still 0) -- the
    // identical failure bumps repeated_failure_step again, but this idle
    // test rerun must NOT be substituted: no edit has been recorded in
    // `seen` since the last think call (step 4), so the trigger stays
    // dormant instead of calling the reasoning subagent a second time with
    // nothing new to tell it.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");
    assert!(!any_rejected(&log));

    let (name5, _args5) = last_action(&log);
    assert_eq!(
        name5, "test",
        "the idle test rerun with no edit since the last think call must commit as test, not be \
         substituted"
    );
    assert_eq!(
        thinker.calls().len(),
        1,
        "the thinker must still have been called exactly once: no edit since step 4's think call"
    );

    // Step 6: a real edit -- new information for the reasoning subagent,
    // recorded in `seen` after the last think call (step 4).
    let rec6 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome6 = log.append_record(rec6).unwrap();
    assert!(matches!(outcome6, Outcome::Continue), "got {outcome6:?}");
    let (name6, _args6) = last_action(&log);
    assert_eq!(name6, "write_file");

    // Step 7: fail again, identical -- a fresh repeated_failure_step, now
    // with an edit (step 6) after the last think call.
    let rec7 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome7 = log.append_record(rec7).unwrap();
    assert!(matches!(outcome7, Outcome::Continue), "got {outcome7:?}");
    assert!(!any_rejected(&log));

    // Step 8: the model tries another edit -- must be substituted for
    // `think` AGAIN: the edit at step 6 happened after the last think call
    // (step 4), so the trigger re-arms. The mock thinker must now have been
    // called exactly twice in total.
    let rec8 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome8 = log.append_record(rec8).unwrap();
    assert!(matches!(outcome8, Outcome::Continue), "got {outcome8:?}");
    assert!(!any_rejected(&log));

    let (name8, args8) = last_action(&log);
    assert_eq!(
        name8, "think",
        "step 8's write_file must be substituted for think again: an edit happened since the \
         last think call"
    );
    assert_eq!(args8, json!({"question": REPEAT_FAILURE_QUESTION}));
    assert_eq!(
        thinker.calls().len(),
        2,
        "the mock thinker must have been called exactly twice in total"
    );

    // Step 9: a passing test -- must clear both decision and
    // repeated_failure_step.
    let rec9 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome9 = log.append_record(rec9).unwrap();
    assert!(matches!(outcome9, Outcome::Continue), "got {outcome9:?}");
    assert!(!any_rejected(&log));

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.decision, None,
        "a passing test must clear decision"
    );
    assert_eq!(
        state.harness.repeated_failure_step, None,
        "a passing test must clear repeated_failure_step"
    );
}

/// The second stuck shape a first live battery showed the original
/// (edit-only) trigger miss entirely: on a trap fixture, the model re-ran
/// the failing test five times running with nothing edited in between --
/// the trigger, keyed on the next `edit_file`/`write_file`, never fired.
/// `implement_repeat_failure_thinks` now also fires on a `test` call made
/// with `edits_since_test == 0` once a repeat is open, the same as it does
/// for an edit.
#[tokio::test]
async fn repeat_failure_substitutes_think_for_an_idle_test_rerun() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    // Each `test` call below uses a DIFFERENT command (the redundant-
    // verification guard, 7.7 in `Active::execute`/`core/src/session.rs`,
    // matches on the exact `cmd` string and would skip a truly identical
    // repeat before it ever reached `set_harness`'s `test` branch), but
    // every one prints the SAME assertion-shaped stdout and exits non-zero,
    // so `normalize_failure_summary` treats them as an identical repeat
    // (classifying as `FailureKind::Assertion`, so the substituted `think`
    // call still carries `REPEAT_FAILURE_QUESTION`, same as before
    // `classify_failure` existed).
    const ASSERTION_CMD_1: &str = "printf 'assertion failed: left: 1 right: 2\\n'; exit 1";
    const ASSERTION_CMD_2: &str = "printf 'assertion failed: left: 1 right: 2\\n'; false; exit 1";
    const ASSERTION_CMD_3: &str = "printf 'assertion failed: left: 1 right: 2\\n'; exit 1; true";
    const ASSERTION_CMD_4: &str =
        "printf 'assertion failed: left: 1 right: 2\\n'; exit 1; true; exit 1";
    let turns = vec![
        // Step 1: fail (fresh failure).
        test_turn(ASSERTION_CMD_1),
        // Step 2: fail again, same shape, with nothing edited in between --
        // repeated_failure_step gets set to this step.
        test_turn(ASSERTION_CMD_2),
        // Step 3: the model reruns the test again with nothing edited --
        // must be substituted for `think` (edits_since_test == 0).
        test_turn(ASSERTION_CMD_3),
        // Step 4: the model runs the test a fourth time -- must NOT be
        // substituted (a `think` already ran since the repeat).
        test_turn(ASSERTION_CMD_4),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: fail (fresh).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.repeated_failure_step, None,
        "the first fail must not set repeated_failure_step"
    );

    // Step 2: fail again, identical, nothing edited -- repeated_failure_step
    // must be set. (Not asserted equal to `state.harness.step` here: see
    // the comment on the analogous assertion in `repeat_failure_
    // substitutes_think_once_then_lets_the_next_edit_through`, above.)
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (state, _v) = active.session().state().await.unwrap();
    // The failing `test` step sets it to its own number; the harness then
    // commits one more version for the `tests` record `common_after_tool`
    // writes on every `test` call (BACKLOG.md item 5), so the counter read
    // back here is exactly one past it.
    let repeat_step = state
        .harness
        .repeated_failure_step
        .expect("the second identical fail must set repeated_failure_step");
    assert_eq!(repeat_step + 1, state.harness.step);
    assert_eq!(state.harness.edits_since_test, 0);
    assert!(!any_rejected(&log));

    // Step 3: the model tries the same test again with nothing edited --
    // must be substituted for `think` with REPEAT_FAILURE_QUESTION; the
    // mock thinker must have been called exactly once; `decision` must
    // carry the question and the scripted answer.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    assert!(!any_rejected(&log));

    let (name3, args3) = last_action(&log);
    assert_eq!(
        name3, "think",
        "step 3's idle test rerun must be substituted for think"
    );
    assert_eq!(args3, json!({"question": REPEAT_FAILURE_QUESTION}));

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, REPEAT_FAILURE_QUESTION);

    let (state, _v) = active.session().state().await.unwrap();
    let decision = state
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, REPEAT_FAILURE_QUESTION);
    assert_eq!(decision.answer, THINKER_ANSWER);

    // Step 4: the model tries the test again -- must NOT be substituted
    // this time (a think call has already run since the repeat).
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    assert!(!any_rejected(&log));

    let (name4, _args4) = last_action(&log);
    assert_eq!(
        name4, "test",
        "the fourth test call must commit as test, not be substituted again"
    );
    assert_eq!(
        thinker.calls().len(),
        1,
        "the thinker must still have been called exactly once"
    );
}

/// DECISIONS.md D9 / BACKLOG.md item 18: the repeated-failure question is
/// shaped to the kind of failure instead of asking every repeat the same
/// arithmetic-trace question written for a rounding-trap assertion. On a
/// Rust compile-error failure (the exact shape a calibration run showed
/// costing four 263-second `think` calls, three of them empty), the
/// substituted `think` call must carry the compile-error question, not
/// `REPEAT_FAILURE_QUESTION`.
#[tokio::test]
async fn repeat_failure_substitutes_the_compile_error_question_for_a_compile_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    // A shell command that prints a rustc-style compile error to stdout and
    // exits non-zero, so `tools/src/test_tool.rs`'s `failure_summary` picks
    // up the `error[E0308]` line (and the `--> ...` line right after it, via
    // `is_rustc_error_line`'s context-pull), and `core/src/session.rs`'s
    // `normalize_failure_summary` carries it into `HarnessState::
    // last_failure_summary`.
    const COMPILE_ERROR_CMD: &str = "printf 'error[E0308]: mismatched types\\n --> \
         src/lib.rs:3:5\\n'; exit 1";

    let turns = vec![
        // Step 1: fail (fresh compile-error failure).
        test_turn(COMPILE_ERROR_CMD),
        // Step 2: a real write (mutated flag set).
        write_file_turn("a.txt", "still wrong\n"),
        // Step 3: fail again, identical -- repeated_failure_step gets set.
        test_turn(COMPILE_ERROR_CMD),
        // Step 4: the model tries another edit -- must be substituted for
        // `think` with the compile-error question.
        write_file_turn("a.txt", "second attempt\n"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: fail (fresh).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Step 2: write_file (real edit).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    // Step 3: fail again, identical -- repeated_failure_step must be set,
    // one step behind the harness step counter (see the analogous comment
    // in `repeat_failure_substitutes_think_once_then_lets_the_next_edit_
    // through`, above).
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state, _v) = active.session().state().await.unwrap();
    let repeat_step = state
        .harness
        .repeated_failure_step
        .expect("the second identical compile-error fail must set repeated_failure_step");
    assert_eq!(repeat_step + 1, state.harness.step);
    assert!(!any_rejected(&log));

    // Step 4: the model sends another write_file -- must be substituted for
    // `think` with the compile-error question, not REPEAT_FAILURE_QUESTION.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    assert!(!any_rejected(&log));

    let (name4, args4) = last_action(&log);
    assert_eq!(
        name4, "think",
        "step 4's write_file must be substituted for think"
    );
    let question = args4
        .get("question")
        .and_then(Value::as_str)
        .expect("think args must carry a question");
    assert!(
        question.contains("Read the first error only"),
        "expected the compile-error question, got: {question}"
    );
    assert_ne!(
        question, REPEAT_FAILURE_QUESTION,
        "a compile-error failure must not get the arithmetic-trace question"
    );

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, question);
}
