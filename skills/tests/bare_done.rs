//! `implement_bare_done` (`skills/src/coding/mod.rs`, wired into
//! `ImplementSkill::validate` right after `green_done_note`, before
//! `implement_no_green_runs_test`/`premature_done_note`): with thinking on,
//! eight of the bench battery's `implement` rejections were `validate_
//! default`'s core rule "action `done` is only valid when the workspace is
//! terminal or hands off" -- `implement` sent `done` with neither `done:
//! true` nor `next_skill` set, after finishing a plan that was already
//! fully done and green. Neither `green_done_note` (which only rewrites
//! when the merged plan has an OPEN step) nor `premature_done_note`'s case
//! (a) (same requirement) catches this shape, so eight rejections telling
//! the model to set `next_skill` never got it to add the field.
//! `implement_bare_done` converts it into the intended handoff instead.
//!
//! Fix 3 (a later bench battery) reordered what `implement_bare_done`
//! falls through to when it does NOT apply (`green_step: None`, or an open
//! plan step): `implement_no_green_runs_test` now runs BEFORE
//! `premature_done_note` and fires even with an open step, so the open-step
//! scenarios below now run a substituted `test` call instead of getting the
//! premature-done note -- see
//! `bare_done_with_open_step_and_no_green_runs_test_instead` and
//! `bare_done_with_open_step_and_failing_run_uses_premature_done_note`.
//!
//! Fix 4 (a later bench battery still) reproduces a live run where
//! `implement_bare_done` did not fire despite `green_step.is_some()` and no
//! open plan step -- see
//! `bare_done_with_maintainer_next_and_three_step_plan_hands_off_to_review`.
//!
//! Fixture style of `skills/tests/no_green.rs`: a `Session<CodingWorkspace>`
//! wired with the real file tools, a scripted `MockModel`, and outcomes/log
//! entries read straight from the `SessionLog`.
//!
//! Deliberately not `mod common;`, for the same reason `no_green.rs`
//! inlines its own `config`/`make_file_tools_session`:
//! `skills/tests/common/mod.rs` exports items this file has no use for, and
//! `-D warnings` fails a binary that leaves any of a shared module's items
//! unused.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use statie_core::{
    Budgets, Capabilities, Event, LogKind, MemoryLog, NullSink, Outcome, RawOutput, SandboxRoot,
    Schema, Session, SessionConfig, SessionLog, SessionLogExt, StepSink,
};
use statie_models::{MockModel, MockTurn};
use statie_skills::{CodingWorkspace, ImplementSkill, PlanSkill, StepStatus};
use statie_store::InMemoryStore;
use statie_tools::{ToolConfig, default_tools};

/// Same shape as `no_green.rs`'s `config`.
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

/// Same shape as `no_green.rs`'s `ArcModel`.
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

/// Same shape as `no_green.rs`'s `make_file_tools_session`.
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

/// Every `System` entry with `event: "continued"`, most recent last.
fn continued_notes(log: &Arc<dyn SessionLog>) -> Vec<String> {
    log.entries(Some(LogKind::System), 10_000)
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("continued"))
        .filter_map(|e| e.payload.get("note").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
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

/// A turn that sends action `done` with the given raw patch object (no
/// `next_skill`, no `done: true` -- the bare shape this file is about).
fn bare_done_turn(patch: Value) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": patch,
            "action": {"type": "done"},
        })
        .to_string(),
    )
}

/// The exact note `implement_bare_done` attaches.
const BARE_DONE_NOTE: &str =
    "`done` with no handoff named; verification passed, so this hands off to review.";

fn seed_plan_done_turn() -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {"plan": [{"id": 1, "text": "only step", "status": "done"}]},
            "action": {"type": "ask_user", "message": "seed?"},
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------
// green, plan complete, done with NEITHER done:true NOR next_skill ->
// handoff to review with the bare-done note; no rejection logged.
// ---------------------------------------------------------------------

#[tokio::test]
async fn bare_done_with_green_and_complete_plan_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        seed_plan_done_turn(),
        test_turn("true"),
        // Bare `done`: no `done: true`, no `next_skill` at all.
        bare_done_turn(json!({})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    // The passing `test` sets `green_step` to its own step; the harness then
    // commits one more version for the `tests` record `common_after_tool`
    // writes on every `test` call (BACKLOG.md item 5), so the counter read
    // back here is one past it.
    assert_eq!(
        state.harness.green_step,
        Some(state.harness.step - 1),
        "setup: the test call must have passed"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; a bare done (green, plan complete) must hand off to review, not be \
         rejected"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.next_skill.as_deref(), Some("review"));

    let notes = continued_notes(&log);
    // The handoff note is attached via `with_notes`, not logged as a
    // `continued` System entry (the action stayed `Done`, not `Continue`).
    // Check it via the log's Patch-adjacent System `note` entries instead.
    let has_note = log.entries(Some(LogKind::System), 10_000).iter().any(|e| {
        e.payload.get("event").and_then(Value::as_str) == Some("note")
            && e.payload.get("text").and_then(Value::as_str) == Some(BARE_DONE_NOTE)
    });
    assert!(
        has_note || notes.iter().any(|n| n == BARE_DONE_NOTE),
        "expected the bare-done note to be logged somewhere: {:?}",
        log.entries(Some(LogKind::System), 10_000)
    );
}

// ---------------------------------------------------------------------
// no green (green_step: None), plan complete, bare done (no next_skill) ->
// falls through to implement_no_green_runs_test (no green run at all here,
// so it runs the test rather than rejecting for a missing next_skill).
// ---------------------------------------------------------------------

#[tokio::test]
async fn bare_done_without_green_falls_through_to_runs_test() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        seed_plan_done_turn(),
        // Bare `done`: no `done: true`, no `next_skill`, and no `test` has
        // ever run this session.
        bare_done_turn(json!({})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None);

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; no green run at all must run the test instead of rejecting"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted test call must have executed"
    );
}

// ---------------------------------------------------------------------
// green_step: None (no test has run), an OPEN plan step, bare done (no
// next_skill) -> fix 3 (`skills/src/coding/mod.rs`) reordered the chain so
// `implement_no_green_runs_test` now runs BEFORE `premature_done_note`: with
// no test ever having run (`last_test_cmd: None`), it substitutes a real
// `test` call regardless of the open plan step -- running the test is the
// right next move whatever is open. Neither the premature-done note nor the
// bare-done handoff fires; `green_done_note` still never gets a chance
// (requires `green_step.is_some()`). See
// `bare_done_with_open_step_and_failing_run_uses_premature_done_note` below
// for the one case where the premature-done note keeps its old priority.
// ---------------------------------------------------------------------

#[tokio::test]
async fn bare_done_with_open_step_and_no_green_runs_test_instead() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "done"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Bare `done` with an open step still pending, and no test run at
        // all this session.
        bare_done_turn(json!({})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "no test has run yet");

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; no green run at all must run the test instead of rejecting or \
         converting to a premature-done note"
    );
    assert!(!any_rejected(&log));

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.harness.tool_calls, 1,
        "the substituted test call must have executed"
    );

    let notes = continued_notes(&log);
    assert!(
        notes
            .iter()
            .all(|n| !n.contains("plan step 2 is still open")),
        "the premature-done note must not fire when the test substitution takes priority: \
         {notes:?}"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "no handoff must have landed: the substituted test call is not a handoff"
    );
}

// ---------------------------------------------------------------------
// green_step: None, an OPEN plan step, but a FAILING test is the most
// recent run and nothing has been edited since -> the test substitution
// declines (re-running the identical failing command would tell the model
// nothing new) and the premature-done note keeps its old priority, exactly
// as before fix 3.
// ---------------------------------------------------------------------

#[tokio::test]
async fn bare_done_with_open_step_and_failing_run_uses_premature_done_note() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "first change", "status": "done"},
                        {"id": 2, "text": "second change", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // A failing test establishes last_test_cmd with green_step staying
        // None, and edits_since_test at 0 (nothing edited since).
        test_turn("false"),
        // Bare `done` with an open step still pending.
        bare_done_turn(json!({})),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec2).unwrap();
    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.harness.green_step, None, "the test call failed");
    assert_eq!(state.harness.edits_since_test, 0);

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(matches!(outcome3, Outcome::Continue), "got {outcome3:?}");
    assert!(!any_rejected(&log));

    let notes = continued_notes(&log);
    let note = notes.last().expect("a `continued` System entry must exist");
    assert!(
        note.contains("plan step 2 is still open"),
        "the premature-done note (open step) must fire ahead of the bare-done handoff: {note}"
    );
    assert_ne!(
        note, BARE_DONE_NOTE,
        "the bare-done note must not fire when a plan step is still open"
    );

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "no handoff must have landed: the plan step is still open"
    );
}

// ---------------------------------------------------------------------
// Fix 4 reproduction (bench battery, live run, thinking on): the exact
// shape from the log. A three-step plan (done/in_progress/pending), a
// maintainer-set `next` commitment (`{"tool": "shell", "path": "cargo
// test", "what": "run cargo test to verify the implementation"}` -- set via
// an operator patch, the same as a maintainer correcting harness state
// mid-session), a passing `test` (green_step set), then the model sending
// the exact output from the log: every plan step's status set to "done" (by
// id only, no text) with a bare `done` action carrying extra empty fields
// (`name`, `args`, `message`) and empty `ack_inbox`. `implement_bare_done`
// must convert this to a handoff to review -- it did not in the live run,
// 11 times across the battery.
// ---------------------------------------------------------------------

#[tokio::test]
async fn bare_done_with_maintainer_next_and_three_step_plan_hands_off_to_review() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // Step 1: seed the three-step plan from the log.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "text": "run cargo test to record the baseline", "status": "done"},
                        {"id": 2, "text": "implement the change", "status": "in_progress"},
                        {"id": 3, "text": "clean up", "status": "pending"},
                    ]
                },
                "action": {"type": "ask_user", "message": "seed?"},
            })
            .to_string(),
        ),
        // Step 3 (after the operator patch below sets `next`): a passing
        // test, establishing green_step.
        test_turn("true"),
        // Step 4: the exact model output from the log.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [
                        {"id": 1, "status": "done"},
                        {"id": 2, "status": "done"},
                        {"id": 3, "status": "done"},
                    ]
                },
                "action": {"type": "done", "name": "", "args": {}, "message": ""},
                "ack_inbox": []
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(Arc::clone(&skill)).await.unwrap();

    let rec1 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let _ = log.append_record(rec1).unwrap();

    // Step 2: the maintainer sets `next` via an operator patch, the same
    // way a maintainer correcting harness state mid-session would.
    active
        .apply_user_patch(json!({
            "next": {
                "tool": "shell",
                "path": "cargo test",
                "what": "run cargo test to verify the implementation",
            }
        }))
        .await
        .unwrap();

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "the passing test step itself is an ordinary tool call: {outcome2:?}"
    );
    let (state, _version) = active.session().state().await.unwrap();
    // The passing `test` sets `green_step` to its own step; the harness then
    // commits one more version for the `tests` record `common_after_tool`
    // writes on every `test` call (BACKLOG.md item 5), so the counter read
    // back here is one past it.
    assert_eq!(
        state.harness.green_step,
        Some(state.harness.step - 1),
        "green_step must be committed on the harness after the passing test"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();

    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; the bare-done conversion must fire and hand off to review"
    );
    assert!(!any_rejected(&log), "must not have been rejected");

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(state.agent.next_skill.as_deref(), Some("review"));
    assert!(
        state
            .agent
            .plan
            .iter()
            .all(|s| s.status == StepStatus::Done),
        "every plan step must be marked done: {:?}",
        state.agent.plan
    );

    let has_note = log.entries(Some(LogKind::System), 10_000).iter().any(|e| {
        e.payload.get("event").and_then(Value::as_str) == Some("note")
            && e.payload.get("text").and_then(Value::as_str) == Some(BARE_DONE_NOTE)
    });
    assert!(
        has_note,
        "expected the bare-done note to be logged: {:?}",
        log.entries(Some(LogKind::System), 10_000)
    );
}

// ---------------------------------------------------------------------
// Stale `next_skill` after a handoff (the bug this file's newest test is
// about): a handoff is requested by the model setting `next_skill` (plan's
// patch `{"next_skill": "implement"}` with action `done`). The session
// consumes it (`Skill::handoff`/`handoff_target` in `activate_inner`'s
// caller) but nothing cleared `next_skill` in the committed workspace
// afterward: the implement activation ran with `next_skill: "implement"`
// still committed. When implement then sent a bare `done` (neither
// `done: true` nor a new `next_skill`), `implement_bare_done`'s
// `merged.next_skill.is_some()` guard saw the STALE `"implement"` (a
// self-handoff, never valid) and stood aside, `Skill::handoff_target`
// returned `None` (implement -> implement is not an allowed transition),
// and `validate_default` rejected with "action `done` is only valid when
// the workspace is terminal or hands off". Eleven rejections in one bench
// battery, one retry-cap failure.
//
// `Skill::on_activate` (`core/src/skill.rs`) fixes this: called once right
// after an activation commits, it returns `{"next_skill": null}` whenever
// the freshly-committed workspace still carries a `next_skill` -- clearing
// exactly the field the handoff consumed, applied as a harness-sourced
// patch (source `"harness"`, reason `"on_activate"`).
//
// This test drives the live shape end to end: `plan` hands off with
// `{"next_skill": "implement"}` + `done`; `implement` activates (asserting
// `next_skill` is cleared right there, with the `Patch`/`on_activate` log
// entry); a passing `test`; then the exact bare-`done` output from the
// bench log -> `Outcome::Handoff("review")`, no `rejected` entry anywhere.
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_handoff_clears_next_skill_so_implement_bare_done_still_hands_off() {
    let root = tempfile::tempdir().unwrap();

    let turns = vec![
        // 1. plan: writes a one-step plan and hands off to implement.
        MockTurn::new(
            json!({
                "patch": {
                    "plan": [{"id": 1, "text": "only step", "status": "pending"}],
                    "next_skill": "implement",
                },
                "action": {"type": "done"},
            })
            .to_string(),
        ),
        // 2. implement: a passing test run.
        test_turn("true"),
        // 3. implement: the exact bare-done shape from the bench log --
        // every plan step's status set to "done" (by id only, no text), a
        // bare `done` action carrying extra empty fields, empty
        // `ack_inbox`.
        MockTurn::new(
            json!({
                "patch": {"plan": [{"id": 1, "status": "done"}]},
                "action": {"type": "done", "name": "", "args": {}, "message": ""},
                "ack_inbox": []
            })
            .to_string(),
        ),
    ];
    let model = Arc::new(MockModel::new(turns));
    let (mut session, log) = make_file_tools_session(Arc::clone(&model), config(&root));

    let plan_skill: Arc<PlanSkill> = Arc::new(PlanSkill::new());
    let mut plan_active = session.activate(Arc::clone(&plan_skill)).await.unwrap();

    let rec1 = plan_active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome1 = log.append_record(rec1).unwrap();
    assert!(
        matches!(outcome1, Outcome::Handoff(ref t) if t == "implement"),
        "got {outcome1:?}; plan must hand off to implement"
    );

    let (state, _version) = session.state().await.unwrap();
    assert_eq!(
        state.agent.next_skill.as_deref(),
        Some("implement"),
        "setup: the handoff target is still committed in the workspace right after plan's step \
         (activation hasn't run yet)"
    );

    // implement activates next, consuming the handoff. `on_activate` must
    // clear the now-stale `next_skill` as part of activation itself.
    let implement_skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session
        .activate(Arc::clone(&implement_skill))
        .await
        .unwrap();

    let (state, _version) = active.session().state().await.unwrap();
    assert_eq!(
        state.agent.next_skill, None,
        "next_skill must be cleared right after implement's activation commits, before any \
         step runs"
    );

    let entries = log.entries(Some(LogKind::Patch), 10_000);
    let on_activate_entry = entries.iter().find(|e| {
        e.payload.get("source").and_then(Value::as_str) == Some("harness")
            && e.payload.get("reason").and_then(Value::as_str) == Some("on_activate")
    });
    assert!(
        on_activate_entry.is_some(),
        "expected a Patch log entry with source=harness, reason=on_activate: {entries:?}"
    );

    let rec2 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome2 = log.append_record(rec2).unwrap();
    assert!(
        matches!(outcome2, Outcome::Continue),
        "got {outcome2:?}; the test call must have executed"
    );
    let (state, _version) = active.session().state().await.unwrap();
    // The passing `test` sets `green_step` to its own step; the harness then
    // commits one more version for the `tests` record `common_after_tool`
    // writes on every `test` call (BACKLOG.md item 5), so the counter read
    // back here is one past it.
    assert_eq!(
        state.harness.green_step,
        Some(state.harness.step - 1),
        "setup: the test call must have passed"
    );

    let rec3 = active.step(Event::Tick, &mut NullSink).await.unwrap();
    let outcome3 = log.append_record(rec3).unwrap();
    assert!(
        matches!(outcome3, Outcome::Handoff(ref t) if t == "review"),
        "got {outcome3:?}; the exact bare-done shape from the bench log must hand off to \
         review, not be rejected"
    );
    assert!(
        !any_rejected(&log),
        "must not have been rejected: {:?}",
        log.entries(Some(LogKind::System), 10_000)
    );
}
