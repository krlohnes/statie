//! Unit 51: `ask_user_permission_note` (`skills/src/coding/mod.rs`), the
//! third sibling in the `ask_user_empty_note`/`ask_user_statement_note`/
//! `ask_user_permission_note` chain -- an `ask_user` message that IS a real
//! question (contains `?`, so the first two siblings leave it alone) but
//! only asks leave to make the very change the task already asked for.
//! Three Qwen runs parked on builds 32 and 33 with exactly this shape,
//! right after a single tool error: "Would you like me to proceed with
//! this change now?".
//!
//! This is the end-to-end proof that the conversion actually drives a
//! real `Action::Continue` through `Session::step`'s commit path and that
//! the run keeps going (a following `edit_file` call executes normally) --
//! the unit tests in `skills/src/coding/tests.rs` cover the `validate`-level
//! conversion against a hand-built `ParsedOutput`, but cannot exercise the
//! full session commit/log path.
//!
//! Fixture style copied from `skills/tests/it/premature_done.rs` and
//! `skills/tests/it/error_think.rs`: a `Session<CodingWorkspace>` wired with
//! the real file tools, a scripted `MockModel`, and outcomes/log entries
//! read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `premature_done.rs`
//! and `error_think.rs` inline their own `config`/`make_file_tools_session`:
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
use statie_skills::{CodingWorkspace, ImplementSkill, PERMISSION_ASK_NOTE};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `premature_done.rs`'s `config`.
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

/// Same shape as `premature_done.rs`'s `ArcModel`.
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

/// Same shape as `premature_done.rs`'s `make_file_tools_session`.
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

/// Every `System` entry with `event: "continued"`, most recent last. Same
/// as `premature_done.rs`'s `continued_notes`.
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

/// An `edit_file` turn with the given `old`/`new` text against `path`. Same
/// shape as `error_think.rs`'s `edit_turn`.
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

/// A turn that sends `ask_user` with the given `message` and an empty
/// patch. Same shape as `premature_done.rs`'s `ask_user_turn`.
fn ask_user_turn(message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

/// Main scenario, straight from this brief: implement, one tool error (an
/// `edit_file` whose `old` text is not in the file), then `ask_user`
/// "Would you like me to proceed with this change now?" -- the third live
/// permission-ask shape. The step must commit as `continued` with
/// `PERMISSION_ASK_NOTE` exactly, the run must not park, and a following
/// `edit_file` call must run normally (proving the session did not get
/// stuck waiting on a human).
#[tokio::test]
async fn permission_ask_after_a_tool_error_converts_and_the_run_keeps_going() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "line one\nline two\n").unwrap();

    let turns = vec![
        // Step 1: `edit_file` with `old` text that is not actually in the
        // file -- a tool_error.
        edit_turn("a.txt", "line not present", "replacement"),
        // Step 2: `ask_user` asking permission to make the very change the
        // task already asked for -- must convert to Continue, not park.
        ask_user_turn("Would you like me to proceed with this change now?"),
        // Step 3: a real edit_file call, proving the run kept going and the
        // model's next tool call executes normally.
        edit_turn("a.txt", "line one", "line ONE"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: the failing edit -- an ordinary tool_error.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    assert!(
        log.entries(Some(LogKind::System), 10_000).iter().any(|e| e
            .payload
            .get("event")
            .and_then(Value::as_str)
            == Some("tool_error")),
        "the bad old-text edit must report a tool_error"
    );

    // Step 2: the permission-ask `ask_user`, right after the tool error --
    // must convert to Continue with PERMISSION_ASK_NOTE exactly, and the
    // run must not park.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; a permission ask must convert to Continue, not park"
    );
    assert!(
        !any_rejected(&log),
        "the permission-ask step must not have been rejected"
    );

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert_eq!(
        note, PERMISSION_ASK_NOTE,
        "the continued note must equal PERMISSION_ASK_NOTE exactly"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "no next_skill was ever set in this scenario's patches, so it stays None"
    );

    // Step 3: a following edit_file call runs normally -- the run is not
    // parked on a human who already asked for the work.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; the following edit_file call must run normally"
    );
    let entries = log.entries(Some(LogKind::Action), 10_000);
    let last = entries.last().expect("an Action entry must exist");
    assert_eq!(
        last.payload.get("type").and_then(Value::as_str),
        Some("tool"),
        "the third step's action must be a tool call: {:?}",
        last.payload
    );
    assert_eq!(
        last.payload.get("name").and_then(Value::as_str),
        Some("edit_file"),
        "the third step's tool call must be the scripted edit_file: {:?}",
        last.payload
    );
    let contents = std::fs::read_to_string(root.path().join("a.txt")).unwrap();
    assert_eq!(
        contents, "line ONE\nline two\n",
        "the following edit_file call must have actually applied"
    );
}

/// A genuine question -- not a permission ask -- right after a tool error
/// must stay `AskUser` and park, same as always: the conversion is
/// specific to permission asks, not every question that follows an error.
#[tokio::test]
async fn a_real_question_after_a_tool_error_still_parks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "line one\nline two\n").unwrap();

    let turns = vec![
        edit_turn("a.txt", "line not present", "replacement"),
        ask_user_turn("Which of the two configs is the production one?"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(
            outcome2,
            Outcome::Parked(statie_core::Park::AskUser(ref message))
                if message == "Which of the two configs is the production one?"
        ),
        "a real question must still park as Park::AskUser, got {outcome2:?}"
    );
}
