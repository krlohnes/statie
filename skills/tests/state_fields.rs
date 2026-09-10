//! Integration tests for `guards.seen_map`/`guards.decision_field`
//! (`core/src/guards.rs`; BACKLOG.md item 2): switches to run the coding
//! skills with model-owned state only, so harness-owned state (`Coding
//! Workspace::seen`/`decision`) can be measured against it with the model,
//! budgets, and window fixed.
//!
//! Driven through `ImplementSkill::new().state_fields(seen, decision)`
//! (mirrors `done_stays_done.rs`'s use of `with_done_stays_done`), with a
//! real `Session<CodingWorkspace>` wired to the real file tools via
//! `common::make_file_tools_session` -- the same fixture style as
//! `skills/tests/seen.rs` and `skills/tests/decision.rs`, which this file
//! complements: those two exercise the switches' `true` (today's) behaviour
//! end-to-end; this file exercises `false`.

mod common;

use std::sync::Arc;

use serde_json::json;

use statie_core::{Event, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{Decision, ImplementSkill};

fn read_turn(path: &str, offset: u64, limit: u64) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "read_file", "args": {"path": path, "offset": offset, "limit": limit}},
        })
        .to_string(),
    )
}

fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
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

/// Numbers each line `line {k}` (1-based) so a `read_file` header is
/// well-formed.
fn numbered_lines(n: u64) -> String {
    (1..=n)
        .map(|k| format!("line {k}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// =======================================================================
// `seen_map` off: after a `read_file`, the committed `seen` is still empty.
// =======================================================================

#[tokio::test]
async fn seen_map_off_leaves_seen_empty_after_a_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let turns = vec![read_turn("a.rs", 1, 100), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new().state_fields(false, true));
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert!(
        state.agent.seen.is_empty(),
        "seen_map = false must leave seen empty after a read_file: {:?}",
        state.agent.seen
    );
}

/// Same scenario with the switch left on (default): `seen` is populated,
/// confirming the off case above is actually exercising the switch and not
/// some unrelated reason `seen` would stay empty.
#[tokio::test]
async fn seen_map_on_records_seen_after_a_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(250)).unwrap();

    let turns = vec![read_turn("a.rs", 1, 100), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new().state_fields(true, true));
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert!(
        !state.agent.seen.is_empty(),
        "seen_map = true should record seen after a read_file"
    );
}

// =======================================================================
// `decision_field` off: a passing `test` never clears an existing
// decision (nothing is written or cleared by the harness at all).
// =======================================================================

#[tokio::test]
async fn decision_field_off_leaves_an_existing_decision_uncleared_by_a_passing_test() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![test_turn("true"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new().state_fields(true, false));
    let mut active = session.activate(skill).await.unwrap();

    // Operator patch is a trusted write to the harness-owned `decision`
    // field, the same seed `skills/tests/decision.rs`'s
    // `passing_test_clears_decision` uses.
    active
        .apply_user_patch(json!({
            "decision": {
                "question": "why?",
                "answer": "because.",
                "step": 0,
            }
        }))
        .await
        .expect("operator patch touching decision is a trusted write");

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.decision,
        Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        "decision_field = false must leave an existing decision uncleared, even by a passing test"
    );
}
