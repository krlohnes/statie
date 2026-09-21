//! Item 12: grounded review findings. `review_grounded_findings`
//! (`skills/src/coding/mod.rs`, wired into `ReviewSkill::validate` right
//! after `review_sendback_note`): a send-back finding must cite a
//! `path:line` the harness can check -- `path` present in the sandbox
//! (`CodingWorkspace::files` or `::seen`) and `line` within the file's known
//! line count. An ungrounded finding is dropped, with a note saying which
//! ones and why. Unit 53: if every finding in the send-back is ungrounded,
//! the step is now refused as a harness-internal `Action::Continue`
//! (`findings` removed from the raw patch, the rest of the patch and the
//! previously committed `findings` kept, `next_skill` stripped) instead of
//! the `PatchError::RuleViolation` rejection this used to be -- the step
//! commits, no retry is spent, and the model sees the reason with its patch
//! intact. A second such refusal in a row is substituted for `think`
//! (item 47's `refusal_thinks`, now wired into `ReviewSkill::validate` too).
//! `guards.grounded_findings` (`core/src/guards.rs`, default on) restores
//! the pre-grounding behaviour when off: every finding passes through
//! unchanged.
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
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink, ThinkOutput,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{
    CodingWorkspace, REFUSAL_QUESTION, REFUSAL_THINK_NOTE_PREFIX, ReviewSkill,
    UNGROUNDED_FINDINGS_NOTE_PREFIX,
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

/// Same shape as `make_file_tools_session`, but with a `MockThinker`
/// attached (`Session::with_thinker`) -- needed to exercise item 47's
/// `refusal_thinks` substitution end-to-end (a real `think` action driven
/// through `Session::execute_think`), the same fixture pattern
/// `skills/tests/it/premature_done.rs`'s `make_file_tools_session_with_
/// thinker` uses.
fn make_file_tools_session_with_thinker(
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

/// Every `System` entry with `event: "continued"`, most recent last -- the
/// note text a harness-internal `Action::Continue` carries (see
/// `core/src/session.rs`'s `Action::Continue` arm; same helper as
/// `premature_done.rs`'s `continued_notes`).
fn continued_notes(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("continued"))
        .filter_map(|e| e.payload.get("note").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The most recent `Action` log entry's `name`/`args` for a `Tool` action,
/// or its `type` (`"continue"`, `"done"`, ...) with a null `args` for any
/// other shape. Same as `premature_done.rs`'s `last_action`.
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

/// The exact note text `review_grounded_findings`'s `PartialDrop` case
/// attaches when it drops `n` (but not all) findings (`skills/src/coding/
/// mod.rs`).
fn dropped_note(n: usize) -> String {
    format!(
        "Dropped {n} finding{} with no file:line citation. Every finding must name the line it \
         is about.",
        if n == 1 { "" } else { "s" }
    )
}

/// The exact note text `review_grounded_findings`'s `AllDropped` case
/// attaches (unit 53): [`UNGROUNDED_FINDINGS_NOTE_PREFIX`] followed by the
/// same quoted, `; `-joined drop list the partial-drop case uses. This is
/// also the exact text of the harness `Action::Continue`'s note (the
/// refusal the model sees), not a rejection reason -- the step commits.
fn all_dropped_note(findings: &[&str]) -> String {
    let named: Vec<String> = findings.iter().map(|f| format!("\"{f}\"")).collect();
    format!("{UNGROUNDED_FINDINGS_NOTE_PREFIX}{}.", named.join("; "))
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
// (c) Every finding in the send-back is ungrounded -> refused as a
// harness-internal `Action::Continue` (unit 53), not a rejection: the step
// COMMITS (findings stay exactly as they were before the step -- empty,
// since none were ever grounded -- but the rest of the patch, e.g.
// `recent_summary`, is applied, and `next_skill` never lands), a
// `continued` System entry is logged with the exact note, and the retry
// count is not consumed. Three such steps in a row are not parked: the
// second becomes item 47's `think` (`refusal_thinks`), and the third is
// still under `CONTINUE_REPEAT_CAP` (3) by the time it commits (see the
// step-by-step assertions below for why).
// ---------------------------------------------------------------------

#[tokio::test]
async fn all_ungrounded_send_back_is_a_refusal_and_commits_with_findings_unchanged() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![done_turn(json!({
        "findings": ["something is wrong somewhere, no idea where"],
        "next_skill": "implement",
        "recent_summary": "looked at the crate",
    }))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (before, before_version) = active.session().state().await.unwrap();
    // Unit 49: `ReviewSkill::on_activate` now always returns a patch (it
    // sets `review_rounds: 1` on every activation, not just when
    // `next_skill` needs clearing), so the version is already 1 by the
    // time the model's first step runs, not 0.
    assert_eq!(before_version, statie_core::Version(1));
    assert!(before.agent.findings.is_empty());
    assert!(!before.agent.done);
    assert_eq!(before.agent.next_skill, None);
    assert_eq!(before.agent.review_rounds, 1);
    let before_findings = before.agent.findings.clone();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; an all-ungrounded send-back must be refused (harness Continue), not \
         rejected, handed off, or parked on the first attempt"
    );
    assert!(
        !any_rejected(&log),
        "unit 53: an all-dropped send-back is a refusal, not a rejection"
    );

    let expected_note = all_dropped_note(&["something is wrong somewhere, no idea where"]);
    let notes = continued_notes(&log);
    assert_eq!(
        notes.last(),
        Some(&expected_note),
        "the committed step must carry the exact all-dropped note"
    );

    // The step COMMITS (unlike the old rejection): the version advances,
    // `findings` stays exactly as it was before the step (nothing
    // ungrounded ever landed), `next_skill` never lands (stripped, same
    // treatment as `ask_user_empty_note`), but the rest of the patch is
    // applied.
    let (after, after_version) = active.session().state().await.unwrap();
    assert_ne!(
        after_version, before_version,
        "unit 53: a refused-but-committed step must advance the version"
    );
    assert_eq!(
        after.agent.findings, before_findings,
        "findings must be unchanged from before the step"
    );
    assert!(!after.agent.done);
    assert_eq!(
        after.agent.next_skill, None,
        "next_skill must never have landed: a send-back with no findings left is not a handoff"
    );
    assert_eq!(
        after.agent.recent_summary, "looked at the crate",
        "the rest of the patch must be applied"
    );

    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.harness.consecutive_continues, 1,
        "the first refusal must extend the consecutive-continues streak to 1"
    );
}

// ---------------------------------------------------------------------
// Unit 47 + 53: two all-uncited send-backs in a row -- the second becomes
// `think` with the exact `REFUSAL_QUESTION` filled from the all-dropped
// note (`refusal_thinks`, now wired into `ReviewSkill::validate`).
// ---------------------------------------------------------------------

#[tokio::test]
async fn two_all_ungrounded_send_backs_in_a_row_the_second_becomes_think() {
    let root = tempfile::tempdir().unwrap();

    let finding = "something is wrong somewhere, no idea where";
    let turns = vec![
        done_turn(json!({
            "findings": [finding],
            "next_skill": "implement",
        })),
        done_turn(json!({
            "findings": [finding],
            "next_skill": "implement",
        })),
        // A third turn just so a step 3 runs and a prompt is recorded for
        // it -- the substituted think's note (unlike a plain refusal's
        // `Action::Continue`) is appended to the observation that lands in
        // the NEXT prompt, not logged as its own `event: "note"` System
        // entry (see `premature_done.rs`'s equivalent assertion). Its own
        // content is irrelevant to this test.
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "confirm?"}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer("name the line each finding is about and send them again"),
    )]));
    let (mut session, log) = make_file_tools_session_with_thinker(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
    );

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: first refusal -> plain Continue with the exact all-dropped
    // note.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let expected_note = all_dropped_note(&[finding]);
    let notes1 = continued_notes(&log);
    assert_eq!(
        notes1.last(),
        Some(&expected_note),
        "step 1 must produce the exact all-dropped note"
    );
    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(state1.harness.consecutive_continues, 1);

    // Step 2: second refusal in a row -> substituted for `think`, with the
    // exact REFUSAL_QUESTION filled from the all-dropped note.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    assert!(
        !any_rejected(&log),
        "the second refusal must still not be a rejection"
    );

    let (name2, args2) = last_action(&log);
    assert_eq!(name2, "think", "step 2 must be substituted for think");
    let expected_question = REFUSAL_QUESTION.replace("{note}", &expected_note);
    assert_eq!(args2, json!({"question": expected_question}));
    assert_eq!(thinker.calls().len(), 1);
    assert_eq!(thinker.calls()[0].question, expected_question);

    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.harness.consecutive_continues, 1,
        "unit 47's carve-out: a think commit must leave consecutive_continues unchanged"
    );
    assert!(
        !state2.agent.findings.contains(&finding.to_string()),
        "the ungrounded finding must never have landed"
    );
    assert_eq!(state2.agent.next_skill, None);

    // Step 3: run one more step so its prompt is recorded, and check that
    // step 2's think-substitution note (prefix + the exact refusal note)
    // was appended to the observation fed into it.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();
    let expected_think_note = format!("{REFUSAL_THINK_NOTE_PREFIX}{expected_note}");
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 3,
        "expected at least 3 prompts (one per step run so far): {}",
        prompts.len()
    );
    assert!(
        prompts[2].user.contains(&expected_think_note),
        "step 2's think substitution must append the prefix plus the exact all-dropped note to \
         the prompt fed to step 3:\n{}",
        prompts[2].user
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
         all-ungrounded case: refused (harness Continue), not handed off"
    );
    assert!(
        !any_rejected(&log),
        "unit 53: an all-dropped send-back is a refusal, not a rejection"
    );

    let expected_note = all_dropped_note(&["a.txt:999 a line that does not exist"]);
    let notes = continued_notes(&log);
    assert_eq!(
        notes.last(),
        Some(&expected_note),
        "the committed step must carry the exact all-dropped note"
    );

    // Unit 53: the step COMMITS (unlike the old rejection): the version
    // advances past the read, `findings` stays exactly as it was before
    // the step (nothing ungrounded ever landed), and `next_skill` never
    // lands.
    let (after, after_version) = active.session().state().await.unwrap();
    assert_ne!(
        after_version, before_version,
        "unit 53: a refused-but-committed step must advance the version"
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
