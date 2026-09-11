//! Integration tests for `strip_harness_owned` (`skills/src/coding/mod.rs`):
//! a model patch that touches a harness-owned field on `CodingWorkspace`
//! (`notes`, `reports`, `seen` -- see `common_harness_owned_fields`) no
//! longer has the WHOLE step rejected by `validate_default`'s owned-field
//! check. Since a bench battery run saw the model echo `seen` back twice
//! (it appears in its own workspace view) and lose a retry each time, the
//! project's rule since run 26 is commit-and-note: `plan`/`implement`/
//! `review`'s `validate` overrides now strip every harness-owned key
//! present in the raw patch before `validate_default` runs, and record one
//! note per removed key -- the rest of the patch (a real plan-step change,
//! a real tool call) still lands.
//!
//! Uses a mock model plus the *real* file tools in a tempdir (`common::
//! make_file_tools_session`), the same fixture style as `skills/tests/
//! seen.rs`/`commitment_rules.rs`.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{Event, LogKind, NullSink, Outcome, SessionLogExt};
use statie_models::{MockModel, MockTurn};
use statie_skills::{ImplementSkill, PlanSkill, ReviewSkill};

fn read_turn(path: &str, offset: u64, limit: u64) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": offset, "limit": limit},
            },
        })
        .to_string(),
    )
}

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

fn any_rejected(log: &Arc<dyn statie_core::SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

// =======================================================================
// 1. `implement`: a patch sends `seen` plus a real plan-step change and a
//    real tool call. The step commits, the plan change lands, `seen` is
//    unchanged, no `rejected` entry, and the note text appears in the next
//    prompt.
// =======================================================================

#[tokio::test]
async fn implement_strips_seen_keeps_plan_change_and_tool_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    // Step 1: a real read, so `seen` is non-empty afterward.
    // Step 2: the model patches `seen` directly (harness-owned) alongside a
    // real plan-step change and a real tool call.
    let turns = vec![
        read_turn("a.rs", 1, 10),
        MockTurn::new(
            json!({
                "patch": {
                    "seen": [{"path": "b.rs", "lines": 1, "read": [], "edited": [], "step": 99}],
                    "plan": [{"id": 1, "text": "do the thing", "status": "done"}],
                },
                "action": {"type": "tool", "name": "list_files", "args": {}},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string(),
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
    assert!(
        !seen_after_read.is_empty(),
        "seen should be non-empty after the read"
    );

    let outcome2 = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the step should commit (tool call ran), not be rejected; got {outcome2:?}"
    );
    assert!(!any_rejected(&log), "no `rejected` entry should be logged");

    let (state2, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state2.agent.seen, seen_after_read,
        "`seen` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        state2.agent.plan.len(),
        1,
        "the real plan-step change must land"
    );
    assert_eq!(state2.agent.plan[0].text, "do the thing");
    assert_eq!(state2.agent.plan[0].status, statie_skills::StepStatus::Done);

    // Step 3: the note from step 2 must appear in the prompt built for it.
    run_step(&mut active, &log).await;
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    let third = &prompts[2].user;
    assert!(
        third.contains(
            "`seen` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
        ),
        "expected the strip-and-note text in the third prompt:\n{third}"
    );
}

// =======================================================================
// 2. `implement`: same shape for `notes`.
// =======================================================================

#[tokio::test]
async fn implement_strips_notes_keeps_plan_change_and_tool_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "notes": "the model wrote this",
                    "plan": [{"id": 1, "text": "do the thing", "status": "done"}],
                },
                "action": {"type": "tool", "name": "list_files", "args": {}},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome1, Outcome::Continue),
        "the step should commit (tool call ran), not be rejected; got {outcome1:?}"
    );
    assert!(!any_rejected(&log), "no `rejected` entry should be logged");

    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.agent.notes, "",
        "`notes` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        state1.agent.plan.len(),
        1,
        "the real plan-step change must land"
    );
    assert_eq!(state1.agent.plan[0].text, "do the thing");

    run_step(&mut active, &log).await;
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second = &prompts[1].user;
    assert!(
        second.contains(
            "`notes` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
        ),
        "expected the strip-and-note text in the second prompt:\n{second}"
    );
}

// =======================================================================
// 3. `plan`: a patch sends `notes` plus a real plan-step addition and a
//    real tool call.
// =======================================================================

#[tokio::test]
async fn plan_strips_notes_keeps_plan_change_and_tool_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "notes": "the model wrote this",
                    "plan": [{"id": 1, "text": "explore the repo", "status": "pending"}],
                },
                "action": {"type": "tool", "name": "list_files", "args": {}},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome1, Outcome::Continue),
        "the step should commit (tool call ran), not be rejected; got {outcome1:?}"
    );
    assert!(!any_rejected(&log), "no `rejected` entry should be logged");

    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.agent.notes, "",
        "`notes` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        state1.agent.plan.len(),
        1,
        "the real plan-step addition must land"
    );
    assert_eq!(state1.agent.plan[0].text, "explore the repo");

    run_step(&mut active, &log).await;
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second = &prompts[1].user;
    assert!(
        second.contains(
            "`notes` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
        ),
        "expected the strip-and-note text in the second prompt:\n{second}"
    );
}

// =======================================================================
// 4. `review`: a patch sends `notes` plus real findings and a real tool
//    call (a send-back, so the action stays a plain tool call this step).
// =======================================================================

#[tokio::test]
async fn review_strips_notes_keeps_findings_and_tool_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), numbered_lines(10)).unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "notes": "the model wrote this",
                    "files": [{"path": "a.txt", "note": "under review"}], "findings": ["a.txt:1 the new function has no test"],
                },
                "action": {"type": "tool", "name": "list_files", "args": {}},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {"done": false, "next_skill": "implement"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let outcome1 = run_step(&mut active, &log).await;
    assert!(
        matches!(outcome1, Outcome::Continue),
        "the step should commit (tool call ran), not be rejected; got {outcome1:?}"
    );
    assert!(!any_rejected(&log), "no `rejected` entry should be logged");

    let (state1, _v) = active.session().state().await.unwrap();
    assert_eq!(
        state1.agent.notes, "",
        "`notes` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        state1.agent.findings,
        vec!["a.txt:1 the new function has no test".to_string()],
        "the real findings must land"
    );

    run_step(&mut active, &log).await;
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second = &prompts[1].user;
    assert!(
        second.contains(
            "`notes` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
        ),
        "expected the strip-and-note text in the second prompt:\n{second}"
    );
}

// =======================================================================
// 5. Operator patch (`Active::apply_user_patch`, the `--patch` path): a
//    harness-owned field is a TRUSTED write here, by existing design (see
//    `core/src/session.rs`'s `apply_trusted_patch` doc and
//    `core/tests/parse_validate.rs`'s
//    `operator_patch_touching_harness_owned_field_is_accepted`, plus this
//    crate's own `skills/tests/commitment_rules.rs`'s
//    `operator_patch_setting_notes_is_accepted_and_next_prompt_carries_the_notes_header`).
//    `apply_user_patch` never routes through `Skill::validate` at all --
//    only the model-authored path does -- so `strip_harness_owned` (wired
//    into `plan`/`implement`/`review`'s `validate` overrides) cannot and
//    does not affect it. An operator/`--patch` write to `notes` is
//    therefore still accepted, not rejected: the operator is a trusted
//    writer of these fields, same as the maintainer.
// =======================================================================

#[tokio::test]
async fn operator_patch_setting_notes_is_still_accepted_not_rejected() {
    let root = tempfile::tempdir().unwrap();
    let turns = vec![MockTurn::new(
        json!({"patch": {}, "action": {"type": "ask_user", "message": "confirm?"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    active
        .apply_user_patch(json!({"notes": "kept from last reasoning"}))
        .await
        .expect("an operator patch touching a harness-owned field is a trusted write");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.notes, "kept from last reasoning");

    run_step(&mut active, &log).await;
    assert!(
        note_texts(&log).is_empty(),
        "no strip-and-note text should be recorded for an operator patch"
    );
}
