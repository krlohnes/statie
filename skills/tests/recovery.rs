//! Milestone 2's validation-and-recovery test suite (statie-spec.md §8;
//! §10 "Invalid outputs recover without state corruption"): parse failures,
//! schema/rule/unacked-inbox rejections, retry-cap parking, unknown tools,
//! tool errors, and cancellation during tool execution all recover without
//! corrupting committed state.
//!
//! Reuses the `EchoTool` / `ToolSet` / `SessionConfig` / `Session` helpers
//! from `toy_loop.rs`, now shared via `tests/common/mod.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use statie_core::{
    Event, LogKind, NullSink, Outcome, Park, Schema, Session, SessionLog, SessionLogExt, Skill,
    Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet, Version,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{ToySkill, ToyWorkspace};

use common::{config, config_with_retries, make_session, tool_set};

/// A tool that always fails with `ExecutionFailed`, to test that a tool
/// error is an *observation*, not a validation failure (spec §8: only
/// parse/validate rejections count toward the retry cap).
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "failing"
    }

    fn description(&self) -> &str {
        "Always fails."
    }

    fn args_schema(&self) -> Schema {
        #[derive(schemars::JsonSchema)]
        struct Args {}
        Schema::of::<Args>()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        Err(ToolError::ExecutionFailed("boom".to_string()))
    }
}

/// A tool that awaits a 10s sleep, deliberately *not* watching `ctx.cancel`
/// itself, so the outer `tokio::select!` in `session::Active::execute` (the
/// one under test, per ARCHITECTURE.md B3 / spec §8) is what decides the
/// race deterministically: the harness's own `token.cancelled()` branch
/// resolves at ~100ms while this future is still asleep for 10s.
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }

    fn description(&self) -> &str {
        "Sleeps for 10s."
    }

    fn args_schema(&self) -> Schema {
        #[derive(schemars::JsonSchema)]
        struct Args {}
        Schema::of::<Args>()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(ToolOutputFull::new("done".to_string()))
    }
}

fn tool_set_with(extra: Vec<Arc<dyn Tool>>) -> ToolSet {
    let mut ts = tool_set();
    for t in extra {
        ts.register(t);
    }
    ts
}

/// Skill wrapper that exposes an extended tool set (echo + whatever the
/// test needs) while keeping `ToySkill`'s workspace, rules, and spec.
fn toy(target: u64) -> Arc<ToySkill> {
    Arc::new(ToySkill::new(target))
}

// ---------------------------------------------------------------------
// 1. Malformed JSON then recovery.
// ---------------------------------------------------------------------

#[tokio::test]
async fn malformed_json_then_recovery_resets_failure_counter_on_commit() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new("{ not json".to_string()),
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "recent_summary": "recovered"},
                "action": {"type": "yield"},
            })
            .to_string(),
        ),
        MockTurn::new("bad a".to_string()),
        MockTurn::new("bad b".to_string()),
        MockTurn::new("bad c".to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Turn 1: malformed JSON.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(version1, Version(0));
    assert_eq!(state1.agent.counter, 0);

    // Turn 2: valid, should recover and commit.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].user.contains("output rejected"),
        "second prompt should show the rejection observation:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[1].user.contains("invalid JSON"),
        "second prompt should explain the JSON was invalid:\n{}",
        prompts[1].user
    );

    let (state2, version2) = active.session().state().await.unwrap();
    assert_eq!(version2, Version(1));
    assert_eq!(state2.agent.counter, 1);

    // failures reset on commit: continuing on the *same* activation (which
    // already saw one rejection before its successful commit above), three
    // more malformed turns must still take a full `retries + 1` rejections
    // before parking with Failure -- proving the failure counter was reset
    // to 0 by the successful commit at turn 2, not left at 1.
    let rec_a = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_a = log.append_record(rec_a).unwrap();
    assert!(matches!(outcome_a, Outcome::Continue), "got {outcome_a:?}");
    let rec_b = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_b = log.append_record(rec_b).unwrap();
    assert!(matches!(outcome_b, Outcome::Continue), "got {outcome_b:?}");
    let rec_c = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_c = log.append_record(rec_c).unwrap();
    assert!(
        matches!(outcome_c, Outcome::Continue),
        "got {outcome_c:?}; with the default retries=3 cap, a 3rd straight \
         rejection since the last commit must still be Continue, not Failure \
         -- if the earlier single rejection (before the successful commit) \
         had leaked into this count, a 3rd bad turn here would already be \
         the 4th rejection and would incorrectly park"
    );
}

// ---------------------------------------------------------------------
// 2. Schema-shaped but wrong types.
// ---------------------------------------------------------------------

#[tokio::test]
async fn schema_shaped_wrong_type_is_rejected_mentioning_field_then_recovers() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({"patch": {"counter": "seven"}, "action": {"type": "yield"}}).to_string(),
        ),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (_, version1) = active.session().state().await.unwrap();
    assert_eq!(version1, Version(0));

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1, "second call not made yet");

    // Check the *rejection reason itself* (the System log entry's
    // `reason` field), not the whole next prompt -- the prompt's AgentState
    // section always renders the literal field name `"counter"` regardless
    // of the rejection reason's content, which would make this assertion a
    // false positive if checked against the whole prompt text.
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("counter"),
        "rejection reason should mention `counter`: {reason}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let (state2, version2) = active.session().state().await.unwrap();
    assert_eq!(version2, Version(1));
    assert_eq!(state2.agent.counter, 1);
}

// ---------------------------------------------------------------------
// 3. Unknown action tag.
// ---------------------------------------------------------------------

/// An `action.type` that names neither a real action tag nor a registered
/// tool (`"fly"`) is normalized to a tool call named `fly` (a local model
/// routinely sends the tool name as the type) rather than rejected as
/// invalid JSON. Since `fly` is not a registered tool, the patch still
/// commits and the unknown tool becomes an observation for the next step
/// -- mirroring `unknown_tool_commits_patch_then_reports_unknown_tool_observation`
/// -- and a `normalized` system entry records the fix.
#[tokio::test]
async fn unknown_action_tag_is_normalized_then_reported_as_unknown_tool() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({"patch": {}, "action": {"type": "fly"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    // The patch committed: normalization turned this into a (recognized
    // shape) tool call, not a parse rejection.
    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("normalized")),
        "expected a normalized system entry, got {sys_entries:?}"
    );
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error")),
        "expected a tool_error system entry for the unknown tool `fly`, got {sys_entries:?}"
    );
}

// ---------------------------------------------------------------------
// 4. Rule violation.
// ---------------------------------------------------------------------

#[tokio::test]
async fn rule_violation_is_rejected_mentioning_rule_and_state_unchanged() {
    let root = tempfile::tempdir().unwrap();

    let long_summary = "x".repeat(600);
    let turns = vec![MockTurn::new(
        json!({
            "patch": {"recent_summary": long_summary},
            "action": {"type": "yield"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let initial_agent = skill.initial_workspace();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));
    assert_eq!(
        state.agent, initial_agent,
        "agent state must be unchanged on rejection"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("rule"),
        "rejection reason should mention `rule`: {reason}"
    );
}

// ---------------------------------------------------------------------
// 5. Merge-patch deleting a required-looking-but-#[serde(default)] field.
// ---------------------------------------------------------------------

#[tokio::test]
async fn merge_patch_null_on_serde_default_field_succeeds_and_resets_to_default() {
    // `ToyWorkspace::counter` is `#[serde(default)]`, so JSON Merge Patch's
    // `null` (which deletes the key from the merged doc) does NOT error the
    // way it would for a field with no default -- the missing key is simply
    // filled back in with `u64::default() == 0`. This documents that
    // behavior rather than treating it as a rejection case.
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({"patch": {"counter": 5}, "action": {"type": "yield"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // First bump the counter away from its default so the reset is visible.
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Yield)),
        "got {outcome:?}"
    );
    let (state, _) = active.session().state().await.unwrap();
    assert_eq!(state.agent.counter, 5);
    drop(active);

    // Now, a fresh session where the very next turn nulls `counter` out.
    let turns2 = vec![MockTurn::new(
        json!({"patch": {"counter": null}, "action": {"type": "yield"}}).to_string(),
    )];
    let model2 = Arc::new(MockModel::new(turns2));
    let (mut session2, log2) = make_session(Arc::clone(&model2), config(&root));
    let skill2 = toy(100);
    let mut active2 = session2.activate(Arc::clone(&skill2)).await.unwrap();

    let rec2 = active2.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log2.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "null-patching a #[serde(default)] field must SUCCEED (not reject): got {outcome2:?}"
    );

    let (state2, version2) = active2.session().state().await.unwrap();
    assert_eq!(version2, Version(1));
    assert_eq!(
        state2.agent.counter, 0,
        "counter should reset to its serde default (0) after the field was deleted"
    );
}

// ---------------------------------------------------------------------
// 6. Retry cap exactly.
// ---------------------------------------------------------------------

#[tokio::test]
async fn retry_cap_exactly_parks_then_recovers_after_failures_reset() {
    let root = tempfile::tempdir().unwrap();

    let model = Arc::new(MockModel::new(vec![
        MockTurn::new("bad 1".to_string()),
        MockTurn::new("bad 2".to_string()),
        MockTurn::new("bad 3".to_string()),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ]));
    let (mut session, log) = make_session(Arc::clone(&model), config_with_retries(&root, 2));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Parked(Park::Failure(_))),
        "got {outcome3:?}"
    );

    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(0));

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected_count = sys_entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .count();
    let failure_count = sys_entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("failure"))
        .count();
    assert_eq!(
        rejected_count, 3,
        "expected exactly 3 rejected entries, got {sys_entries:?}"
    );
    assert_eq!(
        failure_count, 1,
        "expected exactly 1 failure entry, got {sys_entries:?}"
    );

    // A fourth (valid) turn after the failure park commits normally --
    // failures were reset when the session parked.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(
        matches!(outcome4, Outcome::Parked(Park::Yield)),
        "got {outcome4:?}"
    );

    let (state4, version4) = active.session().state().await.unwrap();
    assert_eq!(version4, Version(1));
    assert_eq!(state4.agent.counter, 1);
}

// ---------------------------------------------------------------------
// 7. Unknown tool.
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_tool_commits_patch_then_reports_unknown_tool_observation() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1},
            "action": {"type": "tool", "name": "nope"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    // The patch committed even though the action's tool is unknown --
    // per session.rs, commit happens before `execute()` looks the tool up.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(state.agent.counter, 1);

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error")),
        "expected a tool_error system entry, got {sys_entries:?}"
    );

    // Feed a follow-up turn to inspect the next prompt.
    drop(active);
    let turns2 = vec![
        MockTurn::new(
            json!({"patch": {"counter": 1}, "action": {"type": "tool", "name": "nope"}})
                .to_string(),
        ),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model2 = Arc::new(MockModel::new(turns2));
    let (mut session2, log2) = make_session(Arc::clone(&model2), config(&root));
    let skill2 = toy(100);
    let mut active2 = session2.activate(Arc::clone(&skill2)).await.unwrap();

    let rec_a = active2.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_a = log2.append_record(rec_a).unwrap();
    assert!(matches!(outcome_a, Outcome::Continue), "got {outcome_a:?}");

    let rec_b = active2.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log2.append_record(rec_b).unwrap();

    let prompts = model2.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].user.contains("unknown tool"),
        "next prompt should report the unknown tool:\n{}",
        prompts[1].user
    );
}

// ---------------------------------------------------------------------
// 8. Tool error is an observation, not a failure.
// ---------------------------------------------------------------------

#[tokio::test]
async fn tool_execution_error_is_observation_not_a_validation_failure() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1},
                "action": {"type": "tool", "name": "failing"},
            })
            .to_string(),
        ),
        // Two bad turns follow; with retries=2 that's not enough to park
        // (3 would be needed) -- proving the tool error above did not bump
        // the failure counter.
        MockTurn::new("bad 1".to_string()),
        MockTurn::new("bad 2".to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config_with_retries(&root, 2);
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Box::new(common::ArcModel(Arc::clone(&model))),
        tool_set_with(vec![Arc::new(FailingTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(
        version1,
        Version(1),
        "patch should have committed despite the tool failing"
    );
    assert_eq!(state1.agent.counter, 1);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let prompts = model.prompts();
    assert!(
        prompts[1].user.contains("boom"),
        "prompt after the tool error should contain the tool's error message:\n{}",
        prompts[1].user
    );

    // A following bad turn: only the 2nd rejection so far (this step's "bad
    // 1"); one more ("bad 2") should still be within the retries=2 cap
    // (2 rejections <= retries), not yet parking -- if the tool error had
    // incremented `failures`, this would already be a Failure park.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "got {outcome3:?}; tool error must not have counted toward the retry cap"
    );
}

// ---------------------------------------------------------------------
// 9. Cancel during tool execution.
// ---------------------------------------------------------------------

#[tokio::test]
async fn cancel_during_tool_execution_parks_cancelled_with_patch_already_committed() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1},
            "action": {"type": "tool", "name": "slow"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Box::new(common::ArcModel(Arc::clone(&model))),
        tool_set_with(vec![Arc::new(SlowTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let handle = active.session().cancel_handle();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        active.step(Event::Tick, &mut NullSink),
    )
    .await
    .expect("step should return within 2s")
    .unwrap();
    let outcome = log.append_record(result).unwrap();
    assert!(
        matches!(outcome, Outcome::Parked(Park::Cancelled)),
        "got {outcome:?}"
    );
    cancel_task.await.unwrap();

    // Patch committed before execution (B3/§8): version bumped to 1 even
    // though the tool never finished.
    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(state.agent.counter, 1);
    drop(active);

    // Re-run the same cancel-mid-tool scenario with one extra scripted turn
    // so the *next* prompt (after the abort) can be inspected directly.
    let turns2 = vec![
        MockTurn::new(
            json!({"patch": {"counter": 1}, "action": {"type": "tool", "name": "slow"}})
                .to_string(),
        ),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model2 = Arc::new(MockModel::new(turns2));
    let cfg2 = config(&root);
    let log2: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session2 = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log2),
        Box::new(common::ArcModel(Arc::clone(&model2))),
        tool_set_with(vec![Arc::new(SlowTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg2,
    );
    let skill2 = toy(100);
    let mut active2 = session2.activate(Arc::clone(&skill2)).await.unwrap();

    let handle2 = active2.session().cancel_handle();
    let cancel_task2 = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle2.cancel();
    });
    let rec_a = tokio::time::timeout(
        Duration::from_secs(2),
        active2.step(Event::Tick, &mut NullSink),
    )
    .await
    .expect("step should return within 2s")
    .unwrap();
    let outcome_a = log2.append_record(rec_a).unwrap();
    assert!(
        matches!(outcome_a, Outcome::Parked(Park::Cancelled)),
        "got {outcome_a:?}"
    );
    cancel_task2.await.unwrap();

    let rec_b = active2.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log2.append_record(rec_b).unwrap();

    let prompts = model2.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].user.contains("aborted"),
        "next prompt should report the tool was aborted:\n{}",
        prompts[1].user
    );
}

// ---------------------------------------------------------------------
// 10. Stale version is impossible through the public API (B1, indirect).
// ---------------------------------------------------------------------

#[tokio::test]
async fn rejected_step_leaves_version_unchanged_and_next_commit_lands_at_one() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new("not json".to_string()),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let (_, version_before) = active.session().state().await.unwrap();
    assert_eq!(version_before, Version(0));

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (_, version_after_reject) = active.session().state().await.unwrap();
    assert_eq!(
        version_after_reject, version_before,
        "store version must be unchanged after a rejected step"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let (_, version_final) = active.session().state().await.unwrap();
    assert_eq!(
        version_final,
        Version(1),
        "the next successful commit must land at Version(1), not skip ahead to Version(2)"
    );
}

// ---------------------------------------------------------------------
// 11. Reasoning on rejected outputs is logged but never prompted.
// ---------------------------------------------------------------------

#[tokio::test]
async fn reasoning_on_rejected_output_is_logged_but_never_reaches_a_prompt() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new("not json".to_string()).with_reasoning("SECRET-REASONING".to_string()),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let reasoning_entries = log.entries(Some(LogKind::Reasoning), 10_000);
    assert!(
        reasoning_entries.iter().any(|e| {
            e.payload
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("SECRET-REASONING"))
        }),
        "expected a Reasoning log entry containing the secret text, got {reasoning_entries:?}"
    );

    let prompts = model.prompts();
    for p in &prompts {
        assert!(
            !p.system.contains("SECRET-REASONING"),
            "system prompt leaked reasoning: {}",
            p.system
        );
        assert!(
            !p.user.contains("SECRET-REASONING"),
            "user prompt leaked reasoning: {}",
            p.user
        );
    }
}

// ---------------------------------------------------------------------
// 12. AskUser parks and the next User event resumes.
// ---------------------------------------------------------------------

#[tokio::test]
async fn ask_user_parks_and_next_user_event_resumes_and_acks() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "which file?"},
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "yield"},
                "ack_inbox": [1],
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    match outcome1 {
        Outcome::Parked(Park::AskUser(msg)) => {
            assert_eq!(msg, "which file?");
        }
        other => panic!("expected Parked(AskUser(_)), got {other:?}"),
    }

    let rec2 = active
        .step(Event::User("main.rs".to_string()), &mut NullSink)
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let (state, _) = active.session().state().await.unwrap();
    let item = state.harness.inbox.iter().find(|i| i.id == 1).unwrap();
    assert!(item.acked, "inbox item 1 should be acked");
    assert_eq!(item.text, "main.rs");
}

// ---------------------------------------------------------------------
// 13. Ack of an unknown id is ignored; missing ack still rejected.
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_ack_id_ignored_but_missing_ack_still_rejected() {
    let root = tempfile::tempdir().unwrap();

    let model = Arc::new(MockModel::new(vec![
        // Acks [1, 99] -- 99 doesn't exist and is ignored, but item 2 is
        // still unacked, so this must be rejected.
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "yield"}, "ack_inbox": [1, 99]}).to_string(),
        ),
        MockTurn::new(
            json!({"patch": {}, "action": {"type": "yield"}, "ack_inbox": [1, 2]}).to_string(),
        ),
    ]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Seed two inbox items directly on the session (ids 1 and 2), then
    // Tick so the model call sees both unacked.
    active
        .session()
        .push_user("first".to_string())
        .await
        .unwrap();
    active
        .session()
        .push_user("second".to_string())
        .await
        .unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains('2'),
        "rejection reason should mention the missing unacked id 2: {reason}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let (state, _) = active.session().state().await.unwrap();
    assert!(state.harness.inbox.iter().all(|i| i.acked));
}

/// Null tool args are treated as `{}`; empty tool output becomes an explicit
/// placeholder; and the third identical call with identical output carries a
/// loop warning in the observation.
#[tokio::test]
async fn empty_output_placeholder_null_args_and_loop_guard() {
    use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet};
    struct Silent;
    #[async_trait::async_trait]
    impl Tool for Silent {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "returns nothing"
        }
        fn args_schema(&self) -> Schema {
            Schema::of::<serde_json::Value>()
        }
        async fn call(
            &self,
            args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutputFull, ToolError> {
            assert!(
                args.is_object(),
                "null args must arrive as an object, got {args}"
            );
            Ok(ToolOutputFull::new(String::new()))
        }
    }
    let root = tempfile::tempdir().unwrap();
    let call = |i: u64| {
        MockTurn::new(json!({"patch": {"counter": i}, "action": {"type": "tool", "name": "echo", "args": null}}).to_string())
    };
    let model = Arc::new(MockModel::new(vec![
        call(1),
        call(2),
        call(3),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ]));
    let mut tools = ToolSet::new();
    tools.register(Arc::new(Silent));
    let cfg = common::config(&root);
    let log: Arc<dyn statie_core::SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = statie_core::Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        log.clone(),
        Box::new(common::ArcModel(model.clone())),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(50));
    let mut active = session.activate(skill).await.unwrap();
    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }
    let prompts = model.prompts();
    assert!(
        prompts[1].user.contains("returned no output"),
        "{}",
        prompts[1].user
    );
    assert!(
        !prompts[2].user.contains("[harness]"),
        "second identical call is not yet flagged"
    );
    assert!(
        prompts[3].user.contains("occurrence #3") && prompts[3].user.contains("[harness]"),
        "{}",
        prompts[3].user
    );
}

/// The loop guard must catch alternating loops (A/B/A/B/A), not just
/// consecutive repeats: two different tool calls with stable outputs,
/// interleaved, must still trigger on the third occurrence of either.
#[tokio::test]
async fn alternating_tool_calls_trigger_loop_guard() {
    let root = tempfile::tempdir().unwrap();
    let call = |i: u64, text: &str| {
        MockTurn::new(
            json!({"patch": {"counter": i}, "action": {"type": "tool", "name": "echo", "args": {"text": text}}})
                .to_string(),
        )
    };
    // A/B/A/B/A: `echo` with text "a" and text "b" alternating.
    let model = Arc::new(MockModel::new(vec![
        call(1, "a"),
        call(2, "b"),
        call(3, "a"),
        call(4, "b"),
        call(5, "a"),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ]));
    let cfg = config(&root);
    let (mut session, log) = make_session(model.clone(), cfg);
    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(50));
    let mut active = session.activate(skill).await.unwrap();
    for _ in 0..6 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }
    let prompts = model.prompts();
    assert!(
        !prompts[3].user.contains("[harness]"),
        "third step's prompt (second occurrence of 'a') is not yet flagged: {}",
        prompts[3].user
    );
    assert!(
        prompts[5].user.contains("occurrence #3"),
        "{}",
        prompts[5].user
    );
}

// ---------------------------------------------------------------------
// Normalization: a malformed `action.type` naming the tool directly is
// recovered and executed, and the fix is logged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn malformed_action_type_as_tool_name_executes_and_is_logged() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1},
            "action": {"type": "echo", "args": {"text": "hi"}},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let tool_entries = log.entries(Some(LogKind::ToolOutput), 10_000);
    assert!(
        !tool_entries.is_empty(),
        "expected the `echo` tool to have run despite the malformed action.type"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("normalized")),
        "expected a `normalized` system entry, got {sys_entries:?}"
    );
}

// ---------------------------------------------------------------------
// 9. Oversized rejection observation is clamped, not fatal.
// ---------------------------------------------------------------------

/// A harness-authored rejection observation (here: "unknown tool `<name>`")
/// is never truncated at construction time. With a tiny observation budget
/// and a very long tool name, the rejection text alone can exceed the
/// section budget -- which used to fail the *next* step with `prompt
/// section Observation over budget` and park the session. The loop must
/// clamp the pending observation to the budget before handing it to the
/// builder, so the next step still succeeds.
#[tokio::test]
async fn oversized_rejection_observation_is_clamped_not_fatal() {
    use statie_core::{Budgets, Tokenizer};

    let root = tempfile::tempdir().unwrap();
    let long_name = "n".repeat(2_000);

    let turns = vec![
        // Step 1: call an unknown tool with a very long name -> a long
        // "unknown tool `...`" rejection becomes the pending observation.
        MockTurn::new(
            json!({
                "patch": {"counter": 1},
                "action": {"type": "tool", "name": long_name},
            })
            .to_string(),
        ),
        // Step 2: the model just needs to be asked; whether the step
        // succeeds is what's under test, not what this turn does.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));

    // 150 is small enough that the 2,000-char rejection text overflows it,
    // but large enough for the harness's instructional overflow marker
    // itself to fit under `HeuristicTokenizer`'s coarse char/4 estimate
    // (the marker text alone costs roughly 100 tokens under that estimate).
    let mut cfg = config(&root);
    cfg.budgets = Budgets {
        total: 8000,
        observation: 150,
        augmentation: 500,
    };
    let (mut session, log) = make_session(Arc::clone(&model), cfg);

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // The step that actually renders the oversized pending observation into
    // a prompt is this one -- it must succeed, not fail with SectionOverBudget.
    let rec2 = active.step(Event::Tick, &mut NullSink).await;
    let rec2 = rec2.expect("step must not fail: oversized observation must be clamped");
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    let second_prompt = &prompts[1];

    let obs_idx = second_prompt
        .user
        .find("## Observation")
        .expect("second prompt should carry an Observation section");
    let observation_section = &second_prompt.user[obs_idx..];
    let heading = "## Observation\n";
    let observation_body = observation_section
        .strip_prefix(heading)
        .expect("Observation section should start with its heading");

    // The builder's per-section check counts the rendered `Observation`
    // body alone (not the "## Observation\n" heading) against the budget --
    // this is exactly what `clamp` targets.
    let tok = statie_core::HeuristicTokenizer;
    assert!(
        tok.count(observation_body) <= 150,
        "Observation section body should be within the 150-token budget, got {} tokens:\n{observation_body}",
        tok.count(observation_body)
    );
    assert!(
        observation_body.contains("[harness] output overflowed"),
        "expected the clamped observation to carry a truncation marker, got:\n{observation_body}"
    );
}

// ---------------------------------------------------------------------
// Finish-reason-aware rejection: a `length`-truncated output explains why
// it was cut off instead of leaving the model to guess.
// ---------------------------------------------------------------------

/// A mock turn whose content is truncated mid-string (so JSON repair can't
/// save it -- see `core::skill::repair_unclosed_json`) and whose
/// `finish_reason` is `"length"` must produce a rejection reason that names
/// `done_reason=length` and steers the model toward `edit_file` instead of
/// `write_file`, per the spec's better-rejection-message requirement.
#[tokio::test]
async fn length_cutoff_rejection_explains_itself() {
    let root = tempfile::tempdir().unwrap();

    let truncated = r#"{"patch": {}, "action": {"type": "tool", "name": "write_file", "args": {"content": "abc"#;
    let turns = vec![
        MockTurn::new(truncated.to_string())
            .with_finish_reason("length")
            .with_usage(50, 1100),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("done_reason=length"),
        "rejection reason should explain the length cutoff: {reason}"
    );
    assert!(
        reason.contains("edit_file"),
        "rejection reason should steer toward edit_file: {reason}"
    );

    // Recovers normally on the next (well-formed) turn.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Parked(Park::Yield)),
        "got {outcome2:?}"
    );
}

// ---------------------------------------------------------------------
// 10. An overflowed tool output sets a persistent harness note, cleared
// once a later tool output fits.
// ---------------------------------------------------------------------

/// When a tool's output overflows the observation budget, the truncated
/// marker in the Observation section is only visible in the one prompt that
/// renders it. `HarnessState::last_overflow` makes the fact persistent: the
/// *next* prompt's Harness View must still say so via a `last_observation:`
/// line, and the committed state must reflect it. Once a later tool output
/// fits under budget, the note must be cleared and the line must stop
/// appearing.
#[tokio::test]
async fn overflowed_tool_output_sets_harness_note_until_next_fit() {
    use statie_core::Budgets;

    let root = tempfile::tempdir().unwrap();

    let big_text = "x".repeat(5_000);
    let turns = vec![
        // Step A: `echo` with a huge text -> the output overflows the
        // observation budget.
        MockTurn::new(
            json!({
                "patch": {"counter": 1},
                "action": {"type": "tool", "name": "echo", "args": {"text": big_text}},
            })
            .to_string(),
        ),
        // Step B: `echo` with a tiny text -> the output fits comfortably.
        MockTurn::new(
            json!({
                "patch": {"counter": 2},
                "action": {"type": "tool", "name": "echo", "args": {"text": "ok"}},
            })
            .to_string(),
        ),
        // Step C: just to inspect the prompt built after step B's output.
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));

    // The observation budget must stay small enough that the huge echo
    // output overflows it, but large enough for the harness's instructional
    // overflow marker itself to fit under `HeuristicTokenizer`'s coarse
    // char/4 estimate (the marker text alone costs roughly 100 tokens under
    // that estimate); 150 satisfies both.
    let mut cfg = config(&root);
    cfg.budgets = Budgets {
        total: 8000,
        observation: 150,
        augmentation: 500,
    };
    let (mut session, log) = make_session(Arc::clone(&model), cfg);

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step A: the huge echo output overflows the observation budget.
    let rec_a = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_a = log.append_record(rec_a).unwrap();
    assert!(matches!(outcome_a, Outcome::Continue), "got {outcome_a:?}");

    let (state_a, _) = active.session().state().await.unwrap();
    assert!(
        state_a.harness.last_overflow.is_some(),
        "expected last_overflow to be set in committed state after an overflowing tool output"
    );

    // Step B: the prompt built for this step is the *next* one after the
    // overflow -- its Harness View must carry the persistent note, and its
    // Observation section must show the overflow marker.
    let rec_b = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_b = log.append_record(rec_b).unwrap();
    assert!(matches!(outcome_b, Outcome::Continue), "got {outcome_b:?}");

    let prompts = model.prompts();
    assert!(
        prompts[1].user.contains("last_observation:"),
        "prompt after the overflow should carry a last_observation line:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[1].user.contains("[harness] output overflowed"),
        "prompt after the overflow should show the overflow marker in its Observation section:\n{}",
        prompts[1].user
    );

    // Step B's tool output ("ok") fits comfortably, so the harness note
    // should be cleared in committed state.
    let (state_b, _) = active.session().state().await.unwrap();
    assert!(
        state_b.harness.last_overflow.is_none(),
        "expected last_overflow to be cleared after a tool output that fit"
    );

    // Step C: the prompt built for this step is the next one after the fit
    // -- it must no longer carry the last_observation line.
    let rec_c = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec_c).unwrap();

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(
        !prompts[2].user.contains("last_observation:"),
        "prompt after a fitting tool output should not carry a last_observation line:\n{}",
        prompts[2].user
    );
}
