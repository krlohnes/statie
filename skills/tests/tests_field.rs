//! Integration tests for `CodingWorkspace::tests` (BACKLOG.md item 5): the
//! harness-maintained record of the most recent `test` tool call, written
//! by `Skill::after_tool` (`common_after_tool` in `skills/src/coding/
//! mod.rs`) and replaced whole on every `test` call. Uses a mock model plus
//! the *real* `test` tool in a tempdir (`common::make_file_tools_session`),
//! the same fixture style as `skills/tests/seen.rs`/`skills/tests/
//! decision.rs`.

mod common;

use std::sync::Arc;

use serde_json::json;

use statie_core::{Event, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{ImplementSkill, TestRecord};

fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

/// A `test {}` call with NO `cmd`/`command` argument -- the auto-detect
/// path (`tools/src/test_tool.rs`'s `detect_test_command`), used by unit
/// 23's `detected_command_populates_the_tests_record_cmd` below.
fn test_turn_no_cmd() -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {}},
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

// =======================================================================
// A failing `test` call writes a `tests` record with the command, `pass:
// false`, failing test names, and the first error line.
// =======================================================================

#[tokio::test]
async fn failing_test_call_writes_the_tests_record() {
    let root = tempfile::tempdir().unwrap();

    let cmd = "sh -c 'echo \"test tests::adds ... FAILED\"; \
               echo \"panicked at src/lib.rs:5:5:\"; \
               exit 1'";
    let turns = vec![test_turn(cmd), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    let tests = state
        .agent
        .tests
        .clone()
        .expect("a test call must write a tests record");
    assert_eq!(tests.cmd, cmd);
    assert!(!tests.pass);
    assert_eq!(tests.failing, vec!["tests::adds".to_string()]);
    assert_eq!(
        tests.first_error.as_deref(),
        Some("panicked at src/lib.rs:5:5:")
    );
}

// =======================================================================
// A passing `test` call writes a `tests` record with `pass: true`, empty
// `failing`, and no `first_error`.
// =======================================================================

#[tokio::test]
async fn passing_test_call_writes_a_green_tests_record() {
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
    let tests = state
        .agent
        .tests
        .clone()
        .expect("a passing test call must still write a tests record");
    assert_eq!(
        tests,
        TestRecord {
            cmd: "true".to_string(),
            pass: true,
            failing: vec![],
            first_error: None,
            step: tests.step,
        }
    );
}

// =======================================================================
// A second `test` call replaces the record whole (no merging): a passing
// run after a failing one clears the earlier failure details.
// =======================================================================

#[tokio::test]
async fn a_second_test_call_replaces_the_record_whole() {
    let root = tempfile::tempdir().unwrap();

    let fail_cmd = "sh -c 'echo \"test tests::adds ... FAILED\"; exit 1'";
    let turns = vec![test_turn(fail_cmd), test_turn("true"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    run_step(&mut active, &log).await;
    let (state1, _v) = active.session().state().await.unwrap();
    let step1 = state1.agent.tests.as_ref().unwrap().step;
    assert!(!state1.agent.tests.as_ref().unwrap().pass);

    let outcome2 = run_step(&mut active, &log).await;
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let (state2, _v) = active.session().state().await.unwrap();
    let tests2 = state2
        .agent
        .tests
        .clone()
        .expect("the second test call must write a tests record");
    assert_eq!(
        tests2,
        TestRecord {
            cmd: "true".to_string(),
            pass: true,
            failing: vec![],
            first_error: None,
            step: tests2.step,
        },
        "the second (passing) test call must replace the record whole, not merge with the first"
    );
    assert!(
        tests2.step > step1,
        "the second call's recorded step must be later than the first's"
    );
}

// =======================================================================
// `tests` is harness-owned: a model patch touching it is stripped (the
// step still commits, with a note naming `tests`), the same treatment
// `decision.rs`'s `model_patch_touching_decision_is_stripped_and_noted`
// verifies for `decision`.
// =======================================================================

#[tokio::test]
async fn model_patch_touching_tests_is_stripped_and_noted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "line 1\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "tests": {"cmd": "rm -rf /", "pass": true, "failing": [], "first_error": null, "step": 5},
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

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.tests, None,
        "`tests` must be left unchanged (still unset) by the dropped patch"
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
            "`tests` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
        ),
        "expected the strip-and-note text naming `tests` in the second prompt:\n{second}"
    );
}

// =======================================================================
// `guards.tests_field` off: `ImplementSkill::new().state_fields(seen,
// decision, tests)` with `tests = false` leaves `tests` unset after a
// `test` call.
// =======================================================================

#[tokio::test]
async fn tests_field_off_leaves_tests_unset_after_a_test_call() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![test_turn("true"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> =
        Arc::new(ImplementSkill::new().state_fields(true, true, false));
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.tests, None,
        "tests_field = false must leave tests unset after a test call"
    );
}

// =======================================================================
// Unit 23 part (a): a `test {}` call with no `cmd`/`command` argument
// (auto-detected by `tools/src/test_tool.rs`'s `detect_test_command`, here a
// `Makefile` with a `test:` target) still writes a `tests` record whose
// `cmd` is the detected command -- `common_after_tool` (`skills/src/coding/
// mod.rs`) falls back to `statie_core::detected_test_command` (parsed from
// the tool output's `command: <cmd>` first line) instead of leaving `cmd`
// empty.
// =======================================================================

#[tokio::test]
async fn detected_command_populates_the_tests_record_cmd() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Makefile"), "test:\n\texit 1\n").unwrap();

    let turns = vec![test_turn_no_cmd(), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome = run_step(&mut active, &log).await;
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, _v) = active.session().state().await.unwrap();
    let tests = state
        .agent
        .tests
        .clone()
        .expect("an auto-detected test call must still write a tests record");
    assert_eq!(
        tests.cmd, "make test",
        "the tests record's cmd must be the auto-detected command, not empty"
    );
    assert!(!tests.pass);
}
