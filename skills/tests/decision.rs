//! Integration tests for `CodingWorkspace::decision`: the harness-maintained
//! record of the reasoning subagent's most recent conclusion from a `think`
//! call, cleared when a `test` call passes (`Skill::after_tool`, `skills/
//! src/coding/mod.rs`'s `common_after_tool`).
//!
//! `think` itself is special-cased by `core::Session::execute` (see
//! `core/src/session.rs`'s `execute_think`): the session intercepts
//! `Action::Tool { name: "think", .. }` before dispatch to the tool
//! registry and never calls `Skill::after_tool` for it (only a normal
//! registry tool call reaches that hook). That means the `think` half of
//! `common_after_tool`'s behavior (setting/leaving `decision` from a
//! `think` output) cannot be driven end-to-end through a real `Session` --
//! it is covered instead by unit tests in `skills/src/coding/tests.rs`
//! (`common_after_tool` is private to that crate), the same way the
//! `seen`-from-`read_file`/`edit_file`/etc. shapes have both a session-level
//! integration test here-style and a direct-call unit test there.
//!
//! `test` is a normal registry tool (not intercepted), so the
//! passes-clears/fails-leaves-alone behavior below drives it through a real
//! `Session`, the same fixture style as `skills/tests/seen.rs`/
//! `harness_owned.rs` (`common::make_file_tools_session`). `decision` is
//! also harness-owned the same way `seen`/`notes` are: a model patch
//! touching it is stripped before `validate_default` runs, with a note
//! naming the field (`strip_harness_owned`), not a rejection -- verified
//! here the same way `harness_owned.rs` verifies it for `seen`/`notes` (via
//! the next prompt's text, since a note attached to a committed tool-call
//! step is folded into that step's own observation, not logged as a
//! separate `System` "note" entry -- see `core/src/session.rs`'s "10b.
//! Validation notes").

mod common;

use std::sync::Arc;

use serde_json::json;

use statie_core::{Event, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{Decision, ImplementSkill};

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

fn any_rejected(log: &Arc<dyn statie_core::SessionLog>) -> bool {
    log.entries(Some(statie_core::LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(serde_json::Value::as_str) == Some("rejected"))
}

// =======================================================================
// (c) A passing `test` output clears `decision`; a failing one does not;
//     no existing `decision` stays `None`.
// =======================================================================

#[tokio::test]
async fn passing_test_clears_decision() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn main() {}\n").unwrap();

    let turns = vec![test_turn("true"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

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
        state.agent.decision, None,
        "a passing test should clear an existing decision"
    );
}

#[tokio::test]
async fn failing_test_leaves_decision_alone() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![test_turn("false"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    active
        .apply_user_patch(json!({
            "decision": {
                "question": "why?",
                "answer": "because.",
                "step": 0,
            }
        }))
        .await
        .unwrap();

    run_step(&mut active, &log).await;

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.decision,
        Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        "a failing test must leave an existing decision alone"
    );
}

#[tokio::test]
async fn passing_test_with_no_existing_decision_stays_none() {
    let root = tempfile::tempdir().unwrap();
    let turns = vec![test_turn("true"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(state.agent.decision, None);
}

// =======================================================================
// (d) A model patch `{"decision": {...}}` is stripped, the step is not
//     rejected, and the note names `decision`.
// =======================================================================

#[tokio::test]
async fn model_patch_touching_decision_is_stripped_and_noted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "line 1\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "decision": {"question": "q", "answer": "a", "step": 5},
                    "plan": [{"id": 1, "text": "do the thing", "status": "done"}],
                },
                "action": {"type": "tool", "name": "list_files", "args": {}},
            })
            .to_string(),
        ),
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome, Outcome::Continue),
        "the step should commit (tool call ran), not be rejected; got {outcome:?}"
    );
    assert!(!any_rejected(&log), "no `rejected` entry should be logged");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.decision, None,
        "`decision` must be left unchanged (still unset) by the dropped patch"
    );
    assert_eq!(
        state.agent.plan.len(),
        1,
        "the real plan-step change must land"
    );
    assert_eq!(state.agent.plan[0].text, "do the thing");

    run_step(&mut active, &log).await;
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second = &prompts[1].user;
    assert!(
        second.contains(
            "`decision` is kept by the harness; that part of your patch was dropped and the \
             rest was kept."
        ),
        "expected the strip-and-note text naming `decision` in the second prompt:\n{second}"
    );
}

// =======================================================================
// (e) `tools_exact_per_skill` (unit test in `skills/src/coding/tests.rs`)
//     and the router-has-no-think check here.
// =======================================================================

#[test]
fn router_does_not_have_think() {
    use statie_core::Skill;
    use statie_skills::RouterSkill;
    assert!(!RouterSkill::new().tools().contains(&"think".to_string()));
}
