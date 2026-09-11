//! Session-level tests for the working set (statie-spec.md "10. Working
//! set (recent observations)"): a scripted run of `read_file` calls proves
//! an earlier read's window survives into a later prompt, and that
//! `working_set.max_windows: 0` disables the section entirely.

use super::common;

use std::sync::Arc;

use serde_json::json;

use statie_core::{Event, NullSink, Outcome, Park, SessionLogExt, WorkingSetConfig};
use statie_models::{MockModel, MockTurn};
use statie_skills::PlanSkill;

fn read_call(path: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": 1, "limit": 5},
            },
        })
        .to_string(),
    )
}

/// A no-op filler step: `PlanSkill` disallows `yield`, so `ask_user` stands
/// in for "do nothing more, just let the prompt be built again."
fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

#[tokio::test]
async fn earlier_read_window_survives_into_a_later_prompt() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();

    let turns = vec![
        // Step 1: read a.rs.
        read_call("a.rs"),
        // Step 2: read b.rs -- a.rs's output has now been dropped from the
        // Observation section (only the latest observation is carried
        // there), but with the working set on it should still be visible
        // as a window in step 3's prompt.
        read_call("b.rs"),
        // Step 3: anything, so we can inspect the prompt built for it.
        ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    // Working set on (the default), with room for both windows.
    session = session.with_working_set(WorkingSetConfig {
        max_windows: 4,
        recent_calls: 6,
    });

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    // Steps 1-2 (the reads) commit and continue; step 3 (`ask_user`) parks
    // -- but its prompt is still built and recorded before it parks.
    for i in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        if i < 2 {
            assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
        } else {
            assert!(
                matches!(outcome, Outcome::Parked(Park::AskUser(_))),
                "got {outcome:?}"
            );
        }
    }

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);

    // The third prompt (built for step 3, after steps 1 and 2 committed)
    // must carry the working-set header and a window for a.rs's read --
    // its output would otherwise have fallen out of the Observation
    // section entirely once b.rs was read.
    let third = &prompts[2].user;
    assert!(
        third.contains("working set (what you read recently; derived from the log, not state):"),
        "expected the working-set header in the third prompt:\n{third}"
    );
    assert!(
        third.contains("--- read_file a.rs"),
        "expected a window for a.rs's earlier read in the third prompt:\n{third}"
    );
    assert!(
        third.contains("fn a() {}"),
        "expected a.rs's content inside its window:\n{third}"
    );
}

#[tokio::test]
async fn zero_max_windows_omits_the_working_set_section() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();

    let turns = vec![read_call("a.rs"), read_call("b.rs"), ask_user_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    session = session.with_working_set(WorkingSetConfig {
        max_windows: 0,
        recent_calls: 0,
    });

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    // Steps 1-2 (the reads) commit and continue; step 3 (`ask_user`) parks
    // -- but its prompt is still built and recorded before it parks.
    for i in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        if i < 2 {
            assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
        } else {
            assert!(
                matches!(outcome, Outcome::Parked(Park::AskUser(_))),
                "got {outcome:?}"
            );
        }
    }

    let prompts = model.prompts();
    let third = &prompts[2].user;
    assert!(
        !third.contains("working set (what you read recently; derived from the log, not state):"),
        "expected no working-set header with max_windows: 0:\n{third}"
    );
    assert!(
        !third.contains("--- read_file"),
        "expected no window rendering with max_windows: 0:\n{third}"
    );
}
