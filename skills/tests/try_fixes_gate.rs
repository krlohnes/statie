//! Unit 20: `implement_try_fixes_needs_failing_test`
//! (`skills/src/coding/mod.rs`, wired into `ImplementSkill::validate` right
//! after `write_file_auto_confirm`) -- `try_fixes` is only useful once a
//! failing `test` run is on record for it to work from. `ImplementSkill::
//! tools()` cannot gate on workspace state (`Skill::tools(&self) -> Vec
//! <String>` takes no `cur`/workspace parameter and is genuinely static),
//! so the gate lives in `validate` instead: a `try_fixes` call arriving
//! with no failing `CodingWorkspace::tests` record is substituted for
//! `Action::Tool { name: "test", args: {} }` before the call ever reaches
//! `core/src/session.rs`'s `execute_try_fixes` (which would otherwise log a
//! `tool_error` and reply "no failing test to fix; run test first").
//!
//! This is the end-to-end proof that the substitution actually runs a real
//! `test` tool call through the session (never reaching `execute_try_fixes`
//! at all, so no `tool_error` System entry is ever logged) -- the fixture
//! style is copied from `skills/tests/think_trigger.rs`/`try_fixes_trigger.rs`:
//! a `Session<CodingWorkspace>` wired with the real file tools and a
//! scripted `MockModel`, outcomes/log entries read straight from the
//! `SessionLog`.
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
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
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

/// Same shape as `think_trigger.rs`'s `make_file_tools_session`, with no
/// thinker/fixer attached -- the gate under test fires before either would
/// ever be consulted.
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

/// Whether any `tool_error` System entry has been logged.
fn any_tool_error(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
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

/// A turn that runs action `try_fixes` with an empty patch and no args.
fn try_fixes_turn() -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "try_fixes", "args": {}},
        })
        .to_string(),
    )
}

/// A turn that asks the user a question, ending the run with a `Park` --
/// used purely to drive one more prompt so the prior step's note can be
/// inspected.
fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// Finds the most recent `Action` log entry's `(name, args)`. Same as
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

/// The exact note `implement_try_fixes_needs_failing_test` attaches.
const GATE_NOTE: &str =
    "`try_fixes` needs a failing test to work from, so the harness ran `test` instead.";

// =======================================================================
// (a) No `tests` record at all (fresh workspace): `try_fixes` becomes
// `test` at the same step, the gate note is logged, and no `tool_error`
// entry exists.
// =======================================================================

#[tokio::test]
async fn try_fixes_with_no_tests_record_becomes_test_with_no_tool_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();
    // A `Makefile` with a `test:` target so the substituted `test {}`
    // call's own auto-detect (`tools/src/test_tool.rs`'s
    // `detect_test_command`) finds a real command to run instead of
    // failing with its own unrelated `tool_error` ("no test command
    // detected; pass cmd") -- this test is only about the gate never
    // reaching `execute_try_fixes`, not about the auto-detected command.
    std::fs::write(root.path().join("Makefile"), "test:\n\ttrue\n").unwrap();

    let turns = vec![try_fixes_turn(), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (name1, args1) = last_action(&log);
    assert_eq!(
        name1, "test",
        "a try_fixes call with no tests record at all must be substituted for test"
    );
    assert_eq!(args1, json!({}));

    assert!(
        !any_tool_error(&log),
        "the substitution must run before execute_try_fixes -- no tool_error may be logged"
    );

    // Confirm the workspace now carries a `tests` record from the
    // substituted `test` call, and the gate note reached the next prompt.
    let (state, _v) = active.session().state().await.unwrap();
    assert!(
        state.agent.tests.is_some(),
        "the substituted test call must have written a tests record"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    assert!(
        prompts[1].user.contains(GATE_NOTE),
        "expected the gate note in the second prompt:\n{}",
        prompts[1].user
    );
}

// =======================================================================
// (b) `tests: Some(pass: false)` (a failing test on record): `try_fixes`
// passes through unchanged -- the committed action at that step is
// `try_fixes` itself.
// =======================================================================

#[tokio::test]
async fn try_fixes_with_a_failing_tests_record_passes_through_unchanged() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();

    let turns = vec![
        // Step 1: a failing test -- writes a `tests` record with `pass: false`.
        test_turn("false"),
        // Step 2: try_fixes must pass through unchanged now.
        try_fixes_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state, _v) = active.session().state().await.unwrap();
    let tests = state
        .agent
        .tests
        .clone()
        .expect("the failing test call must have written a tests record");
    assert!(!tests.pass, "the test record must record a failure");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let (name2, args2) = last_action(&log);
    assert_eq!(
        name2, "try_fixes",
        "try_fixes must pass through unchanged when a failing tests record exists"
    );
    assert_eq!(args2, json!({}));

    assert!(!any_tool_error(&log));
}

// =======================================================================
// (c) `tests: Some(pass: true)` (a passing test on record, nothing to
// fix): `try_fixes` is substituted for `test`, just like (a).
// =======================================================================

#[tokio::test]
async fn try_fixes_with_a_passing_tests_record_becomes_test_with_no_tool_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "content\n").unwrap();
    // See the sibling `no_tests_record` test's fixture comment: a
    // `Makefile` so the substituted `test {}` call's own auto-detect finds
    // a real command instead of failing with its own unrelated `tool_error`.
    std::fs::write(root.path().join("Makefile"), "test:\n\ttrue\n").unwrap();

    let turns = vec![
        // Step 1: a passing test -- writes a `tests` record with `pass: true`.
        test_turn("true"),
        // Step 2: try_fixes must be substituted for test -- nothing to fix.
        try_fixes_turn(),
        // Step 3: drives the third prompt so we can inspect it.
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state, _v) = active.session().state().await.unwrap();
    let tests = state
        .agent
        .tests
        .clone()
        .expect("the passing test call must have written a tests record");
    assert!(tests.pass, "the test record must record a pass");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let (name2, args2) = last_action(&log);
    assert_eq!(
        name2, "test",
        "try_fixes with a passing tests record (nothing to fix) must be substituted for test"
    );
    assert_eq!(args2, json!({}));

    assert!(
        !any_tool_error(&log),
        "the substitution must run before execute_try_fixes -- no tool_error may be logged"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3, "expected three model calls");
    assert!(
        prompts[2].user.contains(GATE_NOTE),
        "expected the gate note in the third prompt:\n{}",
        prompts[2].user
    );
}
