//! Two more send-back shapes converted instead of rejected in
//! `ReviewSkill::validate` (`review_sendback_note`, `skills/src/coding/
//! mod.rs`): a live run reached review with a failing test and, meaning
//! "send the work back to implement with these findings" every time, sent
//! `{"patch": {"done": true, "findings": ["..."]}, "action": "done"}` twice
//! (rejected: `done` requires empty `findings`), then `{"patch":
//! {"next_skill": "implement", ...}, "action": <tool call>}` twice
//! (rejected: changing `next_skill` requires action `done`), and the run
//! failed on the retry cap.
//!
//! Fixture style of `skills/tests/review_looks.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `review_looks.rs`,
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
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ReviewSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `review_looks.rs`'s `config`.
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

/// Same shape as `review_looks.rs`'s `ArcModel`.
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

/// Same shape as `review_looks.rs`'s `make_file_tools_session`.
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

/// A turn that reads a file (a harmless, real tool call -- `a.txt` is
/// written to the sandbox root before the session starts in the tests that
/// use this) while also patching the given extra fields.
fn read_file_turn_with_patch(path: &str, patch: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {"type": "tool", "name": "read_file", "args": {"path": path}},
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------
// (a) review + `done: true` + non-empty findings -> handoff to implement,
// not a rejection: `done` is forced back to `false`, `next_skill` is
// injected as `implement` (the model sent none), and a note is logged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_done_true_with_findings_hands_off_to_implement_and_forces_done_false() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        // One look first, so `review_looked_note`'s look-before-approve gate
        // is not what is under test here. It also puts `a.txt` in `seen`
        // with a known line count, so the finding below (citing `a.txt:1`)
        // is grounded (item 12: grounded review findings) and not itself
        // dropped by the unrelated grounding guard.
        read_file_turn_with_patch("a.txt", json!({})),
        done_turn(json!({"done": true, "findings": ["a.txt:1 the test still fails"]})),
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
        "got {outcome2:?}; `done: true` with findings must hand off to implement, not reject"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "the committed workspace's `done` must have been forced back to false"
    );
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 the test still fails".to_string()],
        "findings must be preserved"
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));

    let notes = note_entries(&log);
    let note = notes.last().expect("expected a `note` System entry");
    assert!(
        note.contains("findings are not an approval"),
        "unexpected note: {note}"
    );
    assert!(
        note.contains("`done` set back to false"),
        "unexpected note: {note}"
    );
}

// ---------------------------------------------------------------------
// (b) review + tool action + `next_skill: implement` + non-empty findings
// -> handoff to implement: the tool call is replaced with `done` since the
// model already has findings to send back.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_tool_call_with_next_skill_implement_and_findings_hands_off_to_implement() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![MockTurn::new(
        json!({
            // The finding cites `a.txt:1`, grounded via the `files` entry
            // set in the same patch (item 12: grounded review findings) --
            // `seen` only gets `a.txt` from this step's own `read_file`
            // call AFTER it executes, too late for this step's own
            // grounding check, so `files` (checked directly, no line-count
            // requirement) is what grounds it here instead.
            "patch": {
                "files": [{"path": "a.txt", "note": "under review"}],
                "findings": ["a.txt:1 the test still fails"],
                "next_skill": "implement",
            },
            "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome:?}; a tool call carrying next_skill + findings must hand off, not reject"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.findings,
        vec!["a.txt:1 the test still fails".to_string()]
    );
    assert_eq!(state.agent.next_skill.as_deref(), Some("implement"));
    assert!(!state.agent.done);

    let notes = note_entries(&log);
    let note = notes.last().expect("expected a `note` System entry");
    assert!(
        note.contains("`next_skill` with a tool call is not a handoff"),
        "unexpected note: {note}"
    );
}

// ---------------------------------------------------------------------
// (c) review + tool action + `next_skill: implement` + no findings ->
// Continue: the tool runs, `next_skill` never lands, and a note explains
// why it was dropped.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_tool_call_with_next_skill_and_no_findings_runs_tool_and_drops_next_skill() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"next_skill": "implement"},
                "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
            })
            .to_string(),
        ),
        // Step 2: harmless filler so the third `active.step()` call below
        // (which reads what the note put in *this* prompt) has a scripted
        // turn to answer with.
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "ok"}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; the tool call must run as an ordinary step, not hand off or reject"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must not have landed -- there is nothing to send back"
    );
    assert_eq!(
        state.harness.tool_calls, 1,
        "the tool call must have actually executed"
    );

    // The note is appended to this step's tool observation (see
    // `core/src/session.rs`'s "10b. Validation notes"), which lands in the
    // *next* prompt (same pattern as `premature_done.rs`'s
    // `implement_done_with_open_step_continues_patch_lands_step2_still_open`).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert!(
        prompts.len() >= 2,
        "expected at least 2 model calls, got {}",
        prompts.len()
    );
    assert!(
        prompts[1]
            .user
            .contains("`next_skill` is ignored until you approve"),
        "the second prompt's observation should contain the drop note:\n{}",
        prompts[1].user
    );
}

// ---------------------------------------------------------------------
// (d) review + `done: true` + empty findings after one tool call -> still
// an ordinary approval (`Outcome::Done`), unaffected by the send-back
// conversion. Reference: `review_looks.rs`'s
// `review_approval_after_one_read_file_call_is_accepted` covers this same
// shape end to end (including the look-before-approve gate); this test just
// confirms `review_sendback_note` does not intercept it.
// ---------------------------------------------------------------------

#[tokio::test]
async fn review_done_true_with_empty_findings_after_one_tool_call_is_still_an_approval() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "read_file", "args": {"path": "a.txt"}},
            })
            .to_string(),
        ),
        // A passing test: `review_no_green_note`
        // (`skills/src/coding/mod.rs`) now also requires
        // `cur.harness.green_step` to be set before an approval lands, on
        // top of the look-before-approve gate (already satisfied by the
        // read above).
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "test", "args": {"cmd": "true"}},
            })
            .to_string(),
        ),
        done_turn(json!({"done": true})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();

    assert!(
        matches!(outcome3, Outcome::Done),
        "got {outcome3:?}; an approval (empty findings, tool call and passing test already \
         made) must still land as Done"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done);
    assert!(state.agent.findings.is_empty());
}
