//! Milestone 2's validation-and-recovery test suite (statie-spec.md §8;
//! §10 "Invalid outputs recover without state corruption"): parse failures,
//! schema/rule/unacked-inbox rejections, retry-cap parking, unknown tools,
//! tool errors, and cancellation during tool execution all recover without
//! corrupting committed state.
//!
//! Reuses the `EchoTool` / `ToolSet` / `SessionConfig` / `Session` helpers
//! from `toy_loop.rs`, now shared via `tests/common/mod.rs`.

use super::common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use statie_core::{
    Event, LogKind, NullSink, Outcome, Park, Schema, Session, SessionLog, SessionLogExt, Skill,
    SkillSpec, Tool, ToolCtx, ToolError, ToolOutputFull, ToolSet, Version,
};
use statie_models::{MockModel, MockTurn};
use statie_patch::JsonMergePatch;
use statie_skills::{CodingWorkspace, ImplementSkill, PlanSkill, ToySkill, ToyWorkspace};

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

/// `ToySkill`, but with an extra tool name added to `tools()`. Several
/// pre-Fix-1 tests deliberately call a tool that is registered in the
/// `ToolSet` but was never in `ToySkill::tools()` (`["echo"]`) -- to
/// exercise unknown-tool handling, tool-execution-error handling, and
/// cancellation-during-tool-execution, independent of whether the tool is
/// "active". Since Fix 1 now rejects an inactive tool before any of that
/// runs, those tests need the tool under test to be active; this wrapper
/// grants that without touching the shared `ToySkill` used everywhere else.
struct ToyWithExtraTool {
    inner: ToySkill,
    extra: &'static str,
}

impl ToyWithExtraTool {
    fn new(target: u64, extra: &'static str) -> Self {
        ToyWithExtraTool {
            inner: ToySkill::new(target),
            extra,
        }
    }
}

impl Skill for ToyWithExtraTool {
    type Workspace = ToyWorkspace;
    type Patch = <ToySkill as Skill>::Patch;

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.inner.spec()
    }

    fn initial_workspace(&self) -> Self::Workspace {
        self.inner.initial_workspace()
    }

    fn tools(&self) -> Vec<String> {
        let mut t = self.inner.tools();
        t.push(self.extra.to_string());
        t
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        self.inner.is_terminal(s)
    }

    fn rules(
        &self,
        cur: &statie_core::CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), statie_core::PatchError> {
        self.inner.rules(cur, next)
    }
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
    let cfg = config(&root);
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(Arc::clone(&model))),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    // `fly` is added to the active tool set (Fix 1) but is still not
    // registered in the `ToolSet` -- so this exercises the unknown-tool
    // path specifically, not the not-active path.
    let skill = Arc::new(ToyWithExtraTool::new(100, "fly"));
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
    let cfg = config(&root);
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(Arc::clone(&model))),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    // `nope` is active (Fix 1) but still not registered in the `ToolSet` --
    // exercises the unknown-tool path, not the not-active path.
    let skill = Arc::new(ToyWithExtraTool::new(100, "nope"));
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
    let cfg2 = config(&root);
    let log2: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session2 = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log2),
        Arc::new(common::ArcModel(Arc::clone(&model2))),
        tool_set(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg2,
    );
    let skill2 = Arc::new(ToyWithExtraTool::new(100, "nope"));
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
        Arc::new(common::ArcModel(Arc::clone(&model))),
        tool_set_with(vec![Arc::new(FailingTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    // `failing` must be active for this test to reach tool execution at all
    // (Fix 1) -- it's not in `ToySkill::tools()` by default.
    let skill = Arc::new(ToyWithExtraTool::new(100, "failing"));
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
        Arc::new(common::ArcModel(Arc::clone(&model))),
        tool_set_with(vec![Arc::new(SlowTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    // `slow` must be active for this test to reach tool execution at all
    // (Fix 1) -- it's not in `ToySkill::tools()` by default.
    let skill = Arc::new(ToyWithExtraTool::new(100, "slow"));
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
        Arc::new(common::ArcModel(Arc::clone(&model2))),
        tool_set_with(vec![Arc::new(SlowTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg2,
    );
    let skill2 = Arc::new(ToyWithExtraTool::new(100, "slow"));
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
        Arc::new(common::ArcModel(model.clone())),
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
        notes: 400,
        working_set: 6000,
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
        notes: 400,
        working_set: 6000,
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

// ---------------------------------------------------------------------
// Fix 1: a handoff only happens on `done`. A patch that changes
// `next_skill` alongside a tool call (or any non-`done` action) must be
// rejected -- the skill instructions say "set next_skill and send done" --
// while a patch that leaves `next_skill` unchanged is not rejected, and
// `done` (with or without a freshly-set `next_skill`) still hands off.
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
struct HandoffWs {
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    next_skill: Option<String>,
}

/// A minimal skill whose `handoff` is driven entirely by `next_skill`,
/// mirroring the coding skill set's `common_handoff` (skills/src/coding/mod.rs)
/// but standalone so this test doesn't depend on the coding workspace.
struct HandoffSkill {
    name: &'static str,
    transitions: Vec<String>,
    spec: Arc<SkillSpec>,
}

impl HandoffSkill {
    fn new(name: &'static str, transitions: Vec<&str>) -> Self {
        HandoffSkill {
            name,
            transitions: transitions.into_iter().map(String::from).collect(),
            spec: Arc::new(SkillSpec::new(name, "a handoff test skill")),
        }
    }
}

impl Skill for HandoffSkill {
    type Workspace = HandoffWs;
    type Patch = JsonMergePatch<HandoffWs>;

    fn name(&self) -> &str {
        self.name
    }

    fn spec(&self) -> Arc<SkillSpec> {
        Arc::clone(&self.spec)
    }

    fn initial_workspace(&self) -> Self::Workspace {
        HandoffWs::default()
    }

    fn tools(&self) -> Vec<String> {
        vec!["echo".to_string()]
    }

    fn allowed_transitions(&self) -> Vec<String> {
        self.transitions.clone()
    }

    fn is_terminal(&self, _s: &Self::Workspace) -> bool {
        false
    }

    fn handoff(&self, s: &Self::Workspace) -> Option<String> {
        s.next_skill
            .as_deref()
            .filter(|n| *n != self.name)
            .map(String::from)
    }
}

fn handoff_session(
    model: Arc<MockModel>,
    cfg: statie_core::SessionConfig,
) -> (Session<HandoffWs>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut tools = ToolSet::new();
    tools.register(Arc::new(common::EchoTool));
    let session = Session::new(
        Box::new(statie_store::InMemoryStore::<HandoffWs>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// A tool-action step whose patch also changes `next_skill` is rejected
/// with the corrective message, and the *following* step -- same
/// `next_skill`, action `done` -- hands off.
#[tokio::test]
async fn tool_call_changing_next_skill_is_rejected_then_done_hands_off() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: a tool call that also sets next_skill -- must be rejected.
        MockTurn::new(
            json!({
                "patch": {"counter": 1, "next_skill": "b"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
            })
            .to_string(),
        ),
        // Step 2: same next_skill, this time with `done` -- must hand off.
        MockTurn::new(
            json!({
                "patch": {"next_skill": "b"},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = handoff_session(Arc::clone(&model), common::config(&root));

    let skill: Arc<HandoffSkill> = Arc::new(HandoffSkill::new("a", vec!["b"]));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "got {outcome1:?}; a rejection still yields Continue (or Failure at the retry cap)"
    );

    // The patch must not have committed: version unchanged, next_skill unset.
    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(version1, Version(0));
    assert_eq!(state1.agent.next_skill, None);

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
        reason.contains("next_skill") && reason.contains("`done`"),
        "rejection reason should explain that changing next_skill requires `done`: {reason}"
    );

    // Step 2: same next_skill, this time with `done` -- hands off.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "b"),
        "got {outcome2:?}"
    );
}

/// A `done` action that sets `next_skill` in the same step hands off
/// immediately (the primary, intended path).
#[tokio::test]
async fn done_with_next_skill_hands_off() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"next_skill": "b"},
            "action": {"type": "done"},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = handoff_session(Arc::clone(&model), common::config(&root));

    let skill: Arc<HandoffSkill> = Arc::new(HandoffSkill::new("a", vec!["b"]));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "b"),
        "got {outcome:?}"
    );
}

/// A patch that leaves `next_skill` unchanged (still unset) alongside a
/// tool action is not rejected: only a *change* to the handoff target
/// requires `done`.
#[tokio::test]
async fn tool_call_leaving_next_skill_unset_is_not_rejected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({
            "patch": {"counter": 1},
            "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
        })
        .to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = handoff_session(Arc::clone(&model), common::config(&root));

    let skill: Arc<HandoffSkill> = Arc::new(HandoffSkill::new("a", vec!["b"]));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (state, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
    assert_eq!(state.agent.counter, 1);
}

/// A patch that leaves `next_skill` unchanged at an already-set value
/// (re-sending the same target every step, as the coding skill spec
/// instructs) alongside a tool action is also not rejected: only a change
/// to the handoff target requires `done`.
#[tokio::test]
async fn tool_call_resending_same_next_skill_is_not_rejected() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: `done` with next_skill unset is fine (handoff is None ->
        // None, no change) -- establishes nothing; instead seed next_skill
        // via a first `done` step, then step 2 keeps resending the same
        // value alongside a tool call.
        MockTurn::new(
            json!({
                "patch": {"next_skill": "a"},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = handoff_session(Arc::clone(&model), common::config(&root));

    // Skill "a" setting next_skill to its own name is not a *change* in
    // handoff target (handoff() filters out its own name), so this must not
    // be rejected even though `next_skill` in the raw patch is non-null.
    let skill: Arc<HandoffSkill> = Arc::new(HandoffSkill::new("a", vec!["b"]));
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");

    let (_, version) = active.session().state().await.unwrap();
    assert_eq!(version, Version(1));
}

// ---------------------------------------------------------------------
// Fix 2: the loop-guard note is prepended to the tool output, not
// appended, so it survives `truncate_with_report`'s clamp on a large
// output.
// ---------------------------------------------------------------------

/// After two identical (name, args, output) calls with an output larger
/// than the observation budget, the third call's observation *starts with*
/// the loop-guard note -- proving the note is placed at the front, where it
/// survives truncation, rather than at the end where a large output would
/// push it out of the budget entirely.
#[tokio::test]
async fn loop_guard_note_survives_truncation_on_large_output() {
    let root = tempfile::tempdir().unwrap();

    // Larger than the observation budget set below, so truncation kicks in.
    let big_text = "x".repeat(5_000);
    let call = |i: u64| {
        MockTurn::new(
            json!({
                "patch": {"counter": i},
                "action": {"type": "tool", "name": "echo", "args": {"text": big_text}},
            })
            .to_string(),
        )
    };
    let model = Arc::new(MockModel::new(vec![
        call(1),
        call(2),
        call(3),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ]));

    let mut cfg = config(&root);
    cfg.budgets = statie_core::Budgets {
        total: 20_000,
        observation: 300,
        augmentation: 500,
        notes: 400,
        working_set: 6000,
    };
    let (mut session, log) = make_session(Arc::clone(&model), cfg);

    let skill: Arc<ToySkill> = Arc::new(ToySkill::new(50));
    let mut active = session.activate(skill).await.unwrap();
    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let prompts = model.prompts();
    let obs_idx = prompts[3]
        .user
        .find("## Observation")
        .expect("fourth prompt should carry an Observation section");
    let heading = "## Observation\n";
    let observation_body = prompts[3].user[obs_idx..]
        .strip_prefix(heading)
        .expect("Observation section should start with its heading");

    assert!(
        observation_body.starts_with("[harness] This is occurrence #3"),
        "observation should start with the loop-guard note even though the \
         underlying output overflowed the budget, got:\n{observation_body}"
    );
}

// ---------------------------------------------------------------------
// Fix 1: active tools are enforced at execution, not just suggested by the
// prompt. A tool that is registered in the full `ToolSet` but absent from
// the skill's `tools()` must be rejected at execution time, even though the
// model was able to name it. A call to a tool that *is* active still runs.
// ---------------------------------------------------------------------

#[tokio::test]
async fn tool_not_in_active_set_is_rejected_but_active_tool_still_runs() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: `failing` is registered in the ToolSet (via
        // `tool_set_with`) but is NOT in `ToySkill::tools()` (["echo"]
        // only) -- must be rejected before the tool ever runs.
        MockTurn::new(
            json!({
                "patch": {"counter": 1},
                "action": {"type": "tool", "name": "failing"},
            })
            .to_string(),
        ),
        // Step 2: `echo` is active -- must run normally.
        MockTurn::new(
            json!({
                "patch": {"counter": 2},
                "action": {"type": "tool", "name": "echo", "args": {"text": "hi"}},
            })
            .to_string(),
        ),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = config(&root);
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(Arc::clone(&model))),
        tool_set_with(vec![Arc::new(FailingTool)]),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Step 1: rejected at execution (not a validation failure -- the patch
    // still commits, per the unknown-tool precedent).
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(version1, Version(1));
    assert_eq!(state1.agent.counter, 1);

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_rejected")),
        "expected a tool_rejected system entry, got {sys_entries:?}"
    );

    // Step 2: `echo` (active) runs normally.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let tool_entries = log.entries(Some(LogKind::ToolOutput), 10_000);
    assert!(
        tool_entries
            .iter()
            .any(|e| e.payload.get("name").and_then(Value::as_str) == Some("echo")),
        "expected `echo` to have run, got {tool_entries:?}"
    );

    // Step 3's prompt (built after step 2) should show the rejection
    // observation naming the skill and its available tools from step 1.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();

    let prompts = model.prompts();
    assert!(
        prompts[1].user.contains("is not available in skill `toy`"),
        "second prompt should report the rejection:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[1].user.contains("echo"),
        "second prompt should list the available tools:\n{}",
        prompts[1].user
    );
}

// ---------------------------------------------------------------------
// Fix 2: an `InvalidArgs` tool error's observation carries the tool's args
// schema, not just serde's bare message, so the model can see the expected
// shape instead of guessing field names one rejection at a time.
// ---------------------------------------------------------------------

#[tokio::test]
async fn invalid_args_observation_carries_the_schema() {
    let root = tempfile::tempdir().unwrap();

    // `echo` requires `text: String` (see `common::EchoTool`); omit it so
    // the tool call fails with `ToolError::InvalidArgs`.
    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"counter": 1},
                "action": {"type": "tool", "name": "echo", "args": {"nope": "x"}},
            })
            .to_string(),
        ),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
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

    let prompts = model.prompts();
    assert!(
        prompts[1].user.contains("Expected arguments (JSON schema)"),
        "second prompt should carry the schema-bearing message:\n{}",
        prompts[1].user
    );
    assert!(
        prompts[1].user.contains("text"),
        "second prompt should mention the required field `text`:\n{}",
        prompts[1].user
    );
}

// ---------------------------------------------------------------------
// Fix 1 (plan bare `done` -> implement): a scripted `plan` session where
// the model writes a non-empty plan (with a tool call), then sends a bare
// `done` with no `next_skill` -- reproducing the observed failure mode
// where a local model wrote a full plan, then repeated action `done` with
// no `next_skill` while reasoning "I will now proceed to implement". The
// session must hand off to `implement`, not loop forever on the
// premature-`done` rejection.
// ---------------------------------------------------------------------

fn plan_session(
    model: Arc<MockModel>,
    cfg: statie_core::SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let mut tools = ToolSet::new();
    tools.register(Arc::new(common::EchoTool));
    let session = Session::new(
        Box::new(statie_store::InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

#[tokio::test]
async fn plan_bare_done_after_writing_plan_hands_off_to_implement() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: write a non-empty plan, alongside a tool call (the model
        // is still working, not yet trying to hand off).
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "do the thing", "status": "pending"}]
                },
                "action": {"type": "tool", "name": "echo", "args": {"text": "exploring"}},
            })
            .to_string(),
        ),
        // Step 2: a bare `done` -- no `next_skill` at all -- exactly the
        // shape the rejection was not correcting in the field report.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = plan_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(version1, Version(1));
    assert_eq!(state1.agent.plan.len(), 1);
    assert_eq!(state1.agent.next_skill, None);

    // Step 2: bare `done`, no `next_skill` -- must NOT be rejected; must
    // hand off to `implement` with `next_skill` injected into the commit.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}; bare `done` with a non-empty plan should hand off to implement"
    );

    let (state2, version2) = active.session().state().await.unwrap();
    assert_eq!(version2, Version(2));
    assert_eq!(state2.agent.next_skill.as_deref(), Some("implement"));

    // No rejection should have been logged for the bare-done step.
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        !sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "the bare done step should not have been rejected, got {sys_entries:?}"
    );
}

/// A bare `done` (no `next_skill`) with an *empty* plan now converts to
/// `Action::Continue` instead of being rejected -- updated for this unit
/// (statie-spec.md "the plan skill: `done` with an empty plan commits and
/// notes"), which treats an empty-plan `done` the way `premature_done_note`
/// treats an open plan step in `implement`: the (empty) patch commits, no
/// retry is spent, and the model is told in one line to write the plan
/// first. Previously this step was rejected with a "set next_skill" message
/// and did not commit; see `plan_done_with_next_skill_and_empty_plan_continues_and_strips_next_skill`
/// below for the sibling case (the model sending `next_skill` alongside
/// `done`, also converted) and `skills/tests/premature_done.rs` for the
/// canonical, more detailed coverage of this conversion.
#[tokio::test]
async fn plan_bare_done_with_empty_plan_continues_then_recovers() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: bare `done` with nothing written to `plan` yet -- must
        // convert to Continue, not reject.
        MockTurn::new(json!({"patch": {}, "action": {"type": "done"}}).to_string()),
        // Step 2: model recovers by writing the plan and setting
        // next_skill explicitly.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "do the thing", "status": "pending"}],
                    "next_skill": "implement"
                },
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = plan_session(Arc::clone(&model), config(&root));

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let (state1, version1) = active.session().state().await.unwrap();
    assert_eq!(
        version1,
        Version(1),
        "the (empty) patch must still commit, unlike the old rejection"
    );
    assert!(state1.agent.plan.is_empty());
    assert_eq!(state1.agent.next_skill, None);

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        !sys_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected")),
        "the empty-plan bare done must not have been rejected, got {sys_entries:?}"
    );
    let continued = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("continued"))
        .expect("expected a continued system entry for the empty-plan bare done");
    let note = continued
        .payload
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        note.contains("the plan is empty"),
        "note should explain the plan is empty: {note}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome2:?}"
    );
}

// ---------------------------------------------------------------------
// Fix 4: the loop guard covers tool errors, not just successful tool
// output. A tool call repeated with identical (name, args) that fails the
// same way every time must eventually carry the same "occurrence #N ...
// nothing has changed" note the success path already gets.
// ---------------------------------------------------------------------

#[tokio::test]
async fn repeated_invalid_args_tool_error_triggers_loop_guard() {
    let root = tempfile::tempdir().unwrap();

    // `echo` (common::EchoTool) fails with `InvalidArgs` when `text` is
    // missing; call it identically three times, then recover.
    let call = || {
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "tool", "name": "echo", "args": {"nope": "x"}},
            })
            .to_string(),
        )
    };
    let model = Arc::new(MockModel::new(vec![
        call(),
        call(),
        call(),
        MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string()),
    ]));
    let (mut session, log) = make_session(Arc::clone(&model), config(&root));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let prompts = model.prompts();
    // prompts[0]: first call (no history yet).
    // prompts[1]: after 1st failure -> 2nd call's prompt, not yet flagged.
    // prompts[2]: after 2nd failure -> 3rd call's prompt, not yet flagged
    // (2 prior occurrences is the threshold, matching the success path).
    assert!(
        !prompts[2].user.contains("[harness] This is occurrence"),
        "third call's prompt should not yet be flagged:\n{}",
        prompts[2].user
    );
    // prompts[3]: after the 3rd identical failure -> flagged.
    assert!(
        prompts[3].user.contains("[harness] This is occurrence #3"),
        "fourth prompt should carry the loop-guard note after three identical \
         InvalidArgs failures:\n{}",
        prompts[3].user
    );
}

// ---------------------------------------------------------------------
// Fix 5: rejection observations change between retries so a
// low-temperature model that resends byte-identical output is told, not
// just left to guess, that nothing changed.
// ---------------------------------------------------------------------

#[tokio::test]
async fn consecutive_same_reason_rejections_are_flagged_from_the_second() {
    let root = tempfile::tempdir().unwrap();

    // Same malformed content every turn -> identical rejection reason each
    // time, exercising the "repeated" branch in `reject`.
    let turns = vec![
        MockTurn::new("not json a".to_string()),
        MockTurn::new("not json a".to_string()),
        MockTurn::new(json!({"patch": {"counter": 1}, "action": {"type": "yield"}}).to_string()),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_session(Arc::clone(&model), config_with_retries(&root, 3));

    let skill = toy(100);
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    // Turn 1: first rejection -- attempt count shown, but no "repeated" nudge yet.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Turn 2: second rejection, same reason as turn 1's -- the *next*
    // prompt (built for turn 2) must show "attempt 1 of" (that's the
    // observation carried from turn 1's rejection) and must NOT yet
    // contain the repeated-nudge sentence, since turn 1 was the first
    // rejection.
    let prompts_before_turn2 = model.prompts();
    assert_eq!(
        prompts_before_turn2.len(),
        1,
        "only turn 1 has been sent so far"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[1].user.contains("attempt 1 of"),
        "second prompt (after the first rejection) should show the attempt count:\n{}",
        prompts[1].user
    );
    assert!(
        !prompts[1].user.contains("send something different"),
        "second prompt should not yet carry the repeated-rejection nudge \
         (only the first rejection has happened so far):\n{}",
        prompts[1].user
    );

    // Turn 3's prompt (built after turn 2's rejection, which repeated the
    // same reason as turn 1) must show "attempt 2 of" and the nudge.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Parked(Park::Yield)),
        "got {outcome3:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts[2].user.contains("attempt 2 of"),
        "third prompt (after the second, repeated rejection) should show attempt 2:\n{}",
        prompts[2].user
    );
    assert!(
        prompts[2].user.contains("send something different"),
        "third prompt should carry the repeated-rejection nudge:\n{}",
        prompts[2].user
    );
}

// ---------------------------------------------------------------------
// Read-without-edit guard: tracks, per file path, how many consecutive
// read-only tool calls have targeted it since the last mutating call.
// From the 4th consecutive read of the same path, an observation note
// fires and a `read_guard` system log entry is recorded; a mutating call
// (write_file/edit_file/shell/test) resets the counters.
// ---------------------------------------------------------------------

/// `ToySkill`, but with several extra tool names added to `tools()` --
/// `ToyWithExtraTool` (above) only grants one. Used for the read-guard
/// tests, which need `read_file`/`write_file`/`edit_file`/`shell` active
/// alongside `echo`.
struct ToyWithExtraTools {
    inner: ToySkill,
    extra: Vec<&'static str>,
}

impl ToyWithExtraTools {
    fn new(target: u64, extra: Vec<&'static str>) -> Self {
        ToyWithExtraTools {
            inner: ToySkill::new(target),
            extra,
        }
    }
}

impl Skill for ToyWithExtraTools {
    type Workspace = ToyWorkspace;
    type Patch = <ToySkill as Skill>::Patch;

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn spec(&self) -> Arc<SkillSpec> {
        self.inner.spec()
    }

    fn initial_workspace(&self) -> Self::Workspace {
        self.inner.initial_workspace()
    }

    fn tools(&self) -> Vec<String> {
        let mut t = self.inner.tools();
        t.extend(self.extra.iter().map(|s| s.to_string()));
        t
    }

    fn is_terminal(&self, s: &Self::Workspace) -> bool {
        self.inner.is_terminal(s)
    }

    fn rules(
        &self,
        cur: &statie_core::CommittedState<Self::Workspace>,
        next: &Self::Workspace,
    ) -> Result<(), statie_core::PatchError> {
        self.inner.rules(cur, next)
    }
}

/// Builds a session wired with the real `statie-tools` file tools
/// (`read_file`, `write_file`, `edit_file`, `shell`) plus `echo`, sandboxed
/// to `root`, so the read-guard tests exercise the actual tools rather than
/// the toy `EchoTool`.
fn make_file_tools_session(
    model: Arc<MockModel>,
    root: &tempfile::TempDir,
) -> (statie_core::Session<ToyWorkspace>, Arc<dyn SessionLog>) {
    let mut tools = ToolSet::new();
    tools.register(Arc::new(common::EchoTool));
    tools.register(Arc::new(statie_tools::ReadFile::default()));
    tools.register(Arc::new(statie_tools::WriteFile {
        max_output_bytes: 200_000,
    }));
    tools.register(Arc::new(statie_tools::EditFile {
        max_output_bytes: 200_000,
    }));
    tools.register(Arc::new(statie_tools::Shell {
        default_timeout_secs: 5,
        max_output_bytes: 200_000,
    }));

    let log: Arc<dyn SessionLog> = Arc::new(statie_core::MemoryLog::new());
    let session = Session::new(
        Box::new(statie_store::InMemoryStore::<ToyWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(common::ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        config(root),
    );
    (session, log)
}

/// A `read_file` action naming `path`/`offset`.
fn read_call(counter: u64, path: &str, offset: u64) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"counter": counter},
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": offset, "limit": 5},
            },
        })
        .to_string(),
    )
}

fn yield_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "yield"}}).to_string())
}

/// Number of `read_guard` system-log entries recorded so far.
fn read_guard_log_count(log: &Arc<dyn SessionLog>) -> usize {
    log.entries(Some(LogKind::System), usize::MAX)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("read_guard"))
        .count()
}

#[tokio::test]
async fn fourth_consecutive_read_of_same_path_with_varying_offsets_triggers_note() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "line 1\nline 2\nline 3\n").unwrap();

    // Four `read_file` calls on the same path, with different offsets each
    // time -- the scenario observed live (655/695/795, 650/770, ...) that
    // slips past the identical-(name,args,output) loop guard. All four
    // offsets are distinct (unlike the original 1/2/3/1 sequence) so the
    // (separate) redundant-identical-read guard, which keys on the full
    // call including args, never fires here instead of the read-without-edit
    // streak guard under test (which keys on path alone, ignoring args).
    let turns = vec![
        read_call(1, "lib.rs", 1),
        read_call(2, "lib.rs", 2),
        read_call(3, "lib.rs", 3),
        read_call(4, "lib.rs", 4),
        yield_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), &root);

    let skill: Arc<ToyWithExtraTools> = Arc::new(ToyWithExtraTools::new(100, vec!["read_file"]));
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let prompts = model.prompts();
    // prompts[i] is what the model saw *before* answering turn i (0-based);
    // the observation from read N is shown in prompts[N] (the prompt built
    // for the next turn). So the 4th read's note shows in prompts[4]... but
    // we only ran 4 steps (turns 0-3), producing prompts[0..4]; the 4th
    // read's resulting observation is consumed by the 5th step, which we
    // haven't run. Instead, assert on prompts[3] (built after the 3rd read)
    // showing no note, and run one more step to see the 4th read's note.
    assert!(
        !prompts[3].user.contains("[harness] This is read #"),
        "third prompt (after the 3rd read) must not carry a read-guard note:\n{}",
        prompts[3].user
    );

    // Turn 5 (yield) -- its prompt is built after the 4th read's
    // observation, which must carry the note.
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome5 = log.append_record(rec5).unwrap();
    assert!(
        matches!(outcome5, Outcome::Parked(Park::Yield)),
        "got {outcome5:?}"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 5);
    assert!(
        prompts[4]
            .user
            .contains("[harness] This is read #4 of `lib.rs`"),
        "fifth prompt (after the 4th read) must carry the read-guard note:\n{}",
        prompts[4].user
    );
    assert_eq!(
        read_guard_log_count(&log),
        1,
        "exactly one read_guard log entry, for the 4th read"
    );
}

#[tokio::test]
async fn edit_between_reads_resets_read_guard_counter() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "line 1\nline 2\nline 3\n").unwrap();

    // read, read, read, edit, read -- the edit must reset the counter for
    // `lib.rs`, so the final read (which would otherwise be the 4th
    // consecutive read) carries no note.
    let turns = vec![
        read_call(1, "lib.rs", 1),
        read_call(2, "lib.rs", 2),
        read_call(3, "lib.rs", 3),
        MockTurn::new(
            json!({
                "patch": {"counter": 4},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "lib.rs", "old": "line 2", "new": "line two"},
                },
            })
            .to_string(),
        ),
        read_call(5, "lib.rs", 1),
        yield_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), &root);

    let skill: Arc<ToyWithExtraTools> =
        Arc::new(ToyWithExtraTools::new(100, vec!["read_file", "edit_file"]));
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..6 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue | Outcome::Parked(Park::Yield)),
            "got {outcome:?}"
        );
    }

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 6);
    for (i, p) in prompts.iter().enumerate() {
        assert!(
            !p.user.contains("[harness] This is read #"),
            "prompt {i} must not carry a read-guard note (edit reset the counter):\n{}",
            p.user
        );
    }
    assert_eq!(
        read_guard_log_count(&log),
        0,
        "no read_guard log entry: the edit reset the counter before the 4th read"
    );
}

#[tokio::test]
async fn interleaved_reads_of_two_paths_do_not_trigger_read_guard() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "a1\na2\na3\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "b1\nb2\nb3\n").unwrap();

    // a, b, a, b -- each path reaches a streak of only 2, never the
    // threshold of 4.
    let turns = vec![
        read_call(1, "a.rs", 1),
        read_call(2, "b.rs", 1),
        read_call(3, "a.rs", 2),
        read_call(4, "b.rs", 2),
        yield_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), &root);

    let skill: Arc<ToyWithExtraTools> = Arc::new(ToyWithExtraTools::new(100, vec!["read_file"]));
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..5 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue | Outcome::Parked(Park::Yield)),
            "got {outcome:?}"
        );
    }

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 5);
    for (i, p) in prompts.iter().enumerate() {
        assert!(
            !p.user.contains("[harness] This is read #"),
            "prompt {i} must not carry a read-guard note (each path only reaches 2):\n{}",
            p.user
        );
    }
    assert_eq!(read_guard_log_count(&log), 0);
}

#[tokio::test]
async fn shell_call_resets_all_read_guard_counters() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "a1\na2\na3\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "b1\nb2\nb3\n").unwrap();

    // Three reads of `a.rs`, a `shell` call, then one more read of `a.rs`
    // -- the shell call must reset the counter, so the final read (which
    // would otherwise be the 4th consecutive read) carries no note.
    let turns = vec![
        read_call(1, "a.rs", 1),
        read_call(2, "a.rs", 2),
        read_call(3, "a.rs", 3),
        MockTurn::new(
            json!({
                "patch": {"counter": 4},
                "action": {
                    "type": "tool",
                    "name": "shell",
                    "args": {"cmd": "true"},
                },
            })
            .to_string(),
        ),
        read_call(5, "a.rs", 1),
        yield_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), &root);

    let skill: Arc<ToyWithExtraTools> =
        Arc::new(ToyWithExtraTools::new(100, vec!["read_file", "shell"]));
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..6 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue | Outcome::Parked(Park::Yield)),
            "got {outcome:?}"
        );
    }

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 6);
    for (i, p) in prompts.iter().enumerate() {
        assert!(
            !p.user.contains("[harness] This is read #"),
            "prompt {i} must not carry a read-guard note (shell reset every counter):\n{}",
            p.user
        );
    }
    assert_eq!(
        read_guard_log_count(&log),
        0,
        "no read_guard log entry: the shell call reset the counter before the 4th read"
    );
}

// ---------------------------------------------------------------------
// Commitment slot: a decision made in one step binds the next. Set `next`
// (tool/path/what) on `CodingWorkspace` and the harness holds the model to
// it: at most 2 reads of the committed path, then the harness expects the
// committed call (or a changed `next`, explained in `recent_summary`).
// ---------------------------------------------------------------------

/// A `read_file` action on `CodingWorkspace`, with an optional patch object
/// merged in (e.g. to set/change `next`).
fn coding_read_call(path: &str, patch_extra: Value) -> MockTurn {
    let mut patch = serde_json::json!({});
    if let Value::Object(extra) = patch_extra
        && let Value::Object(map) = &mut patch
    {
        map.extend(extra);
    }
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": 1, "limit": 5},
            },
        })
        .to_string(),
    )
}

/// An `ask_user` action with an empty patch -- used as a harmless filler
/// step in tests that don't care about the outcome (the coding skills all
/// disallow `yield`, so this stands in for a no-op turn).
fn coding_ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// Sets `next` to an `edit_file` commitment on `path`, in the same step as a
/// `read_file` call on that same path -- this both establishes the
/// commitment and counts as its first read (read #1 of the 2-read cap),
/// mirroring how a model would realistically commit right as it starts
/// looking at the file.
fn set_commitment_and_read_turn(path: &str, what: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {
                "next": {"tool": "edit_file", "path": path, "what": what},
            },
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": 1, "limit": 5},
            },
        })
        .to_string(),
    )
}

/// Sets `next` to an `edit_file` commitment on `path` via `ask_user` (a
/// non-tool action, so the commitment-slot rule never gates *this* step) --
/// used when a test wants a clean "commitment is now set" starting point
/// before its first tool call under that commitment.
fn set_commitment_via_ask_user_turn(path: &str, what: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {
                "next": {"tool": "edit_file", "path": path, "what": what},
            },
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )
}

#[tokio::test]
async fn third_read_of_committed_path_is_rejected_then_edit_is_allowed_and_clears_next() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        // Step 1: commit to editing a.rs, reading it in the same step (read #1 of 2).
        set_commitment_and_read_turn("a.rs", "replace `old` with `new` in fn signature"),
        // Step 2: a second read of a.rs, with args different from step 1's read (so the
        // redundant-identical-read guard does not fire here instead of the commitment-slot
        // rule under test) -- still within the 2-read cap.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 2, "limit": 5},
                },
            })
            .to_string(),
        ),
        // Step 3: a third read of a.rs, with args different from step 2's (same reason) --
        // over the commitment-slot cap, must be rejected for that, not for repeating step 2.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 3, "limit": 5},
                },
            })
            .to_string(),
        ),
        // Step 4 (after the rejection is retried): the committed edit_file call.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "a.rs", "old": "fn old() {}", "new": "fn new() {}"},
                },
            })
            .to_string(),
        ),
        // Step 5: just to inspect the prompt built after step 4's result.
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: sets `next` and reads a.rs (read #1) -- allowed.
    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Step 2: second read -- still allowed.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    // Step 3: third read -- rejected.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .rfind(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry for the third read");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("you committed to `edit_file a.rs`"),
        "rejection should name the commitment: {reason}"
    );
    assert!(
        reason.contains("make that call now"),
        "rejection should tell the model to make the call: {reason}"
    );

    // Step 4: the committed edit_file call is allowed.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");

    // Step 5: inspect the prompt built from step 4's result -- its
    // Observation section must start with "commitment fulfilled".
    let rec5 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec5).unwrap();

    let prompts = model.prompts();
    let last_prompt = prompts.last().expect("expected a prompt after the edit");
    let obs_idx = last_prompt
        .user
        .find("## Observation")
        .expect("expected an Observation section");
    let observation = &last_prompt.user[obs_idx..];
    assert!(
        observation.contains("commitment fulfilled: edit_file a.rs"),
        "observation after the committed edit should start with commitment fulfilled:\n{observation}"
    );

    // The committed state has next == None, with a harness-authored patch
    // entry (source: "harness") in the log clearing it.
    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "next should be cleared after the commitment was fulfilled, got {:?}",
        state.agent.next
    );

    let patch_entries = log.entries(Some(LogKind::Patch), 10_000);
    let harness_patch = patch_entries.iter().find(|e| {
        e.payload.get("source").and_then(Value::as_str) == Some("harness")
            && e.payload
                .get("patch")
                .and_then(|p| p.get("next"))
                .is_some_and(Value::is_null)
    });
    assert!(
        harness_patch.is_some(),
        "expected a harness-authored patch entry clearing `next`, got {patch_entries:?}"
    );

    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        "fn new() {}\n"
    );
}

#[tokio::test]
async fn read_of_a_different_file_while_committed_is_rejected_on_first_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();

    let turns = vec![
        set_commitment_via_ask_user_turn("a.rs", "fix fn a"),
        coding_read_call("b.rs", Value::Object(Default::default())),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    // Resume with a user message so the model can take its next turn.
    let rec2 = active
        .step(Event::User("go ahead".to_string()), &mut NullSink)
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry for the off-commitment read");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        reason.contains("you committed to `edit_file a.rs`"),
        "rejection should name the commitment even though b.rs was read: {reason}"
    );
}

#[tokio::test]
async fn changing_next_in_the_same_patch_as_a_read_resets_the_count() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();

    let turns = vec![
        set_commitment_via_ask_user_turn("a.rs", "fix fn a"),
        coding_read_call("a.rs", Value::Object(Default::default())),
        // A different `offset` than the read above, so the (separate)
        // redundant-identical-read guard does not reject this one instead of
        // the commitment-slot logic this test exercises.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 2, "limit": 5},
                },
            })
            .to_string(),
        ),
        // This step both changes `next` to b.rs AND reads b.rs in the same
        // patch: since `next` changed, `commitment_reads` resets to 0
        // before this read is judged, so it counts as read #1 on the new
        // commitment and is allowed (not the 3rd read of the exhausted one).
        coding_read_call(
            "b.rs",
            serde_json::json!({"next": {"tool": "edit_file", "path": "b.rs", "what": "fix fn b"}}),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active
        .step(Event::User("go ahead".to_string()), &mut NullSink)
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    for _ in 0..2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "got {outcome:?}: the read that changes `next` in the same patch must be allowed"
        );
    }

    let (state, _) = active.session().state().await.unwrap();
    let next = state.agent.next.expect("next should still be set to b.rs");
    assert_eq!(next.path.as_deref(), Some("b.rs"));
}

#[tokio::test]
async fn mutating_call_that_does_not_match_commitment_adds_note_and_leaves_next_set() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("c.rs"), "fn c() {}\n").unwrap();

    let turns = vec![
        set_commitment_via_ask_user_turn("a.rs", "fix fn a"),
        // A write to a totally different file -- does not match the
        // commitment (different tool AND different path).
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "write_file",
                    "args": {"path": "c.rs", "content": "fn c2() {}\n"},
                },
            })
            .to_string(),
        ),
        // Just to inspect the prompt built from the write's result.
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Parked(Park::AskUser(_))));

    let rec2 = active
        .step(Event::User("go ahead".to_string()), &mut NullSink)
        .await
        .unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();

    let prompts = model.prompts();
    let last_prompt = prompts.last().expect("expected a prompt after the write");
    assert!(
        last_prompt
            .user
            .contains("note: `next` still says edit_file a.rs"),
        "expected the mismatch note in the next prompt:\n{}",
        last_prompt.user
    );

    // `next` is left alone: still set to the original commitment.
    let (state, _) = active.session().state().await.unwrap();
    let next = state.agent.next.expect("next should still be set");
    assert_eq!(next.path.as_deref(), Some("a.rs"));
    assert_eq!(next.tool, "edit_file");
}

#[tokio::test]
async fn no_commitment_means_reads_are_unlimited() {
    // `PlanSkill`, not `ImplementSkill`: `ImplementSkill` now has
    // `silent_read_cap() == Some(2)` (the reads-require-a-commitment guard,
    // `session.rs` guard 2), so it *does* cap consecutive no-commitment
    // reads whose patch is empty -- see
    // `third_silent_read_is_rejected_then_setting_next_recovers`.
    // This test is about the (separate) commitment-slot rule, which never
    // caps reads while no commitment is set; `PlanSkill` has no read cap of
    // its own (`silent_read_cap()` defaults to `None`), so it isolates that
    // rule the way this test originally intended. Each read below uses a
    // different `offset` so the (separate) redundant-identical-read guard
    // never fires here either -- this test is only about the count, not
    // about repeating the exact same call.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 1, "limit": 5},
                },
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 2, "limit": 5},
                },
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 3, "limit": 5},
                },
            })
            .to_string(),
        ),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 4, "limit": 5},
                },
            })
            .to_string(),
        ),
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..5 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "got {outcome:?}: with no commitment set, reads are not capped by the \
             commitment-slot rule (the pre-existing read-without-edit guard is separate \
             and only *notes*, never rejects)"
        );
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "plan has no silent_read_cap, so none of these reads should be rejected: \
         {sys_entries:?}"
    );
}

#[tokio::test]
async fn coding_workspace_json_without_next_still_deserializes() {
    let json = serde_json::json!({
        "task": "do the thing",
        "plan": [],
        "files": [],
        "findings": [],
        "recent_summary": "",
        "next_skill": null,
        "done": false,
        "reports": {},
    });
    let ws: CodingWorkspace = serde_json::from_value(json).expect(
        "CodingWorkspace JSON without a `next` key must still deserialize (via #[serde(default)])",
    );
    assert!(ws.next.is_none());
}

/// A committed `edit_file` whose anchor is not found must not spend the
/// commitment: the edit did not happen, so `next` stays set and the
/// observation carries the tool error, not "commitment fulfilled".
#[tokio::test]
async fn failed_committed_edit_keeps_next_set() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        set_commitment_and_read_turn("a.rs", "replace `old` with `new`"),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "a.rs", "old": "fn missing() {}", "new": "fn new() {}"},
                },
            })
            .to_string(),
        ),
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();
    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_some(),
        "a failed edit must leave the commitment in place"
    );
    let prompts = model.prompts();
    let third = &prompts[2].user;
    assert!(
        third.contains("not found"),
        "third prompt should carry the edit error, got:\n{third}"
    );
    assert!(
        !third.contains("commitment fulfilled"),
        "a failed edit must not be reported as fulfilled:\n{third}"
    );
    // Milestone 10: step 1's successful `read_file` (toward the
    // commitment) legitimately produces its own harness-authored `seen`
    // patch (`reason: "after_tool"`) -- unrelated to the commitment-clear
    // path this assertion guards, which is distinguished by carrying no
    // `reason` field and patching `next` specifically.
    let patch_entries = log.entries(Some(LogKind::Patch), 10_000);
    assert!(
        patch_entries.iter().all(|e| {
            e.payload.get("source").and_then(Value::as_str) != Some("harness")
                || e.payload.get("reason").and_then(Value::as_str) == Some("after_tool")
        }),
        "no harness clear patch should have been committed: {patch_entries:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        "fn old() {}\n"
    );
}

// ---------------------------------------------------------------------
// Guard 1 (7.7): redundant verification. An exact repeat of the most
// recently run `shell`/`test` command, with nothing mutated in between,
// gives the same result -- the step still commits (patch + action + acks),
// but the call itself is skipped rather than re-run, so a valid patch
// riding along with the redundant call is never discarded.
// ---------------------------------------------------------------------

/// A `shell` action with the given `cmd`, no patch changes.
fn coding_shell_call(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "shell",
                "args": {"cmd": cmd},
            },
        })
        .to_string(),
    )
}

/// A `shell` action with the given `cmd`, alongside a patch that changes
/// `recent_summary` -- used to prove that a redundant repeat's *patch*
/// still commits even though the call itself is skipped.
fn coding_shell_call_with_summary(cmd: &str, summary: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"recent_summary": summary},
            "action": {
                "type": "tool",
                "name": "shell",
                "args": {"cmd": cmd},
            },
        })
        .to_string(),
    )
}

/// A `test` action with the given `cmd`, no patch changes.
fn coding_test_call(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "test",
                "args": {"cmd": cmd},
            },
        })
        .to_string(),
    )
}

/// Finds the reason of the most recent `rejected` System entry, or panics.
fn last_rejection_reason(log: &Arc<dyn SessionLog>) -> String {
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejected = sys_entries
        .iter()
        .rfind(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .expect("expected a rejected system entry");
    rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Finds the most recent `skipped_redundant` System entry, or panics.
fn last_skipped_redundant(log: &Arc<dyn SessionLog>) -> Value {
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    sys_entries
        .iter()
        .rfind(|e| e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant"))
        .expect("expected a skipped_redundant system entry")
        .payload
        .clone()
}

#[tokio::test]
async fn repeating_the_same_shell_command_commits_the_patch_and_skips_the_call() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        coding_shell_call("echo hi"),
        coding_shell_call_with_summary("echo hi", "ran the tests again"),
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "first `echo hi` should be accepted, got {outcome1:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    // No tool actually ran for the redundant step: no `tool_output` entry.
    // (Checked before `append_record`, which consumes `rec2`.)
    assert!(
        !rec2.entries().iter().any(|e| e.kind == LogKind::ToolOutput),
        "the skipped call must not produce a tool_output entry: {:?}",
        rec2.entries()
    );
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the redundant repeat still commits as a normal step, got {outcome2:?}"
    );

    // The patch committed even though the call was skipped: `recent_summary`
    // reflects the second turn's patch.
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.recent_summary, "ran the tests again");

    // A `skipped_redundant` System entry carries the exact guard reason.
    let skipped = last_skipped_redundant(&log);
    let reason = skipped.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(
        reason.contains("nothing has changed since you last ran `echo hi`"),
        "reason: {reason}"
    );
    assert!(reason.contains("it returned: hi"), "reason: {reason}");
    assert_eq!(skipped.get("name").and_then(Value::as_str), Some("shell"));

    // The next prompt shows the guard reason plus the cached prior result,
    // not a fresh tool run.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    let obs = &prompts[2].user;
    assert!(
        obs.contains("nothing has changed since you last ran `echo hi`"),
        "{obs}"
    );
    assert!(
        obs.contains("(call not re-run; this is the earlier result)"),
        "{obs}"
    );
}

#[tokio::test]
async fn piped_repeat_of_the_same_command_is_also_skipped() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        coding_shell_call("echo hi"),
        coding_shell_call("echo hi | cat"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let mut last_outcome = None;
    for _ in 0..2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        last_outcome = Some(log.append_record(rec).unwrap());
    }
    assert!(
        matches!(last_outcome, Some(Outcome::Continue)),
        "got {last_outcome:?}"
    );

    let skipped = last_skipped_redundant(&log);
    let reason = skipped.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(
        reason.contains("nothing has changed since you last ran `echo hi`"),
        "normalization should treat `echo hi | cat` as the same command: {reason}"
    );
}

#[tokio::test]
async fn shell_repeat_after_an_edit_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        coding_shell_call("echo hi"),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "a.rs", "old": "fn old() {}", "new": "fn new() {}"},
                },
            })
            .to_string(),
        ),
        coding_shell_call("echo hi"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "an edit in between should reset the redundant-verification guard: {sys_entries:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        "fn new() {}\n"
    );
}

#[tokio::test]
async fn different_shell_commands_are_both_accepted() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![coding_shell_call("echo hi"), coding_shell_call("echo bye")];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "different commands must not trigger the guard: {sys_entries:?}"
    );
}

#[tokio::test]
async fn test_tool_then_shell_with_same_command_string_both_run() {
    // Fix 1 (`core/src/session.rs`, `VerifyRun::tool`): the redundant-
    // verification guard's memory is now keyed on the TOOL as well as the
    // normalized command -- a `test` after a `shell` of the identical
    // command (or vice versa) is no longer treated as redundant, because
    // the green checkpoint (`note_test_result`) only listens to the `test`
    // tool. A live run showed the model run `cargo test` through `shell`
    // once, then have every following `test` call with the same command
    // skipped as redundant: the harness never saw a passing test, so its
    // own substitution kept inserting `test`, and the guard kept skipping
    // it -- 51 skips in one run. This test proves the fix: a `test` call
    // right after a `shell` call of the identical command actually runs
    // (and vice versa would too, by the same symmetric check).
    //
    // The `test` tool's own auto-detected/explicit command isn't portable
    // in a bare temp sandbox (no Cargo.toml/package.json/etc. to detect,
    // and the underlying command must actually run in CI); an explicit
    // `cmd` on the `test` tool runs the given string exactly the way
    // `shell` does (see `tools/src/test_tool.rs`), so `echo hi` works
    // identically through either tool.
    let root = tempfile::tempdir().unwrap();

    let turns = vec![coding_test_call("echo hi"), coding_shell_call("echo hi")];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        rec2.entries().iter().any(|e| e.kind == LogKind::ToolOutput),
        "the second `shell echo hi` must actually run -- a `test` run of the same command does \
         not make a later `shell` run of it redundant: {:?}",
        rec2.entries()
    );
    assert!(
        !rec2.entries().iter().any(|e| e.kind == LogKind::System
            && e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")),
        "the second call must not be logged as a skip: {:?}",
        rec2.entries()
    );
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
}

// ---------------------------------------------------------------------
// Guard 2: reads require a commitment (skill-driven). While no commitment
// is set, a skill with `silent_read_cap() == Some(n)` rejects the (n+1)th
// consecutive read-only call whose patch is EMPTY (no keys). A read whose
// patch writes anything (a file note, a plan step, `recent_summary`) is
// never counted and resets the streak: reading is not the problem; reading
// that records nothing is.
// ---------------------------------------------------------------------

/// A bare `read_file` action on `path` with an empty patch (no `next`
/// change, no notes) -- distinct from `coding_read_call`, which always
/// merges in `patch_extra`; here we want a plain, silent read.
fn coding_bare_read_call(path: &str) -> MockTurn {
    coding_read_call(path, Value::Object(Default::default()))
}

#[tokio::test]
async fn third_silent_read_is_rejected_then_setting_next_recovers() {
    let root = tempfile::tempdir().unwrap();
    for f in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(root.path().join(f), "fn x() {}\n").unwrap();
    }

    let turns = vec![
        // Steps 1-2: two bare (empty-patch) reads, no commitment -- allowed
        // (cap is 2).
        coding_bare_read_call("a.rs"),
        coding_bare_read_call("b.rs"),
        // Step 3: a third bare read -- over the cap, rejected.
        coding_bare_read_call("c.rs"),
        // Step 3 (retry): sets `next` and reads c.rs in the same patch -- accepted.
        coding_read_call(
            "c.rs",
            json!({"next": {"tool": "edit_file", "path": "c.rs", "what": "fix fn x"}}),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 1..=2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "silent read #{i} should be accepted, got {outcome:?}"
        );
        let rejected_so_far = log
            .entries(Some(LogKind::System), 10_000)
            .iter()
            .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
            .count();
        assert_eq!(rejected_so_far, 0, "no rejection expected before the cap");
    }

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let reason = last_rejection_reason(&log);
    assert!(
        reason.contains("2 read(s) in a row that wrote nothing to the workspace"),
        "reason: {reason}"
    );
    assert!(reason.contains("never read_file"), "reason: {reason}");
    assert_eq!(
        reason,
        "2 read(s) in a row that wrote nothing to the workspace. Write what you learned in \
         this patch (a file note, a plan step, `recent_summary`) or the edit you will make \
         (`next` = {\"tool\":\"edit_file\",\"path\":\"<file>\",\"what\":\"<one line>\"}; \
         `next.tool` is never read_file), then read on. To find the exact spot, `search` \
         (pattern + path) beats paging",
        "exact rejection text: {reason}"
    );

    // Retry: setting `next` alongside the read on c.rs is accepted.
    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "the retry that sets `next` should be accepted, got {outcome3:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejections_after_retry = sys_entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .count();
    assert_eq!(
        rejections_after_retry, 1,
        "only the third bare read should have been rejected: {sys_entries:?}"
    );
}

/// A retry that carries any patch key (here `recent_summary`) with the same
/// read is accepted -- the rejection said "write what you learned", and
/// writing it, even alongside the identical read, must recover.
#[tokio::test]
async fn retry_with_any_patch_key_recovers_from_silent_read_rejection() {
    let root = tempfile::tempdir().unwrap();
    for f in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(root.path().join(f), "fn x() {}\n").unwrap();
    }

    let turns = vec![
        coding_bare_read_call("a.rs"),
        coding_bare_read_call("b.rs"),
        // Step 3: a third bare read of c.rs -- rejected.
        coding_bare_read_call("c.rs"),
        // Step 3 (retry): the exact same read, but the patch now carries
        // `recent_summary` -- not empty, so it is accepted.
        coding_read_call("c.rs", json!({"recent_summary": "c.rs looks unrelated"})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let _ = log.append_record(rec).unwrap();
    }

    let rec_retry = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome_retry = log.append_record(rec_retry).unwrap();
    assert!(
        matches!(outcome_retry, Outcome::Continue),
        "a retry that writes `recent_summary` should be accepted, got {outcome_retry:?}"
    );

    let (state, _) = active.session().state().await.unwrap();
    assert_eq!(state.agent.recent_summary, "c.rs looks unrelated");

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let rejections = sys_entries
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .count();
    assert_eq!(
        rejections, 1,
        "only the bare third read should be rejected: {sys_entries:?}"
    );
}

/// Three reads in a row, each carrying a `files` note, are all accepted:
/// a read whose patch writes something is never counted, so this is
/// unbounded even though the cap is 2.
#[tokio::test]
async fn three_reads_each_with_a_file_note_are_all_accepted() {
    let root = tempfile::tempdir().unwrap();
    for f in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(root.path().join(f), "fn x() {}\n").unwrap();
    }

    let note_read = |path: &str, note: &str| {
        coding_read_call(path, json!({"files": [{"path": path, "note": note}]}))
    };

    let turns = vec![
        note_read("a.rs", "looks unrelated"),
        note_read("b.rs", "looks unrelated"),
        note_read("c.rs", "looks unrelated"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 1..=3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "read #{i} (with a file note) should be accepted, got {outcome:?}"
        );
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "a read that writes a file note each time must never be rejected, no matter how many \
         in a row: {sys_entries:?}"
    );
}

/// A bare read after a read-with-patch starts the streak over: read+patch,
/// bare, bare are all accepted (the streak only reaches 2 on the second bare
/// read); the third bare read in a row is rejected.
#[tokio::test]
async fn bare_read_after_read_with_patch_starts_the_streak_over() {
    let root = tempfile::tempdir().unwrap();
    for f in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        std::fs::write(root.path().join(f), "fn x() {}\n").unwrap();
    }

    let turns = vec![
        // Step 1: read a.rs with a file note -- writes something, streak stays 0.
        coding_read_call("a.rs", json!({"files": [{"path": "a.rs", "note": "n/a"}]})),
        // Step 2: bare read of b.rs -- streak becomes 1, accepted.
        coding_bare_read_call("b.rs"),
        // Step 3: bare read of c.rs -- streak becomes 2, accepted (cap is 2).
        coding_bare_read_call("c.rs"),
        // Step 4: bare read of d.rs -- streak would become 3, rejected.
        coding_bare_read_call("d.rs"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 1..=3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i} should be accepted, got {outcome:?}"
        );
    }

    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    assert!(matches!(outcome4, Outcome::Continue), "got {outcome4:?}");

    let reason = last_rejection_reason(&log);
    assert!(
        reason.contains("2 read(s) in a row that wrote nothing to the workspace"),
        "reason: {reason}"
    );
}

#[tokio::test]
async fn mutating_call_resets_the_silent_read_streak() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        // Step 1: read a.rs, no commitment -- allowed (read #1).
        coding_bare_read_call("a.rs"),
        // Step 2: edit a.rs -- mutating, resets the streak.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "a.rs", "old": "fn old() {}", "new": "fn new() {}"},
                },
            })
            .to_string(),
        ),
        // Step 3: read a.rs again, no commitment -- streak was reset, so
        // this is read #1 again, not #2 -- allowed.
        coding_bare_read_call("a.rs"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "a mutating call in between should reset the silent-read streak: {sys_entries:?}"
    );
}

#[tokio::test]
async fn plan_skill_allows_unbounded_silent_reads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(root.path().join("c.rs"), "fn c() {}\n").unwrap();

    let turns = vec![
        coding_bare_read_call("a.rs"),
        coding_bare_read_call("b.rs"),
        coding_bare_read_call("c.rs"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "plan's silent_read_cap is None, so three consecutive bare reads must not be rejected: \
         {sys_entries:?}"
    );
}

#[tokio::test]
async fn recall_with_empty_patch_counts_for_the_silent_read_guard() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();

    let turns = vec![
        coding_bare_read_call("a.rs"),
        coding_bare_read_call("b.rs"),
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "recall",
                    "args": {"query": "test output"},
                },
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    let reason = last_rejection_reason(&log);
    assert!(
        reason.contains("2 read(s) in a row that wrote nothing to the workspace"),
        "`recall` with an empty patch should count toward the silent-read streak the same as \
         `read_file`: {reason}"
    );
}

/// A bare read with `next` set is governed by the commitment-slot rule
/// (`session.rs` guard 7.6, `COMMITMENT_READ_CAP`), not the silent-read
/// guard: while a commitment is set, reads of the committed path are capped
/// by that separate rule, so no "wrote nothing" rejection should ever fire
/// here even though every read below has an empty patch.
#[tokio::test]
async fn bare_read_with_commitment_set_is_governed_by_commitment_rule_not_silent_read_guard() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        // Step 1: set `next` and read a.rs in the same step (uses the
        // commitment-slot's own read allowance, not the silent-read guard).
        set_commitment_and_read_turn("a.rs", "replace `old` with `new`"),
        // Step 2: a second bare read of a.rs -- still within the
        // commitment-slot's 2-read cap.
        coding_bare_read_call("a.rs"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 1..=2 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i} should be accepted, got {outcome:?}"
        );
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "with a commitment set, the silent-read guard ('wrote nothing') must never fire -- \
         only the commitment-slot rule governs these reads: {sys_entries:?}"
    );

    let no_wrote_nothing = !log.entries(Some(LogKind::System), 10_000).iter().any(|e| {
        e.payload
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|r| r.contains("wrote nothing to the workspace"))
    });
    assert!(
        no_wrote_nothing,
        "no rejection should ever mention 'wrote nothing to the workspace' while a commitment \
         is set"
    );
}

// ---------------------------------------------------------------------
// Redundant-identical-read guard (7.8, `session.rs`,
// `Session::reads_since_mutation` / `read_call_key`): an exact repeat of a
// read-only tool call (same tool, same canonical args) with nothing mutated
// since gives the same output already in the log -- the step still commits
// (patch + action + acks), but the call itself is skipped rather than
// re-run. Runs for every skill, before the commitment-slot rule interferes
// with a mismatched rejection reason. `filter_last_tool_call` is exempt (it
// re-reads a previous output by design).
// ---------------------------------------------------------------------

/// Sets `next` to an `edit_file` commitment on `path` and `recent_summary`
/// to `summary`, reading `path` in the same step -- like
/// `set_commitment_and_read_turn`, but also touches `recent_summary` so a
/// test can prove the patch committed independent of the identical `next`
/// value.
fn set_commitment_and_read_turn_with_summary(path: &str, what: &str, summary: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {
                "next": {"tool": "edit_file", "path": path, "what": what},
                "recent_summary": summary,
            },
            "action": {
                "type": "tool",
                "name": "read_file",
                "args": {"path": path, "offset": 1, "limit": 5},
            },
        })
        .to_string(),
    )
}

#[tokio::test]
async fn identical_read_of_committed_path_commits_the_patch_and_skips_the_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();

    let turns = vec![
        // Step 1: commit to editing a.rs, reading it in the same step
        // (read #1 of the commitment-slot's 2-read cap) -- accepted.
        set_commitment_and_read_turn("a.rs", "replace `a` with `b` in fn signature"),
        // Step 2: the exact same read again -- still within the
        // commitment-slot's 2-read cap, but caught by the (separate,
        // narrower-triggering) redundant-identical-read guard instead: the
        // patch (recent_summary) commits, the read itself is skipped.
        set_commitment_and_read_turn_with_summary(
            "a.rs",
            "replace `a` with `b` in fn signature",
            "re-read a.rs, nothing new",
        ),
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Continue),
        "first read should be accepted, got {outcome1:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !rec2.entries().iter().any(|e| e.kind == LogKind::ToolOutput),
        "the skipped read must not produce a tool_output entry: {:?}",
        rec2.entries()
    );
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the redundant repeat still commits as a normal step, got {outcome2:?}"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.recent_summary, "re-read a.rs, nothing new");

    let skipped = last_skipped_redundant(&log);
    let reason = skipped.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(
        reason.contains("you already ran `read_file"),
        "reason: {reason}"
    );
    assert!(reason.contains("at step"), "reason: {reason}");
    assert!(
        reason.contains("nothing has changed since"),
        "reason: {reason}"
    );
    assert_eq!(
        skipped.get("name").and_then(Value::as_str),
        Some("read_file")
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    let obs = &prompts[2].user;
    assert!(obs.contains("you already ran `read_file"), "{obs}");
}

#[tokio::test]
async fn read_with_different_args_is_not_treated_as_redundant() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();

    let turns = vec![
        // Step 1: commit to editing a.rs, reading it (offset/limit from
        // `set_commitment_and_read_turn`: {"offset": 1, "limit": 5}).
        set_commitment_and_read_turn("a.rs", "replace `a` with `b` in fn signature"),
        // Step 2: a read of the same path but with a different `offset` --
        // a different canonical key, so not redundant; still within the
        // commitment-slot's 2-read cap.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "read_file",
                    "args": {"path": "a.rs", "offset": 2, "limit": 5},
                },
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "a read with different args must not be treated as redundant, got {outcome2:?}"
    );

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!any_rejected, "{sys_entries:?}");
}

#[tokio::test]
async fn mutating_call_clears_the_redundant_read_guard() {
    // `ImplementSkill`, not `PlanSkill`: the guard-clearing behavior under
    // test is specifically "a mutating call resets the map", which needs a
    // mutating tool (`edit_file`) in the active tool set -- `PlanSkill` has
    // no mutating tools at all. `ImplementSkill`'s own
    // `silent_read_cap() == Some(2)` does not interfere here: each read
    // below is either the first silent read (allowed) or immediately
    // follows a mutating call that reset that streak too.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();

    let turns = vec![
        // Step 1: read a.rs, no commitment -- allowed.
        coding_bare_read_call("a.rs"),
        // Step 2: edit a.rs -- mutating, clears the redundant-read map (and
        // resets the silent-read streak).
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "edit_file",
                    "args": {"path": "a.rs", "old": "fn old() {}", "new": "fn new() {}"},
                },
            })
            .to_string(),
        ),
        // Step 3: the exact same read as step 1 -- accepted, because the
        // edit in step 2 cleared the map.
        coding_bare_read_call("a.rs"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "the edit_file call in between should clear the redundant-read map: {sys_entries:?}"
    );
}

#[tokio::test]
async fn identical_search_commits_the_patch_and_skips_the_call() {
    // The guard covers every read-only tool, not just `read_file`. `search`
    // needs no commitment and `PlanSkill` has no `silent_read_cap`, so this
    // isolates the redundant-identical-read guard from every other
    // read-related rule.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();

    let search_turn = |summary: &str| {
        MockTurn::new(
            json!({
                "patch": {"recent_summary": summary},
                "action": {
                    "type": "tool",
                    "name": "search",
                    "args": {"pattern": "fn a"},
                },
            })
            .to_string(),
        )
    };
    let turns = vec![
        search_turn("first search"),
        search_turn("searched again, found nothing new"),
        coding_ask_user_turn(),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    assert!(
        !rec2.entries().iter().any(|e| e.kind == LogKind::ToolOutput),
        "the skipped search must not produce a tool_output entry: {:?}",
        rec2.entries()
    );
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the redundant repeat still commits as a normal step, got {outcome2:?}"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.recent_summary,
        "searched again, found nothing new"
    );

    let skipped = last_skipped_redundant(&log);
    let reason = skipped.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(
        reason.contains("you already ran `search"),
        "the guard must cover every read-only tool, not just read_file: {reason}"
    );
    assert!(reason.contains("at step"), "reason: {reason}");
    assert!(
        reason.contains("nothing has changed since"),
        "reason: {reason}"
    );
    assert_eq!(skipped.get("name").and_then(Value::as_str), Some("search"));

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec3).unwrap();
    let prompts = model.prompts();
    let obs = &prompts[2].user;
    assert!(obs.contains("you already ran `search"), "{obs}");
}

#[tokio::test]
async fn identical_filter_last_tool_call_is_exempt_from_the_redundant_read_guard() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();

    let filter_turn = || {
        MockTurn::new(
            json!({
                "patch": {},
                "action": {
                    "type": "tool",
                    "name": "filter_last_tool_call",
                    "args": {"pattern": "fn"},
                },
            })
            .to_string(),
        )
    };
    let turns = vec![coding_bare_read_call("a.rs"), filter_turn(), filter_turn()];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for _ in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(matches!(outcome, Outcome::Continue), "got {outcome:?}");
    }

    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    let any_rejected = sys_entries
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(
        !any_rejected,
        "filter_last_tool_call re-reads a previous output by design and must be exempt from \
         the redundant-identical-read guard: {sys_entries:?}"
    );
}

// ---------------------------------------------------------------------
// Cross-cutting behaviour of the skip (rather than reject) treatment: the
// commitment slot, the loop guard, and context maintenance must all see a
// skipped step the same way they would a normal committed step, except that
// a skipped mutating call never counts as fulfilling `next` (it never ran).
// ---------------------------------------------------------------------

/// Sets `next` to a `shell` commitment (no path) via `ask_user`.
fn set_shell_commitment_via_ask_user_turn(what: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {
                "next": {"tool": "shell", "path": null, "what": what},
            },
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )
}

/// A skipped call that matches the commitment now DISCHARGES it
/// (statie-spec.md "10. After it works: the rule audit", run 21 steps
/// 19-27): 7.7/7.8 only skip a call whose result is already known, so if
/// that call is the one the model committed to, there is nothing left to
/// wait for. Leaving `next` set here deadlocked run 21 -- every following
/// call was rejected with "you committed to ..." because the one call that
/// could fulfil it was never going to run again. This was previously named
/// `a_skipped_matching_call_does_not_fulfil_the_commitment` and asserted the
/// opposite (pre-fix) behaviour.
#[tokio::test]
async fn a_skipped_matching_call_discharges_the_commitment() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: commit to running `shell`.
        set_shell_commitment_via_ask_user_turn("run the tests"),
        // Step 2: the first `shell echo hi` -- matches the commitment,
        // actually runs, fulfils it; the harness auto-clears `next`.
        coding_shell_call("echo hi"),
        // Step 3: re-commit to `shell` again (a fresh commitment).
        set_shell_commitment_via_ask_user_turn("run the tests again"),
        // Step 4: the exact same `shell echo hi` -- nothing mutated since
        // step 2, so 7.7 skips the call. It matches the step-3 commitment,
        // so it must discharge it (a different prefix from a real
        // fulfilment, but the same `next`-clearing effect).
        coding_shell_call("echo hi"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 0..4 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "step {i}: got {outcome:?}"
        );
    }

    // The step-4 skip must have discharged the step-3 commitment: `next` is
    // cleared, the same as a real fulfilment.
    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        state.agent.next.is_none(),
        "a skipped call that matches the commitment must discharge it (`next` must be null)"
    );

    // A harness-authored clear patch was committed after step 4, the same
    // as a real fulfilment.
    let patch_entries = log.entries(Some(LogKind::Patch), 10_000);
    let last_patch = patch_entries
        .last()
        .expect("expected at least one patch entry");
    assert_eq!(
        last_patch.payload.get("source").and_then(Value::as_str),
        Some("harness"),
        "expected a harness-authored clear patch to follow the discharging skip: \
         {patch_entries:?}"
    );

    // The observation for step 4 must carry the "commitment discharged"
    // prefix, not the "commitment fulfilled" prefix a real match would get
    // (the call's own result never re-ran this step).
    let skipped_entries = log.entries(Some(LogKind::System), 10_000);
    assert!(
        skipped_entries
            .iter()
            .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant")),
        "expected the second `shell echo hi` to be skipped: {skipped_entries:?}"
    );
}

#[tokio::test]
async fn a_third_identical_shell_attempt_still_yields_the_loop_guard_note() {
    // The loop guard keys on (name, args, observation text); a real run's
    // observation ("hi") differs from a skipped repeat's (the guard reason
    // plus the cached result), so the first `shell echo hi` establishes the
    // baseline and the *skips* are what must recur three times -- i.e. this
    // needs a fourth attempt overall (one real run, three skips) before the
    // occurrence counter (keyed on the skip text) reaches 3.
    //
    // Fix 2 (`core/src/session.rs`, `CONTINUE_REPEAT_CAP` shared with the
    // repeated-skip guard) also caps at three CONSECUTIVE identical skips
    // (same tool, same call key): the third skip here (the fourth attempt
    // overall) is both the loop guard's third occurrence AND the repeated-
    // skip guard's third consecutive skip, so this step now parks
    // (`Park::AskUser`) instead of returning `Outcome::Continue` -- the
    // fifth scripted turn (`coding_ask_user_turn`) is never reached.
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        coding_shell_call("echo hi"),
        coding_shell_call("echo hi"),
        coding_shell_call("echo hi"),
        coding_shell_call("echo hi"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    for i in 0..3 {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(outcome, Outcome::Continue),
            "step {i}: got {outcome:?}"
        );
    }

    // The fourth attempt (the third consecutive identical skip) parks
    // instead of continuing, carrying both the loop guard's "occurrence #3"
    // note and the repeated-skip guard's own message.
    let rec4 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome4 = log.append_record(rec4).unwrap();
    let Outcome::Parked(Park::AskUser(message)) = outcome4 else {
        panic!("got {outcome4:?}; the third consecutive identical skip must park as a question");
    };
    assert!(
        message.contains("[harness] This is occurrence #3"),
        "a third identical skipped attempt must still trip the loop guard: {message}"
    );
    assert!(
        message.contains("repeated the same skipped call 3 times in a row"),
        "the repeated-skip guard's own message must also be present: {message}"
    );
    assert!(
        message.contains("shell") && message.contains("echo hi"),
        "the repeated-skip guard's message must name the tool and command: {message}"
    );

    // The second, third, and fourth attempts were all skipped (never re-run).
    let skipped_count = log
        .entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant"))
        .count();
    assert_eq!(skipped_count, 3, "expected three skipped repeats");
}

/// Unit 46 (D29): a `skipped_redundant` step (7.7/7.8's "do NOT run the
/// tool" path -- the observation carries nothing new) is no longer treated
/// specially -- unit 43's skip rule that withheld the spawn for exactly
/// this shape (logging a now-deleted skip event with reason `"skipped_read"`
/// instead) is gone, since build 27 showed it starving the notes during
/// gemma's refusal walks. The maintainer now runs for the skipped step too,
/// the same as it did before item 43 (hence this test's restored,
/// pre-item-43 name): the first turn's own call (spawned after it commits
/// for real) is drained at the start of the skipped turn's own `step()`
/// call, and the skipped turn spawns its own call in turn. A third,
/// unrelated turn forces that second call's own drain (the same
/// one-step-stale pattern `maintainer_called_once_after_committed_tool_
/// step_with_expected_inputs` in `core/tests/it/maintainer.rs` uses) --
/// two calls total by then, as before item 43.
#[tokio::test]
async fn the_maintainer_still_runs_after_a_skipped_step() {
    let root = tempfile::tempdir().unwrap();

    let maintainer = Arc::new(statie_core::MockMaintainer::new(vec![
        Ok(statie_core::MaintainOutput {
            notes: "ran the tests once".to_string(),
            next: None,
        }),
        Ok(statie_core::MaintainOutput {
            notes: "ran the tests once, again".to_string(),
            next: None,
        }),
    ]));

    let turns = vec![
        coding_shell_call("echo hi"),
        coding_shell_call("echo hi"),
        coding_shell_call("echo bye"),
    ];
    let model = Arc::new(MockModel::new(turns));
    let cfg = common::config(&root);
    let (session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);
    let mut session =
        session.with_maintainer(Arc::clone(&maintainer) as Arc<dyn statie_core::Maintainer>);
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the skipped step still commits, got {outcome2:?}"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Continue),
        "the third, unrelated turn still commits, got {outcome3:?}"
    );

    let calls = maintainer.calls();
    assert_eq!(
        calls.len(),
        2,
        "the maintainer must be called for both the first (real) turn, drained during the \
         skipped turn's own step(), and for the skipped turn itself, drained during the \
         third turn's own step() -- as before item 43"
    );
}
