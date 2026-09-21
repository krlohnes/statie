//! Unit 49 (cap the implement-review round trip): build 32 gemma
//! `rename_across_files` seed 2, 188 steps, 148 activations -- `review`
//! handed the same duplicated doc-comment finding back to `implement` 75
//! times, and `implement` handed it straight back with an empty patch and
//! no tool call every time, because re-activation zeroes `harness.
//! tool_calls` and nothing counted the handoffs themselves
//! (`skills/src/coding/mod.rs`, `implement_findings_open_note`/
//! `review_rounds_cap_note`).
//!
//! Fixture style of `skills/tests/it/review_sendback.rs`/`premature_done.rs`:
//! a `Session<CodingWorkspace>` wired with the real file tools and a single
//! scripted `MockModel` covering every activation in order (the same model
//! is shared across `session.activate` calls for different skills --
//! `session.activate` just changes which skill validates the model's next
//! output, not which model answers it), with `drop(active)` releasing the
//! borrow before the next `activate` call (the same pattern `implement_
//! done.rs`/`review_blind.rs`/`recovery.rs` use), and outcomes/log entries
//! read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `review_sendback.rs`,
//! `done_stays_done.rs`, `commitment_rules.rs`, and `premature_done.rs`
//! inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{
    CodingWorkspace, FINDINGS_OPEN_NOTE_PREFIX, ImplementSkill, REVIEW_ROUNDS_CAP_QUESTION,
    ReviewSkill,
};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `review_sendback.rs`'s `config`.
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

/// Same shape as `review_sendback.rs`'s `ArcModel`.
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

/// Same shape as `review_sendback.rs`'s `make_file_tools_session`.
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

/// Every `note` System entry's `text`, most recent last -- where a
/// `Done`/handoff step's validation note lands (see `core/src/session.rs`'s
/// "10b. Validation notes": no pending observation exists for a non-tool
/// action, so the note is logged as a `System` entry instead).
fn note_entries(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Every `System` entry with `event: "continued"`, most recent last (same
/// as `premature_done.rs`'s `continued_notes`) -- where a harness
/// `Action::Continue`'s note is logged.
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

/// An `edit_file` turn with the given `old`/`new` text against `path`, and
/// no other patch changes.
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

/// A turn that reads a file (a harmless, real tool call) while also
/// patching the given extra fields -- same shape as `review_sendback.rs`'s
/// `read_file_turn_with_patch`.
fn read_file_turn_with_patch(path: &str, patch: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

const FINDING: &str = "a.txt:1 duplicated doc-comment line";

/// The send-back patch every `review` turn in these tests sends: `findings`
/// non-empty, `done: false`, `next_skill: implement` explicitly (a plain
/// model-sent send-back, no normalizing needed from `review_sendback_note`).
fn sendback_patch() -> Value {
    json!({
        "findings": [FINDING],
        "done": false,
        "next_skill": "implement",
    })
}

// ---------------------------------------------------------------------
// (1) A scripted round trip: `review` sends the finding back, `implement`
// answers `done` at once (no tool call) -- parked with the exact
// `FINDINGS_OPEN_NOTE_PREFIX` note; then an `edit_file` and a `done` in the
// same activation hand off normally.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_findings_open_note_parks_then_clears_after_an_edit_and_done() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        // review: one look (grounds the `a.txt:1` citation via `seen` and
        // satisfies the look-before-approve gate), then the send-back.
        read_file_turn_with_patch("a.txt", json!({})),
        done_turn(sendback_patch()),
        // implement, round 1: `done` immediately, findings still open, no
        // tool call yet -- must park with the exact note.
        done_turn(json!({})),
        // implement: `edit_file` (addresses the finding).
        edit_turn("a.txt", "hello", "hello world"),
        // implement: a passing `test` (so the final `done` below hands off
        // outright instead of `implement_no_green_runs_test` substituting a
        // test call for it -- unrelated to unit 49, just what an ordinary
        // handoff needs).
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "test", "args": {"cmd": "true"}},
            })
            .to_string(),
        ),
        // implement: `done`, findings cleared, hands off to review -- must
        // land as an ordinary handoff (a tool call already ran this
        // activation, so `implement_findings_open_note` does not fire
        // again).
        done_turn(json!({"findings": [], "next_skill": "review"})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let review_skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut review_active = session.activate(Arc::clone(&review_skill)).await.unwrap();

    let rec1 = review_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let _ = log.append_record(rec1).unwrap();
    let rec2 = review_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}; the send-back must hand off to implement"
    );
    drop(review_active);

    let implement_skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut implement_active = session
        .activate(Arc::clone(&implement_skill))
        .await
        .unwrap();

    // Step 1: parked as Continue with the exact findings-open note.
    let rec3 = implement_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; a `done` with open findings and no tool call must park as Continue"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let expected_note = format!(
        "{FINDINGS_OPEN_NOTE_PREFIX}{FINDING} Fix them with `edit_file` and `test`, or \
         `ask_user` why they are wrong. Your patch was kept."
    );
    let notes = continued_notes(&log);
    assert_eq!(
        notes.last(),
        Some(&expected_note),
        "expected the exact findings-open note, got {notes:?}"
    );
    let (state, _version) = implement_active.session().state().await.unwrap();
    assert_eq!(
        state.agent.findings,
        vec![FINDING.to_string()],
        "findings must still be open -- the patch committed untouched"
    );

    // Step 2: `edit_file` runs normally.
    let rec4 = implement_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    assert!(!any_rejected(&log), "the edit must not have been rejected");

    // Step 3: the passing `test` runs normally.
    let rec5 = implement_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(matches!(outcome5, Outcome::Continue), "got {outcome5:?}");
    assert!(!any_rejected(&log), "the test must not have been rejected");

    // Step 4: `done` with findings cleared hands off to review normally --
    // the findings-open note does not fire again (a tool call already ran
    // this activation).
    let rec6 = implement_active
        .step(Event::Tick, &mut NullSink)
        .await
        .unwrap();
    let outcome6 = log.append_record(rec6).unwrap();
    assert!(
        matches!(outcome6, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome6:?}; the final `done` must hand off normally once findings are cleared"
    );
    assert!(!any_rejected(&log));
}

// ---------------------------------------------------------------------
// (2) Four `review` activations each sending back the same finding: the
// fourth parks `AskUser` with the exact cap question; `review_rounds` reads
// 4.
// ---------------------------------------------------------------------

#[tokio::test]
async fn fourth_review_sendback_parks_with_the_cap_question_and_review_rounds_reads_four() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    // Every review activation: one look, then a send-back. Every implement
    // activation in between bounces it straight back to review with an
    // empty patch and no tool call (mirroring the live run) -- this itself
    // parks via `implement_findings_open_note` (covered by the test above),
    // so each implement activation needs three turns: the parked `done`, a
    // passing `test` (so the final `done` hands off outright instead of
    // `implement_no_green_runs_test` substituting a test call for it --
    // unrelated to unit 49), then a `done` naming review explicitly.
    let mut turns = Vec::new();
    for _round in 1..=4u32 {
        // review: look, then send back.
        turns.push(read_file_turn_with_patch("a.txt", json!({})));
        turns.push(done_turn(sendback_patch()));
        // implement (skipped after round 4's send-back, which parks
        // review itself and never hands off).
        turns.push(done_turn(json!({})));
        turns.push(MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "test", "args": {"cmd": "true"}},
            })
            .to_string(),
        ));
        turns.push(done_turn(json!({"next_skill": "review"})));
    }
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let review_skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let implement_skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());

    for round in 1..=4u32 {
        let mut review_active = session.activate(Arc::clone(&review_skill)).await.unwrap();
        let rec_look = review_active
            .step(Event::Tick, &mut NullSink)
            .await
            .unwrap();
        let _ = log.append_record(rec_look).unwrap();
        let rec_sendback = review_active
            .step(Event::Tick, &mut NullSink)
            .await
            .unwrap();
        let outcome = log.append_record(rec_sendback).unwrap();

        if round < 4 {
            assert!(
                matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
                "round {round}: got {outcome:?}; expected a send-back handoff to implement"
            );
            assert!(!any_rejected(&log), "round {round}: must not be rejected");
            drop(review_active);

            let mut implement_active = session
                .activate(Arc::clone(&implement_skill))
                .await
                .unwrap();
            let rec_bounce1 = implement_active
                .step(Event::Tick, &mut NullSink)
                .await
                .unwrap();
            let _ = log.append_record(rec_bounce1).unwrap();
            let rec_bounce2 = implement_active
                .step(Event::Tick, &mut NullSink)
                .await
                .unwrap();
            let _ = log.append_record(rec_bounce2).unwrap();
            let rec_bounce3 = implement_active
                .step(Event::Tick, &mut NullSink)
                .await
                .unwrap();
            let outcome_bounce3 = log.append_record(rec_bounce3).unwrap();
            assert!(
                matches!(outcome_bounce3, Outcome::Handoff(ref t) if t == "review"),
                "round {round}: got {outcome_bounce3:?}; expected implement to hand back to review"
            );
            drop(implement_active);
        } else {
            assert!(
                matches!(outcome, Outcome::Parked(Park::AskUser(_))),
                "round {round}: got {outcome:?}; the fourth send-back must park with AskUser"
            );
            let Outcome::Parked(Park::AskUser(message)) = outcome else {
                unreachable!()
            };
            let expected_message = REVIEW_ROUNDS_CAP_QUESTION
                .replace("{n}", "3")
                .replace("{findings}", FINDING);
            assert_eq!(
                message, expected_message,
                "expected the exact review-rounds-cap question"
            );
            assert!(!any_rejected(&log), "round {round}: must not be rejected");

            let notes = note_entries(&log);
            assert!(
                notes.last().is_some_and(|n| n
                    == "review rounds cap (3) reached: parked with the findings"),
                "expected the exact park note, got {notes:?}"
            );

            let (state, _version) = review_active.session().state().await.unwrap();
            assert_eq!(
                state.harness.activation, "review",
                "the parked activation must still be review"
            );
            assert_eq!(
                state.agent.review_rounds, 4,
                "review_rounds must read 4 (four activations of review)"
            );
        }
    }
}
