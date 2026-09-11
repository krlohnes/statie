//! `implement_repeated_skip_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `implement_try_fixes_needs_failing_test`
//! and before `implement_repeat_failure_thinks`): unit 21 ("a second refused
//! re-read becomes a `think`"). Core's own redundant-identical-read guard
//! (7.7/7.8, `Active::execute` in `core/src/session.rs`) already refuses to
//! re-run an identical read once (`skipped_redundant`), but a live run could
//! repeat the identical, already-refused call a second, third, ... time
//! before `CONTINUE_REPEAT_CAP` ever parks it. This trigger substitutes a
//! `think` call the SECOND time the model asks for the SAME already-skipped
//! read, so the model gets the reasoning subagent's answer in hand instead
//! of parking three skips deep.
//!
//! This is the end-to-end proof that the substitution actually drives a real
//! `think` call through `Session::execute_think` (`core/src/session.rs`),
//! which writes `CodingWorkspace::decision` (`common_after_tool`'s `think`
//! handling) from the mock thinker's scripted answer -- the unit tests in
//! `skills/src/coding/tests.rs` cover the `validate`-level substitution in
//! isolation, but cannot exercise `Session::with_thinker`/`execute_think` at
//! all.
//!
//! Also proves the mirror-image correction unit 23 made to this guard's own
//! reset rule: a `think` step (`is_think_action`, `core/src/session.rs`)
//! does NOT clear `HarnessState.last_skip` or reset `skip_repeat_count` --
//! a think between two identical skips leaves the streak exactly as it was,
//! so a skip/think/skip/think/... cycle still reaches `CONTINUE_REPEAT_CAP`
//! and parks (at most two thinks, three skips), instead of resetting on
//! every think and running forever.
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
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
    WorkingSetConfig,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, REPEATED_SKIP_QUESTION};
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

/// Same shape as `think_trigger.rs`'s `make_file_tools_session`, but also
/// takes a `WorkingSetConfig` (the second test needs `max_windows: 0`).
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
    thinker: Arc<dyn statie_core::Thinker>,
    working_set: WorkingSetConfig,
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
    .with_working_set(working_set);
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

/// A turn that reads `path` with an empty patch (no `next` set, so the
/// commitment-slot guard never engages -- mirrors `think_trigger.rs`'s
/// `test_turn`/`write_file_turn`).
fn read_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// A turn that writes a file (a real edit).
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

const THINKER_ANSWER: &str = "the change belongs in tax.rs, not invoice.rs: edit line 42";

/// The exact question text unit 21's brief specifies, with `{path}` filled
/// in for `a.txt` -- pinned here so a change to either `REPEATED_SKIP_
/// QUESTION` or the substitution's path-filling breaks a visible assertion,
/// not just a `contains` check.
fn expected_question(path: &str) -> String {
    format!(
        "You asked to read {path} again, but its contents are already in your working set \
         above and nothing has changed. Either state the exact next edit (path, old text, new \
         text) that moves the plan forward, or name a different file or range you actually need \
         and why."
    )
}

/// Main scenario: read `a.txt`, read it again (core's own guard skips it,
/// `harness.last_skip` becomes `Some(count 1)`), read it a THIRD time --
/// this must be substituted for `think` with `REPEATED_SKIP_QUESTION`, the
/// thinker called exactly once, `decision` set from its answer, and no park.
/// Then a real edit clears `last_skip` back to `None`.
#[tokio::test]
async fn second_refused_reread_substitutes_think_then_an_edit_clears_last_skip() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let turns = vec![
        // Step 1: ordinary first read.
        read_turn("a.txt"),
        // Step 2: identical repeat -- core's 7.7/7.8 guard skips it (the
        // file is one line, so the window is never truncated and this
        // skips outright, no paging). `harness.last_skip` must become
        // `Some(count 1)`.
        read_turn("a.txt"),
        // Step 3: the model asks for the SAME read a third time -- must be
        // substituted for `think` with REPEATED_SKIP_QUESTION.
        read_turn("a.txt"),
        // Step 4: a real edit -- must clear `last_skip` back to `None`.
        write_file_turn("a.txt", "edited\n"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) = make_file_tools_session(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
        WorkingSetConfig::default(),
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: ordinary first read -- no skip, `last_skip` stays None.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    assert!(!any_event(&log, "skipped_redundant"));
    let (name1, _args1) = last_action(&log);
    assert_eq!(name1, "read_file");
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.harness.last_skip, None,
        "the first read must not set last_skip"
    );

    // Step 2: identical repeat -- core's own guard skips it outright.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    assert!(
        any_event(&log, "skipped_redundant"),
        "the identical repeat must be skipped by core's own guard"
    );
    let (state2, _v) = active.session().state().await.unwrap();
    let skip = state2
        .harness
        .last_skip
        .clone()
        .expect("a skipped_redundant step must set harness.last_skip");
    assert_eq!(skip.tool, "read_file");
    assert_eq!(skip.count, 1, "the first skip must record count 1");

    // Step 3: the model asks for the SAME read a third time -- must be
    // substituted for `think` with REPEATED_SKIP_QUESTION; the thinker must
    // have been called exactly once; `decision` must carry the question and
    // the scripted answer; the run must NOT park.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "the third repeat must NOT park: got {outcome3:?}"
    );
    assert!(!any_rejected(&log));

    let (name3, args3) = last_action(&log);
    assert_eq!(
        name3, "think",
        "the third identical read must be substituted for think"
    );
    let expected = expected_question("a.txt");
    assert_eq!(args3, json!({"question": expected}));
    assert_eq!(
        expected.as_str(),
        REPEATED_SKIP_QUESTION.replace("{path}", "a.txt")
    );

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, expected);

    let (state3, _v) = active.session().state().await.unwrap();
    let decision = state3
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision.question, expected);
    assert_eq!(decision.answer, THINKER_ANSWER);

    // Step 4: a real edit -- must clear `last_skip` back to `None`.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, _args4) = last_action(&log);
    assert_eq!(name4, "write_file");
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.last_skip, None,
        "a real edit must clear last_skip back to None"
    );
}

/// Second test: `max_windows: 0` (the working-set section disabled
/// entirely) does not change the shape -- the skip still happens (with the
/// "in the log" wording core's guard uses when windows are off), and the
/// third identical read is still substituted for `think`.
#[tokio::test]
async fn max_windows_zero_still_fires() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let turns = vec![read_turn("a.txt"), read_turn("a.txt"), read_turn("a.txt")];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) = make_file_tools_session(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
        WorkingSetConfig {
            max_windows: 0,
            recent_calls: 0,
        },
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let skip_entry = rec2
        .entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")
        })
        .expect("expected a skipped_redundant entry with windows off");
    let reason = skip_entry
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("that output is in the log"),
        "expected the plain log wording when windows are off, got: {reason}"
    );
    log.append_record(rec2).unwrap();

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "must not park: got {outcome3:?}"
    );
    assert!(!any_rejected(&log));

    let (name3, args3) = last_action(&log);
    assert_eq!(
        name3, "think",
        "the third identical read must be substituted for think even with windows off"
    );
    let expected = expected_question("a.txt");
    assert_eq!(args3, json!({"question": expected}));
    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
}

/// Third test: a DIFFERENT read after a skip must not fire -- `last_skip`'s
/// key no longer matches the model's next call, so it commits as an
/// ordinary `read_file`, not `think`.
#[tokio::test]
async fn a_different_read_after_a_skip_does_not_fire() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();
    std::fs::write(root.path().join("b.txt"), "other\n").unwrap();

    let turns = vec![
        // Step 1: read a.txt.
        read_turn("a.txt"),
        // Step 2: identical repeat -- skipped, last_skip set for a.txt.
        read_turn("a.txt"),
        // Step 3: a DIFFERENT read (b.txt) -- must NOT be substituted: the
        // skip's key was for a.txt, not b.txt.
        read_turn("b.txt"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER),
    )]));
    let (mut session, log) = make_file_tools_session(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
        WorkingSetConfig::default(),
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        rec2.entries().iter().any(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")
        }),
        "the identical repeat of a.txt must be skipped"
    );
    log.append_record(rec2).unwrap();
    let (state2, _v) = active.session().state().await.unwrap();
    assert!(
        state2.harness.last_skip.is_some(),
        "the skip of a.txt must set last_skip"
    );

    // Step 3: a different read (b.txt) -- must commit as an ordinary
    // read_file, not think, and the thinker must never be called.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    assert!(!any_rejected(&log));

    let (name3, args3) = last_action(&log);
    assert_eq!(
        name3, "read_file",
        "a different read after a skip must commit as read_file, not be substituted"
    );
    assert_eq!(
        args3.get("path").and_then(Value::as_str),
        Some("b.txt"),
        "got {args3:?}"
    );
    assert_eq!(
        thinker.calls().len(),
        0,
        "the mock thinker must never have been called"
    );
}

/// Fourth test (the mirror-image bounded-sequence test unit 23's correction
/// requires): identical skips of the SAME read, with a `think` between each
/// pair, must still park at `CONTINUE_REPEAT_CAP` with exactly two `think`
/// actions -- proving a `think` no longer clears `HarnessState.last_skip`/
/// resets `skip_repeat_count` (`core/src/session.rs`'s `is_think_action`
/// exemption), so the skip streak survives across the substituted thinks
/// the same way the repeated-tool-error streak now does.
///
/// Trace: ordinary first read (step 1, no skip) -> identical repeat skipped
/// (step 2, `last_skip` count 1) -> identical repeat again, substituted for
/// think (step 3, `skip.step`=2 has no think yet) -> identical repeat
/// skipped again (step 4, SAME streak now count 2, since the think at step
/// 3 did not reset it) -> identical repeat again, substituted for think
/// again (step 5, `skip.step` is now 4, newer than `last_think_step`=3, so
/// the trigger fires again) -> identical repeat skipped a third time (step
/// 6, SAME streak now count 3 = `CONTINUE_REPEAT_CAP`) -> parks. Exactly
/// two thinks (steps 3 and 5), exactly three skips (steps 2, 4, 6).
#[tokio::test]
async fn skip_think_skip_think_skip_parks_at_the_cap_with_exactly_two_thinks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let turns = vec![
        read_turn("a.txt"),
        read_turn("a.txt"),
        read_turn("a.txt"),
        read_turn("a.txt"),
        read_turn("a.txt"),
        read_turn("a.txt"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![
        Ok(ThinkOutput::answer("first think answer")),
        Ok(ThinkOutput::answer("second think answer")),
    ]));
    let (mut session, log) = make_file_tools_session(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
        WorkingSetConfig::default(),
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: ordinary first read -- no skip.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(state1.harness.last_skip, None);

    // Step 2: identical repeat -- skipped, count 1.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(state2.harness.last_skip.as_ref().map(|s| s.count), Some(1));

    // Step 3: substituted think (first of exactly two).
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (name3, _args3) = last_action(&log);
    assert_eq!(name3, "think", "step 3 must be the first substituted think");
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state3.harness.last_skip.as_ref().map(|s| s.count),
        Some(1),
        "the think must not reset the streak: count stays 1"
    );

    // Step 4: identical repeat skipped again -- SAME streak, count 2.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.last_skip.as_ref().map(|s| s.count),
        Some(2),
        "step 4 must advance the SAME streak to count 2, proving the think at step 3 did not \
         reset it"
    );

    // Step 5: substituted think again (second of exactly two) -- the
    // trigger fires because the skip's step (4) is newer than
    // last_think_step (3).
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");
    let (name5, _args5) = last_action(&log);
    assert_eq!(
        name5, "think",
        "step 5 must be the second substituted think (the skip's step is newer than the think \
         step)"
    );
    let (state5, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state5.harness.last_skip.as_ref().map(|s| s.count),
        Some(2),
        "the second think must not reset the streak either: count stays 2"
    );

    // Step 6: identical repeat skipped a third time -- SAME streak reaches
    // count 3 = CONTINUE_REPEAT_CAP, parks.
    let rec6 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome6 = log.append_record(rec6).unwrap();
    assert!(
        matches!(outcome6, Outcome::Parked(_)),
        "the third skip of the streak must park: got {outcome6:?}"
    );

    assert_eq!(
        thinker.calls().len(),
        2,
        "exactly two think actions must have run across the whole bounded sequence"
    );
    assert_eq!(thinker.calls()[0].question, expected_question("a.txt"));
    assert_eq!(thinker.calls()[1].question, expected_question("a.txt"));
}
