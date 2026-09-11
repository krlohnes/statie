//! `implement_no_green_runs_test` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` after `implement_bare_done`/
//! `premature_done_note`): the bench battery's implement-side sibling of
//! `review_blind_approval_runs_test` (`skills/tests/review_blind.rs`).
//! Three of four bench misses were `implement` sending `done` where it
//! should have run `test`; the note-only conversion this replaces told it
//! so three times running and the repeat cap parked the run anyway.
//! Telling this model to run the test does not work; running it for it
//! does.
//!
//! This file covers the cases `skills/tests/no_green.rs` does not: the
//! auto-detect `args: {}` shape when `last_test_cmd` is `None` but edits
//! have happened since (as opposed to no test ever run at all, covered in
//! `no_green.rs`), the known-`last_test_cmd` substitution, and the
//! substituted test passing followed by a real `done` proceeding to
//! handoff. `no_green.rs` covers: no test ever run (`last_test_cmd: None`,
//! `edits_since_test: 0`) -- runs test; a failing run with nothing edited
//! since -- Continue fallback, unchanged note; an open plan step -- the
//! existing premature-done note takes priority.
//!
//! Fixture style of `skills/tests/no_green.rs`/`skills/tests/review_blind.rs`:
//! a `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and outcomes/log entries read straight from the
//! `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `no_green.rs`/
//! `review_blind.rs` inline their own `config`/`make_file_tools_session`:
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
use statie_skills::{CodingWorkspace, ImplementSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `no_green.rs`'s `config`.
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

/// Same shape as `no_green.rs`'s `ArcModel`.
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

/// Same shape as `no_green.rs`'s `make_file_tools_session`.
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

/// Every `System` entry with `event: "continued"`, most recent last.
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

/// A turn that writes a file (a harmless real edit, to drive
/// `edits_since_test` above 0).
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

/// The exact note `implement_no_green_runs_test` attaches when it
/// substitutes a real `test` call for a `done` sent with no passing run
/// (`skills/src/coding/mod.rs`).
const RUNS_TEST_NOTE: &str = "you sent `done` without a passing test run, so the harness ran \
                               the tests for you (below). If they pass, send `done` again; if \
                               they fail, fix what they report first.";

/// The `## Observation` section's body of the given recorded prompt
/// (`statie_models::RecordedPrompt::user`). Same as `review_blind.rs`'s.
fn observation_body(prompt: &str) -> &str {
    let idx = prompt
        .find("## Observation")
        .expect("prompt should carry an Observation section");
    let heading = "## Observation\n";
    prompt[idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading")
}

/// Finds the most recent `Action` log entry's `name`/`args`. Same as
/// `review_blind.rs`'s.
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

fn seed_plan_done_turn() -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
            "action": {"type": "ask_user", "message": "seed?"},
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------
// edits_since_test > 0, last_test_cmd: Some(cmd) -> substituted test call
// uses {"cmd": cmd}; note lands on the tool observation; next_skill/done
// stripped; no park.
// ---------------------------------------------------------------------

#[tokio::test]
async fn edits_since_last_test_with_known_cmd_reruns_that_cmd() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        seed_plan_done_turn(),
        // A passing test establishes last_test_cmd and green_step.
        test_turn("true"),
        // An edit after the passing test: green_step must clear on the
        // NEXT test call, but for now edits_since_test climbs to 1 and
        // green_step is still Some (untouched by a non-test tool call) --
        // so to reach `green_step: None` with edits since, break the
        // build with a failing test right after the edit.
        write_file_turn("a.txt", "content\n"),
        test_turn("false"),
        // Another edit after the failing test -> edits_since_test: 1,
        // green_step: None, last_test_cmd: Some("false").
        write_file_turn("b.txt", "more content\n"),
        // done, with edits since the last (failing) test -> must re-run
        // the same last_test_cmd ("false"), not auto-detect.
        done_turn(json!({"next_skill": "review"})),
        // One more turn so the substituted call's own observation prompt
        // can be captured.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    for _ in 0..5 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the last test call failed");
    assert_eq!(state.harness.last_test_cmd, Some("false".to_string()));
    assert_eq!(
        state.harness.edits_since_test, 1,
        "one write_file happened after the failing test"
    );
    let tool_calls_before = state.harness.tool_calls;

    let rec6 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome6 = log.append_record(rec6).unwrap();
    assert!(
        matches!(outcome6, Outcome::Continue),
        "got {outcome6:?}; edits since a failing run must re-run the test, not park"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must have been stripped -- no handoff happened"
    );
    assert_eq!(
        state.harness.tool_calls,
        tool_calls_before + 1,
        "the substituted test call must have executed exactly one more tool call"
    );

    let notes = continued_notes(&log);
    assert!(
        notes.is_empty(),
        "the runs-test note is appended to the tool observation, not a `continued` entry: \
         {notes:?}"
    );

    let (name, args) = last_action(&log);
    assert_eq!(name, "test");
    assert_eq!(
        args,
        json!({"cmd": "false"}),
        "the substituted call must rerun the SAME command implement last verified with"
    );
}

// ---------------------------------------------------------------------
// After the substituted test call, the NEXT prompt's Observation section
// carries the runs-test note verbatim (same mechanism
// `review_blind_approval_runs_test`'s substitution uses -- see
// `skills/tests/review_blind.rs`'s `observation_body` helper).
// ---------------------------------------------------------------------

#[tokio::test]
async fn edits_since_last_test_substituted_call_note_lands_in_next_observation() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        seed_plan_done_turn(),
        test_turn("false"),
        write_file_turn("a.txt", "content\n"),
        done_turn(json!({"next_skill": "review"})),
        // One more turn so the substituted call's own observation prompt
        // can be captured.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.edits_since_test, 1);
    assert_eq!(state.harness.last_test_cmd, Some("false".to_string()));

    // Step 4: done -> substituted test call.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec4).unwrap();

    let (name, args) = last_action(&log);
    assert_eq!(name, "test");
    assert_eq!(args, json!({"cmd": "false"}));

    // Step 5: capture the prompt built right after the substituted call,
    // carrying its observation.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec5).unwrap();

    let prompts = model.prompts();
    let last = prompts.last().expect("a prompt must have been recorded");
    let body = observation_body(&last.user);
    assert!(
        body.contains(RUNS_TEST_NOTE),
        "the substituted test's observation must carry the runs-test note verbatim: {body}"
    );
}

// ---------------------------------------------------------------------
// The substituted test call passing, followed by a real `done` ->
// handoff proceeds normally (the harness's own substituted run counts the
// same as a model-initiated one).
// ---------------------------------------------------------------------

#[tokio::test]
async fn substituted_test_passing_then_done_hands_off() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "PASS\n").unwrap();

    let turns = vec![
        seed_plan_done_turn(),
        // Break green with a failing test.
        test_turn("sh -c 'grep -q NOPE a.txt'"),
        // Edit since the failing test: edits_since_test becomes 1.
        write_file_turn("marker.txt", "touch\n"),
        // done: green_step is None and edits_since_test > 0 -> substitutes
        // a rerun of the exact last_test_cmd ("grep -q NOPE a.txt"), which
        // NOW passes because the file on disk was edited out-of-band below
        // before this step runs.
        done_turn(json!({"next_skill": "review"})),
        // A second `done`, sent by the model after seeing the substituted
        // run pass -- must hand off for real this time.
        done_turn(json!({"next_skill": "review"})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Steps 1-3: seed, failing test, edit.
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None);
    assert_eq!(state.harness.edits_since_test, 1);
    assert_eq!(
        state.harness.last_test_cmd,
        Some("sh -c 'grep -q NOPE a.txt'".to_string())
    );

    // Make the previously-failing command pass before the substitution
    // reruns it.
    std::fs::write(root.path().join("a.txt"), "NOPE\n").unwrap();

    // Step 4: done -> substituted rerun of "grep -q NOPE a.txt", now
    // passing.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "the substituted rerun must have passed and set green_step"
    );
    assert_eq!(
        state.harness.edits_since_test, 0,
        "the substituted test call itself resets edits_since_test"
    );

    // Step 5: a real `done` after the substituted pass -> hands off.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(
        matches!(outcome5, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome5:?}; done after the substituted run passed must hand off to review"
    );
    assert!(!any_rejected(&log));
}
