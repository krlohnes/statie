//! Item 12: grounded review findings. `review_grounded_findings`
//! (`skills/src/coding/mod.rs`, wired into `ReviewSkill::validate` right
//! after `review_sendback_note`): a send-back finding must cite a
//! `path:line` the harness can check -- `path` present in the sandbox
//! (`CodingWorkspace::files` or `::seen`) and `line` within the file's known
//! line count. An ungrounded finding is dropped, with a note saying which
//! ones and why; if every finding in the send-back is ungrounded, the whole
//! step is refused instead (`PatchError::RuleViolation`, the same shape as
//! any other rule violation -- the step never commits, the model sees the
//! reason and gets a retry). `guards.grounded_findings` (`core/src/
//! guards.rs`, default on) restores the pre-grounding behaviour when off:
//! every finding passes through unchanged.
//!
//! Fixture style of `skills/tests/review_sendback.rs`/`review_looks.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `review_sendback.rs`/
//! `review_looks.rs` inline their own `config`/`make_file_tools_session`:
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
use statie_skills::{CodingWorkspace, ReviewSkill};
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
/// "10b. Validation notes").
fn note_entries(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The rejection reason of the most recent `rejected` System log entry, or
/// `None` if there is none (same pattern as `skills/tests/
/// commitment_rules.rs`'s `last_rejection_reason`).
fn last_rejection_reason(log: &Arc<dyn SessionLog>) -> Option<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .rev()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .and_then(|e| e.payload.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
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

/// The exact note text `review_grounded_findings` attaches when it drops
/// `n` findings (`skills/src/coding/mod.rs`) -- both the partial-drop note
/// and the all-dropped `PatchError::RuleViolation` reason share this same
/// text (the latter reaches the model prefixed with "skill rule violated: "
/// by `PatchError`'s `Display`, so tests assert with `.contains`).
fn dropped_note(n: usize) -> String {
    format!(
        "Dropped {n} finding{} with no file:line citation. Every finding must name the line it \
         is about.",
        if n == 1 { "" } else { "s" }
    )
}

// ---------------------------------------------------------------------
// (a) A grounded finding (cites a file present in `files`, in range per
// `seen`) passes through a send-back unchanged: no note, no drop.
// ---------------------------------------------------------------------

#[tokio::test]
async fn grounded_finding_passes_through_unchanged() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\nworld\n").unwrap();

    // Read `a.txt` first so `seen` carries its real line count (2), then
    // send a finding citing a line inside it.
    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
            })
            .to_string(),
        ),
        done_turn(json!({
            "findings": ["a.txt:1 the greeting is wrong"],
            "next_skill": "implement",
        })),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}; a grounded send-back must hand off, not reject"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 the greeting is wrong".to_string()],
        "the grounded finding must pass through unchanged"
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));

    let notes = note_entries(&log);
    assert!(
        notes.iter().all(|n| !n.contains("Dropped")),
        "no finding was dropped, so no grounding note should have been logged: {notes:?}"
    );
}

// ---------------------------------------------------------------------
// (b) One grounded + one ungrounded finding -> the ungrounded one is
// dropped, the grounded one survives, and the note names exactly what was
// dropped and why. Still a normal handoff (some findings remain).
// ---------------------------------------------------------------------

#[tokio::test]
async fn ungrounded_finding_is_dropped_with_the_exact_note_and_the_grounded_one_survives() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\nworld\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
            })
            .to_string(),
        ),
        done_turn(json!({
            "findings": [
                "a.txt:1 the greeting is wrong",
                "something is wrong somewhere, no idea where",
            ],
            "next_skill": "implement",
        })),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}; a partially grounded send-back must still hand off, not reject"
    );
    assert!(
        !any_rejected(&log),
        "dropping one of two findings is not a refusal"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 the greeting is wrong".to_string()],
        "only the grounded finding must survive"
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));

    let notes = note_entries(&log);
    let note = notes
        .iter()
        .find(|n| n.contains("Dropped"))
        .unwrap_or_else(|| panic!("expected a Dropped note among {notes:?}"));
    assert_eq!(
        note,
        &format!(
            "{} Dropped: \"something is wrong somewhere, no idea where\".",
            dropped_note(1)
        ),
        "note must name exactly how many findings were dropped and why"
    );
}

// ---------------------------------------------------------------------
// (c) Every finding in the send-back is ungrounded -> refused outright: the
// step never commits (state is exactly what it was before the step, i.e.
// the initial default workspace), a `rejected` System entry is logged with
// the exact note as its reason, and the session continues (not parked --
// this is the first attempt, well under the retry cap).
// ---------------------------------------------------------------------

#[tokio::test]
async fn all_ungrounded_send_back_is_refused_and_state_is_unchanged() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "findings": ["something is wrong somewhere, no idea where"],
        "next_skill": "implement",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (before, before_version) = active.session().state().await.unwrap();
    assert_eq!(before_version, statie_core::Version(0));
    assert!(before.agent.findings.is_empty());
    assert!(!before.agent.done);
    assert_eq!(before.agent.next_skill, None);

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an all-ungrounded send-back must be refused (rejected + retry), not \
         handed off or parked on the first attempt"
    );
    assert!(any_rejected(&log), "must have been rejected");

    let reason = last_rejection_reason(&log).expect("expected a rejected System entry");
    assert!(
        reason.contains(&dropped_note(1)),
        "rejection reason must carry the exact drop note: {reason}"
    );

    // A rejected step never commits (`core/src/session.rs`'s `reject`):
    // the version stays exactly what it was, and every field that a
    // successful send-back would have changed is untouched.
    let (after, after_version) = active.session().state().await.unwrap();
    assert_eq!(
        after_version, before_version,
        "a rejected step must not advance the committed version"
    );
    assert!(
        after.agent.findings.is_empty(),
        "the ungrounded finding must never have landed"
    );
    assert!(!after.agent.done);
    assert_eq!(
        after.agent.next_skill, None,
        "next_skill must never have landed either"
    );
}

// ---------------------------------------------------------------------
// (d) A citation whose line is past the file's known line count counts as
// ungrounded, same as no citation at all.
// ---------------------------------------------------------------------

#[tokio::test]
async fn out_of_range_line_counts_as_ungrounded() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\nworld\n").unwrap();

    let turns = vec![
        // `a.txt` has 2 lines; `seen` records that after this read.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
            })
            .to_string(),
        ),
        done_turn(json!({
            "findings": ["a.txt:999 a line that does not exist"],
            "next_skill": "implement",
        })),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let (before, before_version) = active.session().state().await.unwrap();
    assert!(before.agent.findings.is_empty());
    assert_eq!(before.agent.next_skill, None);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; the only finding cites a line past the file's end, so it is the \
         all-ungrounded case: refused, not handed off"
    );
    assert!(any_rejected(&log));

    let reason = last_rejection_reason(&log).expect("expected a rejected System entry");
    assert!(
        reason.contains(&dropped_note(1)),
        "rejection reason must carry the exact drop note: {reason}"
    );

    // A rejected step never commits: the version stays exactly what it was
    // after the read, and the fields a successful send-back would have
    // changed are untouched.
    let (after, after_version) = active.session().state().await.unwrap();
    assert_eq!(
        after_version, before_version,
        "a rejected step must not advance the committed version"
    );
    assert!(
        after.agent.findings.is_empty(),
        "the out-of-range finding must never have landed"
    );
    assert_eq!(
        after.agent.next_skill, None,
        "next_skill must never have landed either"
    );
}

// ---------------------------------------------------------------------
// (e) `guards.grounded_findings` off restores the current (pre-grounding)
// behaviour exactly: an all-ungrounded send-back still hands off, with no
// drop and no note.
// ---------------------------------------------------------------------

#[tokio::test]
async fn switch_off_passes_every_finding_through_unchanged() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "findings": ["something is wrong somewhere, no idea where"],
        "next_skill": "implement",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new().with_grounded_findings(false));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; with the switch off, an ungrounded send-back must hand off exactly as \
         it did before grounding existed"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.findings,
        vec!["something is wrong somewhere, no idea where".to_string()],
        "every finding must pass through unchanged with the switch off"
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));

    let notes = note_entries(&log);
    assert!(
        notes.iter().all(|n| !n.contains("Dropped")),
        "no grounding note should ever be logged with the switch off: {notes:?}"
    );
}
