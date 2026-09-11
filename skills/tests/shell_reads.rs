//! Integration test for `shell_read_substitution` (BACKLOG.md item 16,
//! `skills/src/coding/mod.rs`): a `shell` command that is a pure read of
//! one file is substituted with the equivalent `read_file` call before
//! validation, so the read pages, lands in `seen`, and counts for the
//! read guards -- rather than running as an opaque `shell` call those
//! guards cannot see. Uses a mock model plus the *real* file tools in a
//! tempdir (`common::make_file_tools_session`), the same fixture style as
//! `skills/tests/seen.rs`.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{Event, LogKind, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{ImplementSkill, SeenFile};

fn tool_turn(name: &str, args: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": name, "args": args},
        })
        .to_string(),
    )
}

fn shell_turn(cmd: &str) -> MockTurn {
    tool_turn("shell", json!({"cmd": cmd}))
}

fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// A file with `n` lines, each `line {k}` (1-based).
fn numbered_lines(n: u64) -> String {
    (1..=n)
        .map(|k| format!("line {k}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The exact note `shell_read_substitution` attaches, for the given `cmd`.
fn shell_read_note_for(cmd: &str) -> String {
    format!(
        "`shell {cmd}` was a plain read, so the harness ran it as `read_file` instead: the \
         read pages, lands in `seen`, and counts for the read guards. Use `read_file` directly \
         next time."
    )
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

// =======================================================================
// A real session: `shell {cmd: "sed -n '1,100p' a.rs"}` then `ask_user`.
// The step's committed action must be `read_file` with offset 1, limit
// 100; `seen` must record `a.rs` with read range `[[1,100]]`; and the
// next prompt must carry the substitution note.
// =======================================================================

#[tokio::test]
async fn shell_sed_read_becomes_read_file_pages_into_seen_and_notes_the_next_prompt() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let cmd = "sed -n '1,100p' a.rs";
    let turns = vec![shell_turn(cmd), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: the model's `shell` read is substituted for `read_file`.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (name, args) = last_tool_action(&log);
    assert_eq!(
        name, "read_file",
        "the shell read must be substituted for read_file"
    );
    assert_eq!(
        args,
        json!({"path": "a.rs", "offset": 1, "limit": 100}),
        "expected the substituted read_file args"
    );

    let (state1, _v) = active.session().state().await.unwrap();
    let step1 = state1.agent.seen.first().unwrap().step;
    assert_eq!(
        state1.agent.seen,
        vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 250,
            read: vec![[1, 100]],
            edited: vec![],
            read_step: Some(step1),
            step: step1,
        }],
        "the substituted read must page and land in `seen` exactly like a real read_file call"
    );

    // Step 2: ask_user -- drives the second prompt so we can inspect it.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(statie_core::Park::AskUser(_))),
        "got {outcome2:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2, "expected two model calls");
    let second = &prompts[1].user;
    let expected_note = shell_read_note_for(cmd);
    assert!(
        second.contains(&expected_note),
        "expected the substitution note in the second prompt:\n{second}\n\nnote:\n{expected_note}"
    );
}
