//! Integration tests for BACKLOG.md unit 31's `edit_lines_needs_fresh_read`
//! trigger (`skills/src/coding/mod.rs`): an `edit_file` call with a `lines`
//! field on a path that was NOT read since its last edit is substituted
//! with the equivalent `read_file` of that range, with `EDIT_LINES_STALE_
//! NOTE` as the step's note. Uses a mock model plus the *real* file tools
//! in a tempdir (`common::make_file_tools_session`), the same fixture style
//! as `skills/tests/it/shell_reads.rs` and `skills/tests/it/write_after_
//! read.rs` (item 15).

use super::common;

use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{Event, LogKind, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{EDIT_LINES_STALE_NOTE, ImplementSkill};

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

fn edit_lines_turn(path: &str, lines: &str, new: &str) -> MockTurn {
    tool_turn(
        "edit_file",
        json!({"path": path, "lines": lines, "new": new}),
    )
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

/// The exact note `edit_lines_needs_fresh_read` attaches, for the given
/// `path`.
fn edit_lines_stale_note_for(path: &str) -> String {
    EDIT_LINES_STALE_NOTE.replace("{path}", path)
}

/// Finds the most recent `Action` log entry's `(name, args)` for a
/// `Action::Tool` entry (the `Action` enum's `#[serde(tag = "type",
/// rename_all = "snake_case")]` gives `"type": "tool"`).
fn last_tool_action(log: &Arc<dyn statie_core::SessionLog>) -> (String, Value) {
    let entries = log.entries(Some(LogKind::Action), 10_000);
    let entry = entries.last().expect("an Action entry must exist");
    assert_eq!(
        entry.payload.get("type").and_then(Value::as_str),
        Some("tool"),
        "expected a tool action entry, got {:?}",
        entry.payload
    );
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
// 1. Read the whole file, edit a line (`old`/`new`, so `seen.edited`
//    becomes non-empty and `read_step` now predates `step`), then --
//    with NO intervening re-read -- send an `edit_file {lines}` call: it
//    must be substituted for the equivalent `read_file` of that range, and
//    the exact stale note must appear in the next prompt.
// =======================================================================

#[tokio::test]
async fn edit_lines_on_a_file_not_reread_since_its_last_edit_becomes_read_file_with_the_stale_note()
{
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(20);
    std::fs::write(root.path().join("a.txt"), &original).unwrap();

    let turns = vec![
        read_turn("a.txt"),
        tool_turn(
            "edit_file",
            json!({"path": "a.txt", "old": "line 5", "new": "line FIVE"}),
        ),
        edit_lines_turn("a.txt", "10-12", "replacement"),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(outcome1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}"); // read_file

    let outcome2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(outcome2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}"); // edit_file (old/new)

    let (state2, _v) = active.session().state().await.unwrap();
    let entry = state2
        .agent
        .seen
        .iter()
        .find(|f| f.path == "a.txt")
        .expect("a.txt should be in seen after the edit");
    assert!(
        !entry.edited.is_empty(),
        "the old/new edit must have recorded an edited range"
    );
    assert_ne!(
        entry.read_step,
        Some(entry.step),
        "the edit, not the earlier read, must be the entry's most recent change"
    );

    // Step 3: `edit_file {lines: "10-12"}` with no intervening reread must
    // be substituted for `read_file {path: a.txt, offset: 10, limit: 3}`.
    let outcome3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(outcome3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    no_tool_error(&log);

    let (name, args) = last_tool_action(&log);
    assert_eq!(
        name, "read_file",
        "the stale edit_file {{lines}} call must be substituted for read_file"
    );
    assert_eq!(
        args,
        json!({"path": "a.txt", "offset": 10, "limit": 3}),
        "expected the substituted read_file args to cover the requested lines range"
    );

    // The file on disk must be untouched -- the edit never actually ran.
    let on_disk = std::fs::read_to_string(root.path().join("a.txt")).unwrap();
    assert!(
        !on_disk.contains("replacement"),
        "the substituted call must not have applied the edit; on disk:\n{on_disk}"
    );

    // Step 4: ask_user -- drives the next prompt so we can inspect it.
    let outcome4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(outcome4).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 4, "expected four model calls");
    let fourth = &prompts[3].user;
    let expected_note = edit_lines_stale_note_for("a.txt");
    assert!(
        fourth.contains(&expected_note),
        "expected the exact stale note in the fourth prompt:\n{fourth}\n\nnote:\n{expected_note}"
    );
}

// =======================================================================
// 2. The positive twin: read the whole file, edit a line, THEN re-read the
//    file (any read counts -- `read_since_last_edit` only checks that the
//    read is the entry's most recent change), then send the same
//    `edit_file {lines}` call: it must run for real this time, since the
//    file was read since its last edit.
// =======================================================================

#[tokio::test]
async fn edit_lines_on_a_file_rereread_since_its_last_edit_runs_for_real() {
    let root = tempfile::tempdir().unwrap();
    let original = numbered_lines(20);
    std::fs::write(root.path().join("a.txt"), &original).unwrap();

    let turns = vec![
        read_turn("a.txt"),
        tool_turn(
            "edit_file",
            json!({"path": "a.txt", "old": "line 5", "new": "line FIVE"}),
        ),
        read_turn("a.txt"),
        edit_lines_turn("a.txt", "10-12", "replacement"),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(outcome1).unwrap(); // read_file

    let outcome2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(outcome2).unwrap(); // edit_file (old/new)

    let outcome3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(outcome3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}"); // read_file (reread)

    let (state3, _v) = active.session().state().await.unwrap();
    let entry = state3
        .agent
        .seen
        .iter()
        .find(|f| f.path == "a.txt")
        .expect("a.txt should be in seen after the reread");
    assert!(
        !entry.edited.is_empty(),
        "edit_file must not have cleared `edited`"
    );
    assert_eq!(
        entry.read_step,
        Some(entry.step),
        "the reread must be the entry's most recent change"
    );

    // Step 4: the same `edit_file {lines: "10-12"}` call now runs for real.
    let outcome4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(outcome4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");
    no_tool_error(&log);

    let (name, args) = last_tool_action(&log);
    assert_eq!(
        name, "edit_file",
        "the edit_file {{lines}} call must run as-is after a fresh reread"
    );
    assert_eq!(
        args,
        json!({"path": "a.txt", "lines": "10-12", "new": "replacement"}),
        "expected the model's own edit_file args, unsubstituted"
    );

    let on_disk = std::fs::read_to_string(root.path().join("a.txt")).unwrap();
    assert!(
        on_disk.contains("replacement"),
        "the edit must have actually applied; on disk:\n{on_disk}"
    );
}
