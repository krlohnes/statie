//! `implement_stall_thinks` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `implement_repeated_error_thinks`
//! and before `implement_repeat_failure_thinks`): unit 25 part 3 ("one
//! streak across a refused read and a failed edit on the same path"). Core's
//! `last_skip`/`last_tool_error` guards each only ever see a streak of the
//! SAME shape recurring (an identical re-read, or an identical-SHAPE tool
//! error); neither one alone recognizes a model that alternates re-reading a
//! file (refused as redundant) and failing to edit it (a tool error) as
//! going back and forth on that ONE path. `HarnessState::last_stall`
//! (`core/src/version.rs`) tracks that combined streak, written by core
//! whenever a skip or an edit-tool error lands on a path
//! (`core/src/session.rs`'s stalled-path guard), and this trigger
//! substitutes a `think` call once `count >= 2`.
//!
//! This is the end-to-end proof that the substitution actually drives a real
//! `think` call through `Session::execute_think` (`core/src/session.rs`),
//! which writes `CodingWorkspace::decision` (`common_after_tool`'s `think`
//! handling) from the mock thinker's scripted answer -- the unit tests in
//! `skills/src/coding/tests.rs` cover the `validate`-level substitution in
//! isolation, but cannot exercise `Session::with_thinker`/`execute_think` at
//! all.
//!
//! Fixture style copied from `skills/tests/it/error_think.rs`/`skip_think.rs`:
//! a `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`, plus a `MockThinker` attached via `Session::with_thinker`.
//!
//! Deliberately not `mod common;`, for the same reason `error_think.rs`/
//! `skip_think.rs` inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
    WorkingSetConfig,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, STALLED_PATH_QUESTION};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `error_think.rs`'s `config`.
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

/// Same shape as `error_think.rs`'s `ArcModel`.
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

/// Same shape as `error_think.rs`'s `make_file_tools_session`.
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

/// A turn that reads `path` with an empty patch (no `next` set, so the
/// commitment-slot guard never engages -- mirrors `skip_think.rs`'s
/// `read_turn`).
fn read_turn(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

/// An `edit_file` turn with the given `old`/`new` text against `path` --
/// mirrors `error_think.rs`'s `edit_turn`.
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

/// A turn that writes a file (a real, SUCCESSFUL edit, of a DIFFERENT shape
/// than `edit_file`) -- used to prove an executed edit on the stalled path
/// resets the stall.
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
/// `error_think.rs`'s `last_action`.
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

fn has_event(rec: &statie_core::StepRecord, event: &str) -> bool {
    rec.entries().iter().any(|e| {
        e.kind == LogKind::System && e.payload.get("event").and_then(Value::as_str) == Some(event)
    })
}

fn non_progress_cap_shapes(rec: &statie_core::StepRecord) -> Option<String> {
    rec.entries()
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("non_progress_cap")
        })
        .and_then(|e| e.payload.get("shapes").and_then(Value::as_str))
        .map(str::to_string)
}

const THINKER_ANSWER_1: &str = "the anchor moved; re-read a.txt before your next edit";
const THINKER_ANSWER_2: &str = "still stuck: try inserting after line 3 instead";
const THINKER_ANSWER_3: &str = "give up on a.txt; the change belongs in b.txt instead";
const THINKER_ANSWER_4: &str = "one more nudge toward the actual fix";

/// The exact question text unit 25 part 3's brief specifies, with `{path}`
/// filled as `a.txt` -- pinned here so a change to either
/// `STALLED_PATH_QUESTION` or the substitution's path-filling breaks a
/// visible assertion, not just a `contains` check.
fn expected_question(path: &str) -> String {
    format!(
        "You have been going back and forth on {path}: reading it again and editing it have \
         both failed. Using the working set above, state the one exact edit (old text as it \
         appears, new text) that moves the plan forward, or say which other file the change \
         belongs in."
    )
}

/// Main scenario (the brief's own trace): read P, read P (skipped by core's
/// own 7.7/7.8 guard), edit P (a `tool_error`), read P again -- the LAST of
/// these must be substituted for `think` with `STALLED_PATH_QUESTION` for P,
/// the thinker called exactly once so far, and `decision` set from its
/// answer.
///
/// Then, continuing to alternate reads and edits on the SAME path (using two
/// DIFFERENT edit-error shapes -- "old text not found" and "old text occurs
/// N times" -- so neither the ordinary repeated-skip guard nor the ordinary
/// repeated-tool-error guard's own SAME-shape streak ever reaches
/// `CONTINUE_REPEAT_CAP` (3) first) parks at the SIXTH stall event with a
/// `non_progress_cap` System entry whose `shapes` names the path
/// ("stalled on a.txt").
#[tokio::test]
async fn stall_substitutes_think_then_continuing_to_alternate_parks_at_the_sixth() {
    let root = tempfile::tempdir().unwrap();
    // "line " (with the trailing space) appears at the start of all three
    // lines, so `old: "line "` is a literal match 3 times -- the "old text
    // occurs N times" shape, DIFFERENT from "old text not found".
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        // Step 1: ordinary first read -- no skip.
        read_turn("a.txt"),
        // Step 2: identical repeat -- core's 7.7/7.8 guard skips it.
        // `last_stall` becomes `Some(count 1)`.
        read_turn("a.txt"),
        // Step 3: an edit that fails ("old text not found in a.txt", shape
        // A). `last_stall` advances to count 2 (same path, no executed step
        // since step 2).
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 4: the model asks to read a.txt again -- `last_stall.count`
        // is now 2, so this must be substituted for `think` with
        // STALLED_PATH_QUESTION.
        read_turn("a.txt"),
        // Step 5: a DIFFERENT edit-error shape ("old text occurs 3 times in
        // a.txt", shape B) -- resets the ORDINARY repeated-tool-error
        // guard's own same-shape streak back to count 1, but is STILL a
        // stall event on the SAME path, advancing `last_stall` to count 3.
        edit_turn("a.txt", "line ", "replacement "),
        // Step 6: read again -- `last_stall.count` (3) >= 2 and a think has
        // run since count first reached 2 (step 4) but `last_stall.step`
        // (5) is newer than `last_think_step` (4), so the trigger fires
        // again.
        read_turn("a.txt"),
        // Step 7: back to shape A -- different from step 5's shape B, so
        // the ordinary repeated-tool-error guard sees a fresh streak again.
        // `last_stall` advances to count 4.
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 8: read again -- substituted for think a third time.
        read_turn("a.txt"),
        // Step 9: shape B again -- `last_stall` advances to count 5.
        edit_turn("a.txt", "line ", "replacement "),
        // Step 10: read again -- substituted for think a fourth time.
        read_turn("a.txt"),
        // Step 11: shape A again -- `last_stall` reaches count 6 =
        // NON_PROGRESS_CAP, parking via `park_non_progress_cap`.
        edit_turn("a.txt", "line not present", "replacement"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![
        Ok(ThinkOutput::answer(THINKER_ANSWER_1)),
        Ok(ThinkOutput::answer(THINKER_ANSWER_2)),
        Ok(ThinkOutput::answer(THINKER_ANSWER_3)),
        Ok(ThinkOutput::answer(THINKER_ANSWER_4)),
    ]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: ordinary first read.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.harness.last_stall, None,
        "the first read must not set last_stall"
    );

    // Step 2: identical repeat -- skipped, last_stall count 1.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (state2, _v) = active.session().state().await.unwrap();
    let stall2 = state2
        .harness
        .last_stall
        .clone()
        .expect("the skipped repeat must set last_stall");
    assert_eq!(stall2.path, "a.txt");
    assert_eq!(stall2.count, 1);

    // Step 3: the failing edit -- last_stall advances to count 2 (same
    // path, no executed step since step 2's skip).
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (state3, _v) = active.session().state().await.unwrap();
    let stall3 = state3
        .harness
        .last_stall
        .clone()
        .expect("the failing edit must advance last_stall");
    assert_eq!(stall3.path, "a.txt");
    assert_eq!(
        stall3.count, 2,
        "the skip (step 2) and the edit error (step 3) must count as ONE stalled-on-P streak"
    );

    // Step 4: the model asks to re-read a.txt -- last_stall.count is 2, so
    // this must be substituted for think with STALLED_PATH_QUESTION.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(
        matches!(outcome4, Outcome::Continue),
        "the stalled-path substitution must NOT park: got {outcome4:?}"
    );
    let (name4, args4) = last_action(&log);
    assert_eq!(
        name4, "think",
        "the read right after the stall reaches count 2 must be substituted for think"
    );
    let expected = expected_question("a.txt");
    assert_eq!(args4, json!({"question": expected}));
    assert_eq!(
        expected.as_str(),
        STALLED_PATH_QUESTION.replace("{path}", "a.txt")
    );
    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once so far"
    );
    assert_eq!(thinker.calls()[0].question, expected);
    let (state4, _v) = active.session().state().await.unwrap();
    let decision4 = state4
        .agent
        .decision
        .clone()
        .expect("decision must be set from the think call's output");
    assert_eq!(decision4.question, expected);
    assert_eq!(decision4.answer, THINKER_ANSWER_1);
    assert_eq!(
        state4.harness.last_stall.as_ref().map(|s| s.count),
        Some(2),
        "the think step must keep the stall count exactly as it was (D19)"
    );

    // Steps 5-10: alternating edit (differing shapes)/read, each read
    // substituted for think, each edit advancing the SAME stall streak.
    for (step_no, expected_stall_count) in [(5, 3u32), (7, 4), (9, 5)] {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {step_no}: got {outcome:?}"
        );
        let (name, _args) = last_action(&log);
        assert_eq!(
            name, "edit_file",
            "step {step_no} must commit as an ordinary edit_file error"
        );
        let (state, _v) = active.session().state().await.unwrap();
        let stall = state
            .harness
            .last_stall
            .clone()
            .expect("step {step_no} must advance last_stall");
        assert_eq!(stall.path, "a.txt");
        assert_eq!(
            stall.count, expected_stall_count,
            "step {step_no}: last_stall.count"
        );

        // The following read step must be substituted for think again.
        let rec_read = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome_read = log.append_record(rec_read).unwrap();
        assert!(
            matches!(outcome_read, Outcome::Continue),
            "the read after step {step_no}: got {outcome_read:?}"
        );
        let (name_read, args_read) = last_action(&log);
        assert_eq!(
            name_read, "think",
            "the read right after step {step_no} must be substituted for think"
        );
        assert_eq!(args_read, json!({"question": expected_question("a.txt")}));
        let (state_read, _v) = active.session().state().await.unwrap();
        assert_eq!(
            state_read.harness.last_stall.as_ref().map(|s| s.count),
            Some(expected_stall_count),
            "the think step after step {step_no} must keep the stall count exactly as it was"
        );
    }

    assert_eq!(
        thinker.calls().len(),
        4,
        "exactly four think actions must have run across the whole bounded sequence so far"
    );

    // Step 11: the sixth stall event (shape A again) -- last_stall reaches
    // count 6 = NON_PROGRESS_CAP, parking via park_non_progress_cap with a
    // non_progress_cap System entry whose shapes name the path.
    let rec11 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let is_cap = has_event(&rec11, "non_progress_cap");
    let shapes = non_progress_cap_shapes(&rec11);
    let outcome11 = log.append_record(rec11).unwrap();
    assert!(
        matches!(outcome11, Outcome::Parked(_)),
        "the sixth stall event must park: got {outcome11:?}"
    );
    assert!(is_cap, "the park must log a non_progress_cap System entry");
    let shapes = shapes.expect("non_progress_cap entry should carry a shapes string");
    assert_eq!(
        shapes, "stalled on a.txt",
        "the shape must name the path exactly as the brief's example does"
    );
    match outcome11 {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert!(
                msg.contains("6 steps in a row without progress"),
                "park message should say 6 steps in a row without progress: {msg}"
            );
            assert!(
                msg.contains("stalled on a.txt"),
                "park message should contain the shape: {msg}"
            );
        }
        other => panic!("step 11: expected Parked(AskUser(_)), got {other:?}"),
    }
}

/// Second test (the brief's own requirement): an executed edit on the
/// stalled path, in between a skip and a later edit error, resets the
/// stall -- proving the count does NOT survive a real, SUCCESSFUL step on
/// that path the way it survives a `think`.
#[tokio::test]
async fn an_executed_edit_on_the_path_in_between_resets_the_stall() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let turns = vec![
        // Step 1: ordinary first read.
        read_turn("a.txt"),
        // Step 2: identical repeat -- skipped, last_stall count 1.
        read_turn("a.txt"),
        // Step 3: a SUCCESSFUL write_file on a.txt -- an executed step on
        // the SAME path, must reset last_stall back to None.
        write_file_turn("a.txt", "brand new contents\n"),
        // Step 4: a failing edit right after -- must start a FRESH stall
        // streak at count 1, not continue the old one at count 2.
        edit_turn("a.txt", "line not present", "replacement"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer(THINKER_ANSWER_1),
    )]));
    let (mut session, log) =
        make_file_tools_session(Arc::clone(&model), config(&root), Arc::clone(&thinker) as _);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();
    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.harness.last_stall.as_ref().map(|s| s.count),
        Some(1),
        "the skipped repeat must set last_stall to count 1"
    );

    // Step 3: a successful write_file -- must clear last_stall.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    let (name3, _args3) = last_action(&log);
    assert_eq!(name3, "write_file");
    let (state3, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state3.harness.last_stall, None,
        "an executed write_file on the stalled path must reset last_stall back to None"
    );

    // Step 4: a failing edit right after -- must start a FRESH streak at
    // count 1 (not continue the old streak at count 2), and must NOT be
    // substituted for think (count < 2).
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (name4, _args4) = last_action(&log);
    assert_eq!(
        name4, "edit_file",
        "the failing edit must commit as an ordinary edit_file error, not a think substitution"
    );
    let (state4, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state4.harness.last_stall.as_ref().map(|s| s.count),
        Some(1),
        "the failing edit after the reset must start a FRESH stall streak at count 1"
    );
    assert_eq!(
        thinker.calls().len(),
        0,
        "the mock thinker must never have been called: the stall never reached count 2"
    );
}
