//! `implement`'s `done: true`-stripping fix (`skills/src/coding/mod.rs`,
//! `implement_done_not_ours_strip`): the plan/router fix
//! (`plan_done.rs`/`plan_done_not_ours_note`) closed the hole where `done:
//! true` in a `plan`/`router` patch committed and ended the session. The
//! report that followed it flagged that `ImplementSkill::is_terminal` still
//! delegated to `common_is_terminal` (`ws.done`), so a raw `{"done": true}`
//! in an `implement` patch could still commit and end the session without
//! ever reaching review -- only review's approval (empty `findings`) may
//! finish the task (statie-spec.md §5/§10).
//!
//! The fix has two independent parts, both exercised here:
//! - `ImplementSkill::is_terminal` is now unconditionally `false` --
//!   `implement` can never end the session, whatever `ws.done` says.
//! - `ImplementSkill::validate` now strips `done` from the raw patch
//!   whenever the *merged* workspace would read `done: true`, BEFORE the
//!   rest of `implement`'s existing `done`-handling chain
//!   (`green_done_note`/`implement_bare_done`/`implement_no_green_runs_test`/
//!   `premature_done_note`) runs -- unlike `plan`/`router`'s
//!   `done_not_ours_note`, this never decides the action itself; the chain
//!   decides exactly as if `done` had never been sent.
//!
//! Fixture style of `plan_done.rs`/`review_blind.rs`: a `Session<
//! CodingWorkspace>` wired with the real file tools, a scripted `MockModel`,
//! and outcomes/log entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `plan_done.rs`/
//! `review_blind.rs` inline their own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, Skill, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, ReviewSkill};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `plan_done.rs`'s `config`.
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

/// Same shape as `plan_done.rs`'s `ArcModel`.
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

/// Same shape as `plan_done.rs`'s `make_file_tools_session`.
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

/// Whether any `rejected` System entry has been logged.
fn any_rejected(log: &Arc<dyn SessionLog>) -> bool {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .any(|e| e.payload.get("event").and_then(Value::as_str) == Some("rejected"))
}

/// A turn that runs action `test` with the given shell command and an
/// empty patch.
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------
// The report's exact shape: implement, with a passing test already this
// activation (green_step set via a scripted `test` call) and a fully-done
// plan, sends `{"patch": {"done": true}, "action": {"type": "done"}}`. The
// existing chain (`green_done_note`) hands off to review -- but only once
// `done` itself is stripped from the raw patch so it never commits.
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_true_with_passing_test_hands_off_to_review_not_terminal() {
    let root = tempfile::tempdir().unwrap();
    let cmd = "true";

    let implement_turns = vec![
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "text": "only step", "status": "in_progress"}]},
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        test_turn(cmd),
        // The defect shape: raw `done: true` in the patch, action `done`.
        MockTurn::new(
            json!({
                "patch": {"done": true, "plan": [{"id": 1, "status": "done"}]},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    // Queued on the same `MockModel`, replayed in order across the
    // `implement` -> `review` handoff below -- same pattern
    // `review_blind.rs` uses, since `Session` has no way to swap models
    // mid-session.
    let review_turns = vec![
        test_turn(cmd),
        MockTurn::new(
            json!({
                "patch": {"findings": [], "done": true},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
    ];
    let mut all_turns = implement_turns;
    all_turns.extend(review_turns);
    let model = Arc::new(MockModel::new(all_turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let implement: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&implement)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.green_step,
        // one past: the `tests` record commits its own version after every `test` call
        Some(state.harness.step - 1),
        "setup: the scripted test call must have set green_step"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();

    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; done:true with a passing test and complete plan must still hand off \
         to review, via the existing chain, once done itself is stripped"
    );
    assert!(
        !any_rejected(&log),
        "the done:true step must not have been rejected"
    );

    // The committed workspace's `done` must NOT be true -- the whole point
    // of the fix -- and `implement` itself must never be terminal, whatever
    // `ws.done` says.
    let (state, _version) = active.session().state().await.unwrap();
    assert!(
        !state.agent.done,
        "done must not have committed as true: {:?}",
        state.agent
    );
    assert!(
        !implement.is_terminal(&state.agent),
        "implement must never be terminal, even if done were somehow true"
    );

    drop(active);

    // Now hand the session to review, which approves cleanly -- confirming
    // only review can actually finish the task.
    let review: Arc<ReviewSkill> = Arc::new(ReviewSkill::new());
    let mut active = session.activate(Arc::clone(&review)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec).unwrap();
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();

    assert!(
        matches!(outcome2, Outcome::Done),
        "got {outcome2:?}; review's approval must be what finishes the task"
    );
    let (state, _version) = active.session().state().await.unwrap();
    assert!(state.agent.done, "review's approval must commit done: true");
}

// ---------------------------------------------------------------------
// The not-ours note lands on a step even when the chain substitutes a real
// `test` call instead of handing off (no passing test yet).
// ---------------------------------------------------------------------

#[tokio::test]
async fn implement_done_true_without_passing_test_runs_test_and_notes_not_ours() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {"done": true, "plan": [{"id": 1, "text": "only step", "status": "done"}]},
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // A second turn so the prompt built after the substituted call (the
        // one carrying its observation) gets recorded.
        MockTurn::new(
            json!({
                "patch": {},
                "action": {"type": "ask_user", "message": "what next?"},
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let implement: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&implement)).await.unwrap();

    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome = log.append_record(rec).unwrap();

    assert!(
        matches!(outcome, Outcome::Continue),
        "got {outcome:?}; no passing test yet, so the chain must substitute a `test` call \
         instead of handing off"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert!(!state.agent.done, "done must not have committed as true");

    // A substituted tool call carries its validation notes in the next
    // prompt's observation (never as a `continued` System entry), so take
    // one more step and read the prompt the model was shown.
    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let prompts = model.prompts();
    let last = prompts.last().expect("a prompt must have been recorded");
    assert!(
        last.user.contains("`done: true` is review's to set"),
        "the not-ours note must land in the substituted test's observation: {}",
        last.user
    );
    assert!(
        last.user.contains("ran the tests for you"),
        "the substitution note must land alongside it: {}",
        last.user
    );
}
