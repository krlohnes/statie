//! Unit 32: a tool call with no name is named by its arguments; an anchor
//! miss names `lines`.
//!
//! `infer_empty_tool_name` (`skills/src/coding/mod.rs`), wired as the FIRST
//! line of `PlanSkill`/`ImplementSkill`/`ReviewSkill`'s `validate`, before
//! `strip_harness_owned`: a nameless `Action::Tool` call whose `args` shape
//! `infer_tool_name` recognises is renamed in place (args unchanged) and the
//! step's observation carries `INFERRED_TOOL_NOTE_PREFIX` followed by the
//! guessed tool name in backticks and a period. A shape `infer_tool_name`
//! can't guess from is left with an empty name, which then falls through to
//! the ordinary empty-name rejection (item 29, `core/src/session.rs`).
//!
//! End-to-end proof through a real `Session<CodingWorkspace>` with the real
//! file tools, a scripted `MockModel`, and (for the `think` case) a
//! `MockThinker` attached via `Session::with_thinker` -- fixture style copied
//! from `skills/tests/it/think_trigger.rs` (thinker wiring) and
//! `skills/tests/it/harness_owned.rs` (`note_texts` helper) /
//! `skills/tests/it/router.rs` (`rejected_handoff.rs`'s exact hand-off text).
//!
//! Deliberately not `mod common;`, for the same reason those files inline
//! their own `config`/session builder: `skills/tests/it/common/mod.rs`
//! exports items this file has no use for, and `-D warnings` fails a binary
//! that leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, StepSink, ThinkOutput,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, INFERRED_TOOL_NOTE_PREFIX, ImplementSkill, PlanSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

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

/// A turn sending a nameless tool call with the given `args`.
fn nameless_tool_turn(args: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "", "args": args},
        })
        .to_string(),
    )
}

/// Unit 42: a turn sending a tool call whose `name` is a path (not empty,
/// not a real tool name) and whose `args` are the given shape.
fn path_named_tool_turn(name: &str, args: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": name, "args": args},
        })
        .to_string(),
    )
}

/// A turn that asks the user something, used purely to drive one more step
/// so the harness builds (and the mock model records) the next prompt --
/// the only place a validation note (`ValidatedStep::notes`, step 10b in
/// `core/src/session.rs`) is observable when it merges into an existing
/// `pending_observation` rather than landing as its own `event: "note"`
/// System entry. Mirrors `skills/tests/it/edit_lines.rs`'s step-4
/// `ask_user_turn`.
fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

fn note_texts(log: &Arc<dyn statie_core::SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| {
            e.payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn tool_rejected_payload(log: &Arc<dyn statie_core::SessionLog>) -> Option<Value> {
    log.entries(Some(LogKind::System), 10_000)
        .into_iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected"))
        .map(|e| e.payload)
}

fn last_action(log: &Arc<dyn statie_core::SessionLog>) -> (String, Value) {
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

// =======================================================================
// 1. A nameless call with `{"question": "why?"}` runs `think`: the mock
//    thinker is called with that exact question, and the step's note is
//    exactly the INFERRED text for `think`.
// =======================================================================

#[tokio::test]
async fn implement_infers_think_from_a_nameless_question_call() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        nameless_tool_turn(json!({"question": "why?"})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let thinker = Arc::new(statie_core::MockThinker::new(vec![Ok(
        ThinkOutput::answer("because"),
    )]));
    let (mut session, log) = make_file_tools_session_with_thinker(
        Arc::clone(&model),
        config(&root),
        Arc::clone(&thinker) as _,
    );

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, args) = last_action(&log);
    assert_eq!(name, "think", "the nameless call must run as think");
    assert_eq!(args, json!({"question": "why?"}));

    assert_eq!(
        thinker.calls().len(),
        1,
        "the mock thinker must have been called exactly once"
    );
    assert_eq!(thinker.calls()[0].question, "why?");

    let notes = note_texts(&log);
    assert!(
        notes.is_empty(),
        "the note merges into the pending observation, not a separate System entry: {notes:?}"
    );

    // Drive one more step so the harness builds the next prompt from the
    // merged observation, and inspect it for the exact INFERRED note text
    // (same technique as `skills/tests/it/edit_lines.rs`'s stale-note
    // check).
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note = format!("{INFERRED_TOOL_NOTE_PREFIX}`think`.");
    assert!(
        second.contains(&expected_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
}

// =======================================================================
// 2. A nameless call with `{"path": "a.rs", "content": "x"}` runs
//    `write_file`: asserts the exact outcome the code does (a new file, no
//    prior read needed for item 15's guard, so the write applies and no
//    tool_error is logged), plus the exact INFERRED note for write_file.
// =======================================================================

#[tokio::test]
async fn implement_infers_write_file_from_a_nameless_path_content_call() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        nameless_tool_turn(json!({"path": "a.rs", "content": "x"})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, _args) = last_action(&log);
    assert_eq!(
        name, "write_file",
        "the nameless call must run as write_file"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let tool_error = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"));
    assert!(
        !tool_error,
        "a new file's write_file must not be refused: {sys_entries:?}"
    );

    let on_disk = std::fs::read_to_string(root.path().join("a.rs")).unwrap();
    assert_eq!(on_disk, "x", "write_file must have applied");

    // Drive one more step so the harness builds the next prompt from the
    // merged observation, and inspect it for the exact INFERRED note text.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note = format!("{INFERRED_TOOL_NOTE_PREFIX}`write_file`.");
    assert!(
        second.contains(&expected_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
}

// =======================================================================
// 3. A nameless call with empty `args` stays empty: `infer_tool_name`
//    guesses nothing, so the item 29 empty-name rejection text fires
//    unchanged, and no INFERRED note is attached.
// =======================================================================

#[tokio::test]
async fn implement_empty_args_stays_empty_and_gets_the_item_29_empty_name_rejection() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![nameless_tool_turn(json!({}))];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let payload = tool_rejected_payload(&log).expect("expected a tool_rejected system log entry");
    assert_eq!(payload.get("name").and_then(Value::as_str), Some(""));
    assert_eq!(payload.get("skill_with_tool"), Some(&Value::Null));

    let active_tools = ImplementSkill::new().tools();
    let expected = format!(
        "action.name is empty: put the tool's name in `name` and its arguments in `args`; \
         available tools: {active_tools:?}"
    );
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected.as_str())
    );

    let notes = note_texts(&log);
    assert!(
        notes.is_empty(),
        "an unguessable nameless call must get no INFERRED note, got {notes:?}"
    );
}

// =======================================================================
// 4. With `plan` active, a nameless call with `{"path": "a.rs", "old": "a",
//    "new": "b"}` becomes `edit_file` (INFERRED note attached), and is then
//    rejected by the item 29 inactive-tool check naming `implement` (`plan`
//    has no `edit_file`) -- both notes present: the INFERRED note (from
//    `validate`) and the item 29 hand-off text (from the `tool_rejected`
//    system entry).
// =======================================================================

#[tokio::test]
async fn plan_infers_edit_file_then_gets_the_item_29_rejection_naming_implement() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        nameless_tool_turn(json!({"path": "a.rs", "old": "a", "new": "b"})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    // The INFERRED note (validate-level, unit 32) merges into the same
    // pending observation the item 29 rejection sets, rather than landing
    // as its own System entry -- see `implement_infers_think_...`'s doc,
    // above, for why.
    let notes = note_texts(&log);
    assert!(
        notes.is_empty(),
        "the note merges into the rejection's pending observation: {notes:?}"
    );

    // The item 29 hand-off text (naming implement), from the tool_rejected
    // system entry (rejected_handoff.rs's exact text) -- the rejected call
    // is named `edit_file`, not the original empty name, proving the rename
    // landed before the inactive-tool check ran.
    let payload = tool_rejected_payload(&log).expect("expected a tool_rejected system log entry");
    assert_eq!(
        payload.get("name").and_then(Value::as_str),
        Some("edit_file"),
        "the rejected call must be named edit_file, not the original empty name: {payload:?}"
    );
    assert_eq!(
        payload.get("skill_with_tool").and_then(Value::as_str),
        Some("implement")
    );
    let expected = "tool `edit_file` is not available in skill `plan`; `implement` has it. \
                    Finish this skill's job, then hand off: {\"patch\": {\"next_skill\": \
                    \"implement\"}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": \
                    {}}, \"ack_inbox\": []}";
    assert_eq!(
        payload.get("reason").and_then(Value::as_str),
        Some(expected)
    );

    // Both notes present, as the acceptance criteria requires: drive one
    // more step so the harness builds the next prompt from the merged
    // observation, and check it carries BOTH the INFERRED note and the
    // item 29 hand-off text.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_inferred_note = format!("{INFERRED_TOOL_NOTE_PREFIX}`edit_file`.");
    assert!(
        second.contains(&expected_inferred_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n\
         {expected_inferred_note}"
    );
    assert!(
        second.contains(expected),
        "expected the exact item 29 hand-off text in the second prompt:\n{second}\n\ntext:\n\
         {expected}"
    );
}

// =======================================================================
// Unit 42: a call whose `name` is a path, not empty and not one of the
// skill's active tools, is renamed the same way an empty `name` is --
// `infer_empty_tool_name`'s second case, wired the same first line of
// `validate`.
// =======================================================================

// (a) `name: "src/lib.rs"`, `args: {"offset": 1, "limit": 40}` (no `path`
// key) runs `read_file` on `src/lib.rs`: `path` is inserted from the
// original `name` before inference, and the exact note names both.

#[tokio::test]
async fn implement_path_named_call_with_offset_limit_runs_read_file_on_that_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        (1..=60)
            .map(|k| format!("line {k}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let turns = vec![
        path_named_tool_turn("src/lib.rs", json!({"offset": 1, "limit": 40})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, args) = last_action(&log);
    assert_eq!(
        name, "read_file",
        "the path-named call must run as read_file"
    );
    assert_eq!(
        args,
        json!({"offset": 1, "limit": 40, "path": "src/lib.rs"})
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let tool_error = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"));
    assert!(
        !tool_error,
        "the inferred read_file must not be refused: {sys_entries:?}"
    );
    let tool_rejected = tool_rejected_payload(&log);
    assert!(
        tool_rejected.is_none(),
        "read_file is active for implement, so no rejection: {tool_rejected:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note =
        format!("{INFERRED_TOOL_NOTE_PREFIX}`read_file` (the name was `src/lib.rs`).");
    assert!(
        second.contains(&expected_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
}

// (b) `name: "src/lib.rs"`, `args: {"path": "src/lib.rs", "old": "a", "new":
// "b"}` (already has its own `path`) runs `edit_file`: `path` is left as
// sent (not overwritten), and the exact note names both.

#[tokio::test]
async fn implement_path_named_call_with_its_own_path_key_runs_edit_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "a\n").unwrap();

    let turns = vec![
        path_named_tool_turn(
            "src/lib.rs",
            json!({"path": "src/lib.rs", "old": "a", "new": "b"}),
        ),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, args) = last_action(&log);
    assert_eq!(
        name, "edit_file",
        "the path-named call must run as edit_file"
    );
    assert_eq!(
        args,
        json!({"path": "src/lib.rs", "old": "a", "new": "b"}),
        "args' own path key must be left as sent, not overwritten"
    );

    let on_disk = std::fs::read_to_string(root.path().join("src/lib.rs")).unwrap();
    assert_eq!(on_disk, "b\n", "edit_file must have applied");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note =
        format!("{INFERRED_TOOL_NOTE_PREFIX}`edit_file` (the name was `src/lib.rs`).");
    assert!(
        second.contains(&expected_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
}

// (c) Unit 52: `name: "src/lib.rs"`, `args: {}` now runs `read_file`: after
// `path` is inserted from the original `name`, `args` is `{"path":
// "src/lib.rs"}` -- `path` as its only key, a bare path-named call that can
// only be a read (see `infer_empty_tool_name`'s doc, unit 52) -- so
// inference no longer falls through to the item-29 rejection the way it
// used to when this shape returned `None`. The committed action is
// `read_file` and the observation is the file's contents.

#[tokio::test]
async fn implement_path_named_call_with_empty_args_infers_read_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "fn lib() {}\n").unwrap();

    let turns = vec![
        path_named_tool_turn("src/lib.rs", json!({})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, args) = last_action(&log);
    assert_eq!(
        name, "read_file",
        "the bare path-named call must run as read_file"
    );
    assert_eq!(args, json!({"path": "src/lib.rs"}));

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let tool_error = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"));
    assert!(
        !tool_error,
        "the inferred read_file must not be refused: {sys_entries:?}"
    );
    let tool_rejected = tool_rejected_payload(&log);
    assert!(
        tool_rejected.is_none(),
        "read_file is active for implement, so no rejection: {tool_rejected:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note =
        format!("{INFERRED_TOOL_NOTE_PREFIX}`read_file` (the name was `src/lib.rs`).");
    assert!(
        second.contains(&expected_note),
        "expected the exact INFERRED note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
    assert!(
        second.contains("fn lib() {}"),
        "expected the observation to carry the file's contents:\n{second}"
    );
}

// (d) `name: "read_file"` with `{"path":"a","limit":5}` is untouched: an
// active tool name is never rewritten, even though it also happens to look
// like a plain word -- no note at all.

#[tokio::test]
async fn implement_active_tool_name_read_file_is_left_untouched() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a"),
        (1..=10)
            .map(|k| format!("line {k}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let turns = vec![
        path_named_tool_turn("read_file", json!({"path": "a", "limit": 5})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (name, args) = last_action(&log);
    assert_eq!(name, "read_file");
    assert_eq!(args, json!({"path": "a", "limit": 5}));

    let tool_rejected = tool_rejected_payload(&log);
    assert!(
        tool_rejected.is_none(),
        "an active tool name must never be rejected: {tool_rejected:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec2).unwrap();

    let notes = note_texts(&log);
    assert!(
        notes.is_empty(),
        "an active tool name must never get an INFERRED note, got {notes:?}"
    );
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    assert!(
        !second.contains(INFERRED_TOOL_NOTE_PREFIX),
        "an active tool name must never get an INFERRED note in the prompt:\n{second}"
    );
}
