//! Item 28: the router cannot ask the user anything.
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
use statie_skills::{CodingWorkspace, ImplementSkill, PlanSkill, RouterSkill};
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

/// A router `done` with no `next_skill` is rejected with the hand-off note,
/// and that note offers neither `yield` nor `ask_user`: the router allows
/// neither, so the pause clause is empty (`core/src/skill.rs`,
/// `validate_default`, item 28). Exact text, so a change to either flag or
/// to the note's wording breaks a visible assertion.
#[tokio::test]
async fn router_done_without_next_skill_note_offers_no_pause_action() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![MockTurn::new(
        json!({"patch": {}, "action": {"type": "done"}}).to_string(),
    )];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_router_session(Arc::clone(&model), session_config(&root));

    let skill: Arc<RouterSkill> = Arc::new(RouterSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let entries = rec.entries().to_vec();
    let outcome = log.append_record(rec).unwrap();
    assert!(matches!(outcome, Outcome::Continue), "{outcome:?}");

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
        reason,
        "skill rule violated: action `done` is only valid when the workspace is terminal or \
         hands off: set `next_skill` to one of [\"plan\", \"implement\", \"review\"] (you are \
         `router`) and send `done` again, or keep working with a tool\n\
         Exact shape: {\"patch\": {\"next_skill\": \"plan\"}, \"action\": {\"type\": \"done\", \
         \"name\": \"\", \"args\": {}}, \"ack_inbox\": []}"
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
