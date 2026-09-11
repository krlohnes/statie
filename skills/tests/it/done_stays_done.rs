//! `guards.done_stays_done` on/off pair for `plan_rules` (statie-spec.md
//! "10. After it works: the rule audit", milestone 12): on, a plan step
//! committed `done` may not regress back to `pending`/`in_progress`
//! (`skills/src/coding/mod.rs`'s `plan_rules`); off, the exact same
//! regression is accepted and the step actually reopens in the committed
//! workspace.
//!
//! The two tests below drive the mechanism directly via
//! `ImplementSkill::with_done_stays_done(true/false)`, independent of any
//! config-level default. The individual skill constructors
//! (`ImplementSkill::new()`/`Default`) still default their OWN
//! `done_stays_done` field to `true` -- it is only the config-facing
//! defaults (`Guards::default()`, `coding_skills()`/
//! `coding_skills_with_delegates()`, `CodingRegistry::with_guards` with no
//! configured `guards` section) that now default to `false`, since the
//! first bench battery's audit showed the switch off no worse: the rule
//! rejected the model twelve times over for moving its "run cargo test"
//! plan step back to in-progress after an edit, until the retry cap parked
//! it as a failure with the crate already green. See
//! `coding_skills_default_is_done_stays_done_off` below for that
//! config-facing default.
//!
//! Mirrors the `plan_rules` tests in `commitment_rules.rs`: a
//! `Session<CodingWorkspace>` wired with the real file tools, a scripted
//! `MockModel`, and rejection reasons read from the `System` log's
//! `rejected` entries.
//!
//! Deliberately not `mod common;` here, for the same reason
//! `commitment_rules.rs` inlines its own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` also exports `EchoTool`/`tool_set`/
//! `make_session` (for `Session<ToyWorkspace>`), which this file has no use
//! for, and `-D warnings` fails a binary that leaves any of a shared
//! module's items unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, Guards, LogKind, MemoryLog, NullSink, Outcome, Park, RawOutput,
    SandboxRoot, Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, StepStatus, coding_skills};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `commitment_rules.rs`'s `config`.
fn config(root: &tempfile::TempDir) -> SessionConfig {
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

/// Same shape as `commitment_rules.rs`'s `ArcModel`.
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

/// Same shape as `commitment_rules.rs`'s `make_file_tools_session`.
fn make_file_tools_session(
    model: Arc<MockModel>,
    cfg: SessionConfig,
) -> (Session<CodingWorkspace>, Arc<dyn SessionLog>) {
    let log: Arc<dyn SessionLog> = Arc::new(MemoryLog::new());
    let tools = default_tools(&ToolConfig::default());
    let session = Session::new(
        Box::new(InMemoryStore::<CodingWorkspace>::new()),
        Arc::clone(&log),
        Arc::new(ArcModel(model)),
        tools,
        Box::new(statie_core::HeuristicTokenizer),
        cfg,
    );
    (session, log)
}

/// Same shape as `commitment_rules.rs`'s `last_rejection_reason`.
fn last_rejection_reason(log: &Arc<dyn SessionLog>) -> String {
    let sys_entries = log.entries(Some(LogKind::System), 10_000);
    sys_entries
        .iter()
        .rev()
        .find(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
        .and_then(|e| e.payload.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Same shape as `commitment_rules.rs`'s `set_plan_turn`.
fn set_plan_turn(plan: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"plan": plan},
            "action": {"type": "ask_user", "message": "confirm?"},
        })
        .to_string(),
    )
}

/// Runs `turns` against a fresh `ImplementSkill` session (real file tools,
/// sandboxed to a fresh tempdir), one `step()` call per turn, with
/// `guards.done_stays_done` wired via `ImplementSkill::with_done_stays_done`.
/// Returns whether the *last* turn was rejected, the most recent rejection
/// reason (meaningful only when that flag is true), and the committed
/// workspace's plan after the last turn (so a test can assert the step
/// actually reopened, not merely that nothing rejected).
async fn run_turns(
    turns: Vec<MockTurn>,
    done_stays_done: bool,
) -> (bool, String, Vec<(u32, StepStatus)>) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(turns.clone()));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> =
        Arc::new(ImplementSkill::new().with_done_stays_done(done_stays_done));
    let mut active = session.activate(skill).await.unwrap();

    let mut rejected = false;
    for _ in &turns {
        let before = last_rejection_reason(&log);
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "got {outcome:?}"
        );
        let after = last_rejection_reason(&log);
        // A fresh rejection this turn changed the most-recent reason (or
        // set one where there was none); good enough since these scripts
        // never repeat an identical rejection reason back to back.
        rejected = after != before && !after.is_empty();
    }

    let (state, _version) = active.session().state().await.unwrap();
    let plan = state.agent.plan.iter().map(|s| (s.id, s.status)).collect();

    (rejected, last_rejection_reason(&log), plan)
}

// ---------------------------------------------------------------------
// On (default): reopening a done step is rejected. Same scenario as
// `commitment_rules.rs`'s
// `done_step_moved_to_in_progress_is_rejected_and_points_at_next_open_step`,
// reproduced here (rather than imported) so the on/off pair sits together
// and the off case below is an exact, side-by-side mirror.
// ---------------------------------------------------------------------

#[tokio::test]
async fn guard_on_rejects_reopening_a_done_step() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "in_progress", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];

    let (rejected, reason, plan) = run_turns(turns, true).await;
    assert!(rejected, "second turn should have been rejected");
    assert!(
        reason.contains("plan step 1 is done and stays done (first)"),
        "rejection reason should name the offending step: {reason}"
    );
    // The committed workspace must still show step 1 as `done` -- the
    // rejected patch never landed.
    assert_eq!(
        plan.iter().find(|(id, _)| *id == 1).map(|(_, s)| *s),
        Some(StepStatus::Done),
        "step 1 must still be done in the committed workspace: {plan:?}"
    );
}

// ---------------------------------------------------------------------
// Off: the exact same regression is accepted, and the step actually
// reopens in the committed workspace.
// ---------------------------------------------------------------------

#[tokio::test]
async fn guard_off_allows_reopening_a_done_step() {
    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "in_progress", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];

    let (rejected, _reason, plan) = run_turns(turns, false).await;
    assert!(
        !rejected,
        "the reopen must not be rejected when the guard is off"
    );
    assert_eq!(
        plan.iter().find(|(id, _)| *id == 1).map(|(_, s)| *s),
        Some(StepStatus::InProgress),
        "step 1 must actually have reopened to in_progress in the committed workspace: {plan:?}"
    );
}

// ---------------------------------------------------------------------
// Config-facing default: `Guards::default().done_stays_done` is `false`,
// and `coding_skills()` (which threads that same default through
// `coding_skills_configured`) accepts the reopen out of the box, with no
// explicit `with_done_stays_done` call at all -- this is what a fresh CLI
// config (no `guards` section) actually runs with as of the first bench
// battery's audit.
// ---------------------------------------------------------------------

#[test]
fn guards_default_has_done_stays_done_off() {
    assert!(
        !Guards::default().done_stays_done,
        "done_stays_done must default to false, unlike every other guard"
    );
}

#[tokio::test]
async fn coding_skills_default_accepts_reopening_a_done_step() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        set_plan_turn(json!([
            {"id": 1, "status": "done", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
        set_plan_turn(json!([
            {"id": 1, "status": "in_progress", "text": "first"},
            {"id": 2, "status": "pending", "text": "second"},
        ])),
    ];
    let model = Arc::new(MockModel::new(turns.clone()));
    let cfg = config(&root);
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), cfg);

    let implement = coding_skills()
        .into_iter()
        .find(|(name, _)| name == "implement")
        .map(|(_, skill)| skill)
        .expect("coding_skills() must include implement");
    let mut active = session.activate(implement).await.unwrap();

    for _ in &turns {
        let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
        let outcome = log.append_record(rec).unwrap();
        assert!(
            matches!(
                outcome,
                Outcome::Continue | Outcome::Parked(Park::AskUser(_))
            ),
            "got {outcome:?}"
        );
    }

    assert_eq!(
        last_rejection_reason(&log),
        "",
        "coding_skills()'s default must accept the reopen with no rejection at all"
    );

    let (state, _version) = active.session().state().await.unwrap();
    let step1_status = state
        .agent
        .plan
        .iter()
        .find(|s| s.id == 1)
        .map(|s| s.status);
    assert_eq!(
        step1_status,
        Some(StepStatus::InProgress),
        "step 1 must actually have reopened under coding_skills()'s default"
    );
}
