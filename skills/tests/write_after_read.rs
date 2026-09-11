//! Integration tests for BACKLOG.md item 15: `write_file`'s whole-file
//! rewrite guard (`tools/src/write_file.rs`'s `MAX_EXISTING_FILE_LINES`) is
//! skipped when the harness's own `seen` record (`CodingWorkspace::seen`)
//! already shows a full, recent read of the target file, via `write_file_
//! confirmed_by_seen`/`write_file_auto_confirm` wired into `ImplementSkill::
//! validate` (`skills/src/coding/mod.rs`). Uses a mock model plus the
//! *real* file tools in a tempdir (`common::make_file_tools_session`), the
//! same fixture style as `skills/tests/seen.rs`.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{Event, LogKind, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::ImplementSkill;

fn tool_turn(name: &str, args: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": name, "args": args},
        })
        .to_string(),
    )
}

fn read_turn(path: &str) -> MockTurn {
    tool_turn("read_file", json!({"path": path}))
}

fn write_turn(path: &str, content: &str) -> MockTurn {
    tool_turn("write_file", json!({"path": path, "content": content}))
}

fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// A file with `n` lines, each `line {k}` (1-based), each occurring
/// exactly once.
fn numbered_lines(n: u64) -> String {
    (1..=n)
        .map(|k| format!("line {k}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

async fn run_step<K>(
    active: &mut statie_core::Active<'_, K>,
    log: &Arc<dyn statie_core::SessionLog>,
) -> Outcome
where
    K: statie_core::Skill,
    K::Workspace: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec).unwrap()
}

fn no_tool_error(log: &Arc<dyn statie_core::SessionLog>) {
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let err = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"));
    assert!(
        err.is_none(),
        "expected no tool_error, got: {sys_entries:?}"
    );
}

// =======================================================================
// 1. A 62-line file, read whole (`read_file` with no offset/limit: the
//    default 100-line window covers all 62 lines in one call, so `seen`
//    already covers 1..=62 afterward), then `write_file` with new 62-line
//    content: the write applies (file content changed on disk) and no
//    tool_error is logged.
// =======================================================================

#[tokio::test]
async fn write_file_applies_after_a_full_read_of_a_62_line_file() {
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(62);
    std::fs::write(root.path().join("a.rs"), &original).unwrap();

    let replaced = (1..=62)
        .map(|k| format!("changed {k}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let turns = vec![
        read_turn("a.rs"),
        write_turn("a.rs", &replaced),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await; // read_file (whole file)
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, _v) = active.session().state().await.unwrap();
    let entry = state1
        .agent
        .seen
        .iter()
        .find(|f| f.path == "a.rs")
        .expect("a.rs should be in seen after the read");
    assert_eq!(
        entry.lines, 62,
        "the 62-line file's length must be recorded"
    );
    assert_eq!(
        entry.read,
        vec![[1, 62]],
        "the default read window (100 lines) must cover the whole 62-line file in one call"
    );

    let outcome2 = run_step(&mut active, &log).await; // write_file (confirmed by seen)
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    no_tool_error(&log);

    let on_disk = std::fs::read_to_string(root.path().join("a.rs")).unwrap();
    assert_eq!(
        on_disk, replaced,
        "write_file must have applied the new content, not been refused"
    );
}

// =======================================================================
// 2. `write_file` on the same 62-line file with no prior read is refused
//    with the extended message (the harness never set confirmed_full_read,
//    since seen has no entry for the path at all).
// =======================================================================

#[tokio::test]
async fn write_file_without_a_prior_read_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(62);
    std::fs::write(root.path().join("a.rs"), &original).unwrap();

    let replaced = "replaced\n".to_string();

    let turns = vec![write_turn("a.rs", &replaced), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    run_step(&mut active, &log).await; // write_file (refused)

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let tool_error = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .unwrap_or_else(|| panic!("expected a tool_error system entry, got {sys_entries:?}"));
    let reason = tool_error
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason,
        "tool execution failed: a.rs exists with 62 lines; write_file replaces the whole file and you would be writing it from memory. Use edit_file (old/new, or after/before + new) for changes to an existing file. If the file is beyond repair, `revert_file` restores its content from before this session's first edit. If you have read the whole file since your last edit of it, the harness allows the rewrite automatically.",
        "expected the extended refusal message, got: {reason}"
    );

    let on_disk = std::fs::read_to_string(root.path().join("a.rs")).unwrap();
    assert_eq!(
        on_disk, original,
        "the file must be left untouched by the refused write"
    );
}

// =======================================================================
// 3. The motivating scenario: a 62-line file is read, edited (`edit_file`
//    on one line), then read again in full, then `write_file`'d with new
//    content. `edit_file` never clears `edited`, so the old rule ("`edited`
//    empty") refused this write even though the second read came after the
//    edit; the fixed rule (coverage full AND (`edited` empty OR `read_step
//    == step`)) must allow it: the write applies and no tool_error is
//    logged.
// =======================================================================

#[tokio::test]
async fn write_file_applies_after_an_edit_followed_by_a_full_reread() {
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(62);
    std::fs::write(root.path().join("a.rs"), &original).unwrap();

    let replaced = (1..=62)
        .map(|k| format!("changed {k}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let turns = vec![
        read_turn("a.rs"),
        tool_turn(
            "edit_file",
            json!({"path": "a.rs", "old": "line 25", "new": "line twenty-five"}),
        ),
        read_turn("a.rs"),
        write_turn("a.rs", &replaced),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await; // read_file (whole file)
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let outcome2 = run_step(&mut active, &log).await; // edit_file (one line)
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let outcome3 = run_step(&mut active, &log).await; // read_file (whole file, again)
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");

    let (state3, _v) = active.session().state().await.unwrap();
    let entry = state3
        .agent
        .seen
        .iter()
        .find(|f| f.path == "a.rs")
        .expect("a.rs should be in seen after the reread");
    assert!(
        !entry.edited.is_empty(),
        "edit_file must not have cleared `edited`"
    );
    assert_eq!(
        entry.read_step,
        Some(entry.step),
        "the reread must be the entry's most recent change"
    );

    let outcome4 = run_step(&mut active, &log).await; // write_file (confirmed by seen)
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    no_tool_error(&log);

    let on_disk = std::fs::read_to_string(root.path().join("a.rs")).unwrap();
    assert_eq!(
        on_disk, replaced,
        "write_file must have applied the new content after the edit-then-full-reread, not \
         been refused"
    );
}

// =======================================================================
// 4. The negative twin: `read_file`, `edit_file`, then `write_file` with no
//    intervening reread is refused with the extended message -- the read
//    from step 1 predates the edit from step 2, so `write_file_confirmed_
//    by_seen` must say no.
// =======================================================================

#[tokio::test]
async fn write_file_after_an_edit_with_no_reread_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(62);
    std::fs::write(root.path().join("a.rs"), &original).unwrap();

    let replaced = "replaced\n".to_string();

    let turns = vec![
        read_turn("a.rs"),
        tool_turn(
            "edit_file",
            json!({"path": "a.rs", "old": "line 25", "new": "line twenty-five"}),
        ),
        write_turn("a.rs", &replaced),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    run_step(&mut active, &log).await; // read_file
    run_step(&mut active, &log).await; // edit_file
    run_step(&mut active, &log).await; // write_file (refused)

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let tool_error = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .unwrap_or_else(|| panic!("expected a tool_error system entry, got {sys_entries:?}"));
    let reason = tool_error
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason,
        "tool execution failed: a.rs exists with 62 lines; write_file replaces the whole file and you would be writing it from memory. Use edit_file (old/new, or after/before + new) for changes to an existing file. If the file is beyond repair, `revert_file` restores its content from before this session's first edit. If you have read the whole file since your last edit of it, the harness allows the rewrite automatically.",
        "expected the extended refusal message, got: {reason}"
    );

    let on_disk = std::fs::read_to_string(root.path().join("a.rs")).unwrap();
    assert_ne!(
        on_disk, replaced,
        "the file must be left untouched by the refused write"
    );
}
