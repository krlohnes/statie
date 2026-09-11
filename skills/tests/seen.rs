//! Integration tests for `CodingWorkspace::seen` (milestone 10): the
//! harness-maintained per-file record of length, read/edited line ranges,
//! and last-touch step, kept via `Skill::after_tool` after a successful
//! `read_file`/`edit_file`/`write_file`/`revert_file` call. Uses a mock
//! model plus the *real* file tools in a tempdir (`common::
//! make_file_tools_session`), the same fixture style as
//! `skills/tests/commitment_rules.rs`/`working_set.rs`.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{Event, LogKind, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{ImplementSkill, PlanSkill, SeenFile};

fn tool_turn(name: &str, args: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": name, "args": args},
        })
        .to_string(),
    )
}

fn read_turn(path: &str, offset: u64, limit: u64) -> MockTurn {
    tool_turn(
        "read_file",
        json!({"path": path, "offset": offset, "limit": limit}),
    )
}

fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// A file with `n` lines, each `line {k}` (1-based), each occurring
/// exactly once -- so a range of consecutive lines is a valid unique
/// `edit_file` anchor.
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

// =======================================================================
// 1-2. read_file 1-100 then 101-200 of a 250-line file: `seen` accumulates
//    the merged range and updates `lines`/`step`.
// =======================================================================

#[tokio::test]
async fn read_file_records_and_extends_the_range() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let turns = vec![
        read_turn("a.rs", 1, 100),
        read_turn("a.rs", 101, 100),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await;
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state1, _v) = active.session().state().await.unwrap();
    let step1 = state1.agent.seen.first().map(|f| f.step);
    assert_eq!(
        state1.agent.seen,
        vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 250,
            read: vec![[1, 100]],
            edited: vec![],
            read_step: step1,
            step: step1.unwrap(),
        }]
    );

    let outcome2 = run_step(&mut active, &log).await;
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (state2, _v) = active.session().state().await.unwrap();
    let step2 = state2.agent.seen.first().map(|f| f.step);
    assert_eq!(
        state2.agent.seen,
        vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 250,
            read: vec![[1, 200]],
            edited: vec![],
            read_step: step2,
            step: step2.unwrap(),
        }]
    );
    assert!(
        step2.unwrap() > step1.unwrap(),
        "the second read's recorded step must be later than the first's"
    );
}

// =======================================================================
// 3. An edit reported at lines 150-156: `edited: [[150, 156]]`.
// =======================================================================

#[tokio::test]
async fn edit_file_records_the_edited_range() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let old = (150..=156)
        .map(|k| format!("line {k}"))
        .collect::<Vec<_>>()
        .join("\n");
    let new = (150..=156)
        .map(|k| format!("changed {k}"))
        .collect::<Vec<_>>()
        .join("\n");

    let turns = vec![
        tool_turn("edit_file", json!({"path": "a.rs", "old": old, "new": new})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.seen,
        vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 0,
            read: vec![],
            edited: vec![[150, 156]],
            read_step: None,
            step: 1,
        }]
    );
}

// =======================================================================
// 4. write_file of a new 12-line file: its own entry, `edited [[1,12]]`,
//    `read []`.
// =======================================================================

#[tokio::test]
async fn write_file_of_a_new_file_records_its_own_entry() {
    let root = tempfile::tempdir().unwrap();

    let content = numbered_lines(12);
    let turns = vec![
        tool_turn("write_file", json!({"path": "new.rs", "content": content})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.seen,
        vec![SeenFile {
            path: "new.rs".to_string(),
            lines: 12,
            read: vec![],
            edited: vec![[1, 12]],
            read_step: None,
            step: 1,
        }]
    );
}

// =======================================================================
// 5. revert_file clears both ranges.
// =======================================================================

#[tokio::test]
async fn revert_file_clears_read_and_edited() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let old = (150..=156)
        .map(|k| format!("line {k}"))
        .collect::<Vec<_>>()
        .join("\n");
    let new = (150..=156)
        .map(|k| format!("changed {k}"))
        .collect::<Vec<_>>()
        .join("\n");

    let turns = vec![
        read_turn("a.rs", 1, 100),
        tool_turn("edit_file", json!({"path": "a.rs", "old": old, "new": new})),
        tool_turn("revert_file", json!({"path": "a.rs"})),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    run_step(&mut active, &log).await; // read
    run_step(&mut active, &log).await; // edit
    let outcome = run_step(&mut active, &log).await; // revert
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    let step = state.agent.seen.first().map(|f| f.step);
    assert_eq!(
        state.agent.seen,
        vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 250,
            read: vec![],
            edited: vec![],
            read_step: None,
            step: step.unwrap(),
        }]
    );
}

// =======================================================================
// 6. A model patch setting `seen` is stripped (not rejected): the step
//    commits, `seen` is left exactly as the harness had it, no `rejected`
//    entry is logged, and a note explaining the drop is recorded.
// =======================================================================

#[tokio::test]
async fn model_patch_touching_seen_is_stripped_and_noted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    // Step 1: a real read, so `seen` is non-empty afterward.
    // Step 2: the model tries to patch `seen` directly alongside an
    // unrelated action.
    let turns = vec![
        read_turn("a.rs", 1, 10),
        MockTurn::new(
            json!({
                "patch": {"seen": [{"path": "b.rs", "lines": 1, "read": [], "edited": [], "step": 99}]},
                "action": {"type": "ask_user", "message": "confirm?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await;
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let (state1, _v) = active.session().state().await.unwrap();
    let seen_after_read = state1.agent.seen.clone();

    let outcome2 = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome2, Outcome::Parked(statie_core::Park::AskUser(_))),
        "the ask_user action should still park the session; got {outcome2:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejection = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        rejection.is_none(),
        "the seen-patch step must not be rejected: {sys_entries:?}"
    );

    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.agent.seen, seen_after_read,
        "`seen` must be left unchanged by the dropped patch"
    );

    let note_entry = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"));
    let note_text = note_entry
        .and_then(|e| e.payload.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        note_text,
        "`seen` is kept by the harness; that part of your patch was dropped and the rest was kept.",
        "expected the strip-and-note text, got: {note_text}"
    );
}

// =======================================================================
// 7. A 13th file evicts the oldest (smallest-step) entry.
// =======================================================================

#[tokio::test]
async fn a_thirteenth_file_evicts_the_oldest_entry() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..13 {
        std::fs::write(root.path().join(format!("f{i}.rs")), numbered_lines(5)).unwrap();
    }

    let mut turns: Vec<MockTurn> = (0..13)
        .map(|i| read_turn(&format!("f{i}.rs"), 1, 5))
        .collect();
    turns.push(ask_user_turn());

    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    // `PlanSkill` (unlike `ImplementSkill`) has no `silent_read_cap`, so 13
    // consecutive reads with nothing else in the patch don't trip the
    // read-without-progress guard.
    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..13 {
        let outcome = run_step(&mut active, &log).await;
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(state.agent.seen.len(), 12, "cap must not be exceeded");
    assert!(
        !state.agent.seen.iter().any(|f| f.path == "f0.rs"),
        "the oldest (step 1) entry f0.rs should have been evicted: {:?}",
        state.agent.seen
    );
    assert!(
        state.agent.seen.iter().any(|f| f.path == "f12.rs"),
        "the newest entry f12.rs should be present: {:?}",
        state.agent.seen
    );
}

// =======================================================================
// 8. The prompt for the next step contains "seen" and the path.
// =======================================================================

#[tokio::test]
async fn prompt_for_the_next_step_carries_seen_and_the_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    let turns = vec![read_turn("a.rs", 1, 10), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    run_step(&mut active, &log).await;
    run_step(&mut active, &log).await;

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second = &prompts[1].user;
    assert!(
        second.contains("\"seen\""),
        "expected `seen` in the second prompt:\n{second}"
    );
    assert!(
        second.contains("a.rs"),
        "expected the path in the second prompt:\n{second}"
    );
}
