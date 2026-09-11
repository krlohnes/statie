//! `implement_no_green_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `implement_stall_thinks`): unit 27
//! ("no green cap"). `HarnessState::fails_since_green` (`core/src/
//! version.rs`) counts `test` tool runs whose result was a failure since the
//! last passing `test` run in this activation, incremented by core at the
//! same call site as `green_step`/`repeated_failure_step`
//! (`core/src/session.rs`). Once that count reaches `NO_GREEN_THINK_AT`
//! (12), with no `think` since the last failing `test`'s own step, this
//! trigger substitutes a `think` call asking whether the plan itself is
//! wrong -- unlike the other `think` triggers (each keyed on one specific
//! streak SHAPE recurring: an identical skip, an identical-shape tool
//! error, a stalled path), this one is keyed purely on the raw count of
//! failing `test` runs, so it fires on ANY non-`test`/`think` action once
//! the count is high enough, even a fresh edit that has never repeated
//! itself.
//!
//! This is the end-to-end proof that the substitution actually drives a real
//! `think` call through `Session::execute_think` (`core/src/session.rs`),
//! which writes `CodingWorkspace::decision` (`common_after_tool`'s `think`
//! handling) from the mock thinker's scripted answer -- the unit tests in
//! `skills/src/coding/tests.rs` cover the `validate`-level substitution in
//! isolation, but cannot exercise `Session::with_thinker`/`execute_think` at
//! all.
//!
//! Fixture style copied from `skills/tests/it/think_trigger.rs`/
//! `stall_think.rs`: a `Session<CodingWorkspace>` wired with the real file
//! tools, a scripted `MockModel`, and outcomes/log entries read straight
//! from the `SessionLog`, plus a `MockThinker` attached via
//! `Session::with_thinker`.
//!
//! Deliberately not `mod common;`, for the same reason `think_trigger.rs`/
//! `stall_think.rs` inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.
//!
//! Each failing `test` call in this fixture carries a DIFFERENT failure
//! summary, so `HarnessState::repeated_failure_step` never fires (no two
//! consecutive fails are identical) -- this keeps `implement_repeat_failure_
//! thinks` from substituting anything itself, so the ONLY trigger exercised
//! here is `implement_no_green_thinks`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, NO_GREEN_QUESTION};
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

/// Same shape as `think_trigger.rs`'s `make_file_tools_session`.
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

/// A turn that reads a file with an empty patch -- mirrors `stall_think.rs`'s
/// `read_turn`.
fn read_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// Finds the most recent `Action` log entry's `name`/`args`. Same as
/// `think_trigger.rs`'s `last_action`.
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

const THINKER_ANSWER: &str = "the plan step is wrong: drop it and add a new one for the actual \
     bug";

/// The exact question text unit 27's brief specifies, with `{n}` filled as
/// 12 -- pinned here so a change to either `NO_GREEN_QUESTION` or the
/// substitution's `{n}`-filling breaks a visible assertion, not just a
/// `contains` check.
fn expected_question(n: u32) -> String {
    format!(
        "{n} test runs have failed since the last pass. Is the plan itself wrong? Name the one \
         smallest change that could make the failing test pass, or say which plan step should be \
         dropped."
    )
}

/// Twelve failing `test` runs (each with a DIFFERENT failure summary, so the
/// repeated-failure trigger never fires), then a `read_file` action: the
/// read must be substituted for `think` with `NO_GREEN_QUESTION` filled with
/// `n = 12`.
#[tokio::test]
async fn twelve_failing_tests_then_a_read_becomes_think_with_the_filled_question() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    // Each command prints a DIFFERENT assertion-shaped summary (the command
    // string itself differs, and so does its output), so no two consecutive
    // fails are identical and `repeated_failure_step` never fires -- the
    // only trigger this fixture can exercise is `implement_no_green_thinks`.
    let mut turns = Vec::new();
    for n in 1..=12u32 {
        turns.push(test_turn(&format!(
            "printf 'assertion failed: case {n}\\n'; exit 1"
        )));
    }
    // Step 13: the model tries to read a file -- must be substituted for
    // think once fails_since_green has reached 12.
    turns.push(read_turn("a.txt"));

    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Steps 1-12: twelve failing test runs, each committing as an ordinary
    // `test` action (never substituted -- the trigger explicitly leaves
    // `test`/`think` actions alone).
    let mut fails_since_green = Vec::new();
    for n in 1..=12u32 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {n}: got {outcome:?}"
        );
        let (name, _args) = last_action(&log);
        assert_eq!(
            name, "test",
            "step {n} must commit as an ordinary test action, never substituted"
        );
        let (state, _v) = active.session().state().await.unwrap();
        fails_since_green.push(state.harness.fails_since_green);
        assert_eq!(
            thinker.calls().len(),
            0,
            "the thinker must not have been called yet at step {n}"
        );
    }
    assert_eq!(
        fails_since_green,
        (1..=12u32).collect::<Vec<_>>(),
        "fails_since_green must climb by exactly one on every failing test run"
    );

    // Step 13: the model asks to read a.txt -- fails_since_green is now 12
    // (>= NO_GREEN_THINK_AT) and no think has run since the last failing
    // test's step, so this must be substituted for think with
    // NO_GREEN_QUESTION filled with n = 12.
    let rec13 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome13 = log.append_record(rec13).unwrap();
    assert!(
        matches!(outcome13, Outcome::Continue),
        "the no-green substitution must NOT park: got {outcome13:?}"
    );
    let (name13, args13) = last_action(&log);
    assert_eq!(
        name13, "think",
        "the read at step 13 must be substituted for think once fails_since_green reaches 12"
    );
    let expected = expected_question(12);
    assert_eq!(args13, json!({"question": expected}));
    assert_eq!(expected.as_str(), NO_GREEN_QUESTION.replace("{n}", "12"));

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, expected);

    let (state13, _v) = active.session().state().await.unwrap();
    let decision13 = state13
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision13.question, expected);
    assert_eq!(decision13.answer, THINKER_ANSWER);
    assert_eq!(
        state13.harness.fails_since_green, 12,
        "the think step must not itself change fails_since_green (it is not a test call)"
    );
}

/// A `test` action at the same point (fails_since_green already at or past
/// `NO_GREEN_THINK_AT`) is NOT substituted: the trigger explicitly leaves
/// `test`/`think` actions alone so the model can still verify its own fix.
#[tokio::test]
async fn a_test_action_at_the_same_point_is_not_substituted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let mut turns = Vec::new();
    for n in 1..=12u32 {
        turns.push(test_turn(&format!(
            "printf 'assertion failed: case {n}\\n'; exit 1"
        )));
    }
    // Step 13: instead of a read, the model re-runs the test -- must NOT be
    // substituted.
    turns.push(test_turn("printf 'assertion failed: case 13\\n'; exit 1"));
    // Step 14: after the un-substituted test action commits as a 13th
    // failing run, the model tries a read -- fails_since_green is now 13,
    // still >= NO_GREEN_THINK_AT, so this read gets substituted, proving
    // the trigger only skipped step 13 because it was itself a `test` call,
    // not because the gate stopped holding.
    turns.push(read_turn("a.txt"));

    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    for n in 1..=12u32 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {n}: got {outcome:?}"
        );
    }
    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(state.harness.fails_since_green, 12);

    // Step 13: a `test` action while fails_since_green >= NO_GREEN_THINK_AT
    // -- must commit as an ordinary (failing) test action, not think.
    let rec13 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome13 = log.append_record(rec13).unwrap();
    assert!(matches!(outcome13, Outcome::Continue), "got {outcome13:?}");
    let (name13, _args13) = last_action(&log);
    assert_eq!(
        name13, "test",
        "a test action at the no-green cap must not be substituted for think"
    );
    assert_eq!(
        thinker.calls().len(),
        0,
        "the thinker must not have been called: the test action itself is exempt"
    );
    let (state13, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state13.harness.fails_since_green, 13,
        "the un-substituted test action must still commit as a real failing test run"
    );

    // Step 14: the next non-test action (a read) IS substituted, proving
    // the gate itself was never the reason step 13 was skipped.
    let rec14 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome14 = log.append_record(rec14).unwrap();
    assert!(matches!(outcome14, Outcome::Continue), "got {outcome14:?}");
    let (name14, args14) = last_action(&log);
    assert_eq!(
        name14, "think",
        "the next non-test action after the un-substituted test must be substituted for think"
    );
    let expected = expected_question(13);
    assert_eq!(args14, json!({"question": expected}));
    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once, at step 14"
    );
}
