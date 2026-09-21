//! Item 28: the router cannot ask the user anything. Item 55: the router's
//! bare `done` is routed by state instead of being rejected.
//!
//! `RouterSkill::allows_ask_user` overrides the `Skill` trait default (`true`)
//! to `false`, next to its existing `allows_yield() == false` override
//! (`skills/src/coding/mod.rs`). With the router active, a scripted model
//! output whose action is `ask_user` must not be accepted as a park: it is
//! rejected by `core::skill::validate_default`'s `Action::AskUser` check
//! (`!skill.ask_user_allowed()`), which `core/src/session.rs`'s `step`
//! reaches through `self.skill.validate(&cur, parsed)` -- the same rule
//! `allows_ask_user()` feeds when `Session::activate_inner`
//! (`core/src/session.rs`) narrows the advertised `action.type` schema enum.
//! The exact rejection text is quoted from `core/src/skill.rs`:
//! `"action \`ask_user\` is not allowed in skill \`{}\`"` with `{}` filled
//! in by `skill.skill_name()` (`"router"` here).
//!
//! `router_bare_done_with_empty_plan_routes_to_plan_no_rejection` below
//! covers item 55: `router_bare_done_routes` (`skills/src/coding/mod.rs`)
//! routes a bare `done` (no `next_skill`) by state instead of letting
//! `validate_default`'s handoff rule reject it.
//!
//! Fixture style copied from `skills/tests/it/bare_done.rs`/`child.rs`: a
//! `Session<CodingWorkspace>` wired with an empty `ToolSet` (router has no
//! tools, `RouterSkill::tools() == vec![]`), a scripted `MockModel`, and
//! outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `bare_done.rs`/
//! `no_green.rs` inline their own `config`/`make_router_session`:
//! `skills/tests/common/mod.rs` is hardcoded to `ToyWorkspace` and exports
//! items this file has no use for, and `-D warnings` fails a binary that
//! leaves any of a shared module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, StepSink,
    ToolSet,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{
    CodingWorkspace, ImplementSkill, PlanSkill, ROUTER_ROUTED_NOTE_PREFIX, RouterSkill,
};
use statie_store::InMemoryStore;

/// The exact rejection text `validate_default` (`core/src/skill.rs`)
/// produces for a forbidden `ask_user` action, with `{}` filled by the
/// skill's own name, as it reaches the log after `PatchError::RuleViolation`'s
/// `Display` impl prepends "skill rule violated: " and `Session::reject`
/// (`core/src/session.rs`) calls `e.to_string()` on it.
const ASK_USER_NOT_ALLOWED_NOTE: &str =
    "skill rule violated: action `ask_user` is not allowed in skill `router`";

struct ArcModel(Arc<MockModel>);

#[async_trait]
impl statie_core::Model for ArcModel {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn step(
        &self,
        prompt: statie_core::BoundedPrompt,
        schema: &Schema,
        sink: &mut dyn StepSink,
        cancel: CancellationToken,
    ) -> Result<RawOutput, statie_core::ModelError> {
        self.0.step(prompt, schema, sink, cancel).await
    }
}

fn session_config(root: &tempfile::TempDir) -> SessionConfig {
    SessionConfig {
        budgets: Budgets {
            total: 8000,
            observation: 500,
            augmentation: 500,
            notes: 400,
            working_set: 6000,
        },
        retries: 3,
        root: SandboxRoot::new(root.path()).unwrap(),
    }
}

fn make_router_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        ToolSet::new(),
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// A turn that sends action `ask_user` with the given message and no patch.
fn ask_user_turn(message: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "ask_user", "message": message},
        })
        .to_string(),
    )
}

/// With the router active, a model output whose action is `ask_user` with
/// message "Which file?" is rejected outright -- never parked as
/// `Park::AskUser`. Asserts the exact outcome (`Outcome::Continue`, not
/// `Outcome::Parked(Park::AskUser(_))`) and the exact rejection note core
/// produces (quoted from `core/src/skill.rs`'s `validate_default`, not
/// paraphrased).
#[tokio::test]
async fn router_ask_user_is_rejected_not_parked() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![ask_user_turn("Which file?")];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_router_session(Arc::clone(&model), session_config(&root));

    let skill: Arc<RouterSkill> = Arc::new(RouterSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entries = rec.entries().to_vec();
    let outcome = log.append_record(rec).unwrap();

    // Rejected, not parked: the loop-continue outcome, never `Parked(AskUser(_))`.
    assert!(matches!(outcome, Outcome::Continue), "{outcome:?}");
    assert!(
        !matches!(outcome, Outcome::Parked(Park::AskUser(_))),
        "ask_user must not be allowed to park the router"
    );

    let rejected = entries
        .iter()
        .find(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(Value::as_str) == Some("rejected")
        })
        .expect("expected a rejected system log entry");
    let reason = rejected
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        reason, ASK_USER_NOT_ALLOWED_NOTE,
        "rejection reason must match core::skill::validate_default's exact text"
    );
}

/// The `text` of every System entry with `event: "note"`. Copied from
/// `skills/tests/it/plan_ask_handoff.rs`'s helper of the same name.
fn note_texts(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("note"))
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Item 55: a router `done` with no `next_skill` at all -- the live shape,
/// `{"patch": {"task": "..."}, "action": {"type": "done"}}` -- used to be
/// rejected outright by `validate_default`'s handoff rule (the previous
/// version of this test asserted that exact rejection text). It is now
/// routed by state instead: `router_bare_done_routes`
/// (`skills/src/coding/mod.rs`) sees an empty plan and hands off to `plan`,
/// committing as `Outcome::Handoff("plan")` with the exact note in the log
/// and NO `rejected` entry anywhere. A second activation, on the same
/// session/store, lands in `plan` -- proving the handoff this step named is
/// the one the harness actually follows.
#[tokio::test]
async fn router_bare_done_with_empty_plan_routes_to_plan_no_rejection() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({"patch": {"task": "fix the bug"}, "action": {"type": "done"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_router_session(Arc::clone(&model), session_config(&root));

    let skill: Arc<RouterSkill> = Arc::new(RouterSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();
    assert!(
        matches!(outcome, Outcome::Handoff(ref t) if t == "plan"),
        "got {outcome:?}; a bare done with an empty plan must hand off to plan"
    );

    let rejected_any = log
        .entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"));
    assert!(!rejected_any, "the routed step must not have been rejected");

    let expected_note = format!("{ROUTER_ROUTED_NOTE_PREFIX}`plan`: the plan is empty.");
    let notes = note_texts(&log);
    assert!(
        notes.iter().any(|n| n == &expected_note),
        "expected the exact routing note in the log: {notes:?}"
    );

    // The second activation lands in `plan`, consuming the handoff -- the
    // same "next activation follows the named handoff" check
    // `bare_done.rs`'s `plan_handoff_clears_next_skill_so_implement_bare_
    // done_still_hands_off` uses.
    let plan_skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut plan_active = session.activate(Arc::clone(&plan_skill)).await.unwrap();
    let (state, _version) = plan_active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must be cleared right after plan's activation commits"
    );
}

/// `allows_ask_user()` is `false` for `RouterSkill` and `true` for
/// `PlanSkill`/`ImplementSkill` (the default the `Skill` trait provides,
/// left un-overridden by either).
#[test]
fn allows_ask_user_is_false_only_for_router() {
    assert!(!RouterSkill::new().allows_ask_user());
    assert!(PlanSkill::new().allows_ask_user());
    assert!(ImplementSkill::new().allows_ask_user());
}
