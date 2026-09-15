//! Unit 26 (situational rules): `guards.situational_rules`, threaded into
//! `PlanSkill`/`ImplementSkill`/`ReviewSkill` via `with_situational_rules`
//! (`skills/src/coding/mod.rs`). With the switch on, `Skill::situational`
//! (`core/src/skill.rs`) returns `procedure::render(procedure::
//! situation(cur))` (`skills/src/coding/procedure.rs`), rendered by the
//! session as its own `## Now` section right after the harness view
//! (`core/src/session.rs`). With the switch off (the default), nothing
//! changes anywhere: no `## Now` section, and the skill's `spec()`
//! instructions are byte-identical to before this unit existed.
//!
//! Fixture style copied from `skills/tests/it/state_fields.rs`/
//! `think_listed.rs`: a real `Session<CodingWorkspace>` wired to the real
//! file tools via `common::make_file_tools_session`, a scripted
//! `MockModel`, and prompts read back via `MockModel::prompts()`
//! (`RecordedPrompt::user`/`system`).

use super::common;

use std::sync::Arc;

use serde_json::json;

use statie_core::{Event, NullSink, Outcome, SessionLogExt, Skill};
use statie_models::MockTurn;
use statie_skills::{ImplementSkill, PATCH_WHAT_CHANGED, PlanSkill, ReviewSkill, RouterSkill};

/// An `edit_file` turn with the given `old`/`new` text against `path` --
/// mirrors `error_think.rs`/`stall_think.rs`'s `edit_turn`.
fn edit_turn(path: &str, old: &str, new: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "edit_file",
                "args": {"path": path, "old": old, "new": new},
            },
        })
        .to_string(),
    )
}

/// A `test` turn running the given shell command.
fn test_turn(cmd: &str) -> MockTurn {
    MockTurn::new(
        json!({
            "patch": {},
            "action": {"type": "tool", "name": "test", "args": {"cmd": cmd}},
        })
        .to_string(),
    )
}

fn ask_user_turn() -> MockTurn {
    MockTurn::new(json!({"patch": {}, "action": {"type": "ask_user", "message": "?"}}).to_string())
}

/// A shell command that fails with an assertion-shaped summary --
/// `classify_failure` (`skills/src/coding/mod.rs`) reads this as
/// `FailureKind::Assertion`, matching `think_listed.rs`'s `ASSERTION_CMD`.
const ASSERTION_CMD: &str = "printf 'assertion failed: left: 1 right: 2\\n'; exit 1";

async fn run_step(
    active: &mut statie_core::Active<'_, ImplementSkill>,
    log: &Arc<dyn statie_core::SessionLog>,
) -> Outcome {
    let rec = active.step(Event::Tick, &mut NullSink).await.unwrap();
    log.append_record(rec).unwrap()
}

// =======================================================================
// (a) With the switch on: after a failing assertion `test`, the prompt's
// `## Now` section includes the failure-handling bullet and does NOT
// include the after-edit bullet.
// =======================================================================

#[tokio::test]
async fn on_after_failing_assertion_test_shows_failure_bullet_not_edit_bullet() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "old text\n").unwrap();

    let turns = vec![
        edit_turn("a.rs", "old text", "new text"),
        test_turn(ASSERTION_CMD),
        ask_user_turn(),
    ];
    let model = Arc::new(statie_models::MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new().with_situational_rules(true));
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: edit_file (sets last_tool = edit_file).
    let outcome1 = run_step(&mut active, &log).await;
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Step 2: test, fails with an assertion -- sets last_failure_summary
    // and last_tool = test.
    let outcome2 = run_step(&mut active, &log).await;
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");

    // Step 3's prompt is built from the state committed after step 2, so
    // its `## Now` section reflects `test_fail_assert`. Step 3's own turn
    // is `ask_user` (a valid way to park the run and inspect its prompt
    // without another edit/test executing), so its outcome is `Parked`,
    // not `Continue`.
    let outcome3 = run_step(&mut active, &log).await;
    assert!(matches!(outcome3, Outcome::Parked(_)), "got {outcome3:?}");

    let prompts = model.prompts();
    let prompt3 = &prompts[2];
    assert!(
        prompt3.user.contains("## Now"),
        "expected a `## Now` section in step 3's prompt, got:\n{}",
        prompt3.user
    );
    assert!(
        prompt3
            .user
            .contains("use it when a fix did not change the failure"),
        "expected the think-when-stuck (failure-handling) bullet in step 3's `## Now`, got:\n{}",
        prompt3.user
    );
    assert!(
        !prompt3
            .user
            .contains("run `test` and read the `result:` line"),
        "must NOT include the after-edit run-test bullet after a test failure, got:\n{}",
        prompt3.user
    );
}

// =======================================================================
// (b) With the switch on: after a successful `edit_file`, the prompt's
// `## Now` section includes "run `test`" guidance and NOT the failure
// bullet.
// =======================================================================

#[tokio::test]
async fn on_after_successful_edit_shows_run_test_guidance_not_failure_bullet() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "old text\n").unwrap();

    let turns = vec![edit_turn("a.rs", "old text", "new text"), ask_user_turn()];
    let model = Arc::new(statie_models::MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new().with_situational_rules(true));
    let mut active = session.activate(skill).await.unwrap();

    // Step 1: edit_file succeeds -- sets last_tool = edit_file.
    let outcome1 = run_step(&mut active, &log).await;
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");

    // Step 2's prompt is built from the state committed after step 1, so
    // its `## Now` section reflects `edit_ok`. Step 2's own turn is
    // `ask_user`, so its outcome is `Parked`, not `Continue`.
    let outcome2 = run_step(&mut active, &log).await;
    assert!(matches!(outcome2, Outcome::Parked(_)), "got {outcome2:?}");

    let prompts = model.prompts();
    let prompt2 = &prompts[1];
    assert!(
        prompt2.user.contains("## Now"),
        "expected a `## Now` section in step 2's prompt, got:\n{}",
        prompt2.user
    );
    assert!(
        prompt2
            .user
            .contains("run `test` and read the `result:` line"),
        "expected the after-edit run-test bullet in step 2's `## Now`, got:\n{}",
        prompt2.user
    );
    assert!(
        !prompt2
            .user
            .contains("use it when a fix did not change the failure"),
        "must NOT include the failure-handling bullet after a successful edit, got:\n{}",
        prompt2.user
    );
}

// =======================================================================
// (c) The step-3 prompt is smaller in tokens with the switch on than with
// it off, for the same scripted turns.
// =======================================================================

#[tokio::test]
async fn on_step_3_prompt_is_smaller_in_tokens_than_off() {
    async fn step3_tokens(situational: bool) -> usize {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "old text\n").unwrap();

        let turns = vec![
            edit_turn("a.rs", "old text", "new text"),
            test_turn(ASSERTION_CMD),
            ask_user_turn(),
        ];
        let model = Arc::new(statie_models::MockModel::new(turns));
        let cfg = common::config(&root);
        let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

        let skill: Arc<ImplementSkill> =
            Arc::new(ImplementSkill::new().with_situational_rules(situational));
        let mut active = session.activate(skill).await.unwrap();

        // Steps 1-2 execute a real tool call each (`Continue`); step 3's own
        // turn is `ask_user` (`Parked`) -- its prompt (built from the state
        // committed after step 2) is what `prompts[2]` below reads.
        let outcome1 = run_step(&mut active, &log).await;
        assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
        let outcome2 = run_step(&mut active, &log).await;
        assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
        let outcome3 = run_step(&mut active, &log).await;
        assert!(matches!(outcome3, Outcome::Parked(_)), "got {outcome3:?}");

        let prompts = model.prompts();
        prompts[2].tokens
    }

    let on_tokens = step3_tokens(true).await;
    let off_tokens = step3_tokens(false).await;

    assert!(
        on_tokens < off_tokens,
        "expected the switch-on step-3 prompt ({on_tokens} tokens) to be strictly smaller than \
         switch-off ({off_tokens} tokens)"
    );
}

// =======================================================================
// (d) With the switch off (default): no prompt contains `## Now`, and the
// spec text equals today's verbatim -- snapshot taken before moving any
// bullet.
// =======================================================================

/// `PlanSkill::new()`'s instructions, snapshotted verbatim before any
/// bullet was moved to `procedure.rs` -- the "switch off" acceptance
/// requires `spec()` stay byte-identical to this.
const PLAN_INSTRUCTIONS_SNAPSHOT: &str = "You are the planner for a coding task. Use `search` to find definitions and \
     usages before reading whole files; use `list_files` and `read_file` \
     (with `offset`/`limit` for files longer than ~200 lines) until you \
     understand what needs to change. Do not write or run code yet.\n\n\
     Patch only what changed this step. Never resend `task`. To move a plan step, send only \
     that entry with its `id` and the changed field, e.g. {\"plan\": [{\"id\": 3, \"status\": \
     \"done\"}]}; do not resend the other steps. Rewrite `recent_summary` only when what you \
     know changed, in at most two sentences. A step whose state did not change sends an empty \
     patch: {\"patch\": {}, \"action\": ...}.\n\n\
     What to put in the workspace:\n\
     - Record every file you judge relevant in `files`, each with a one-line \
       note explaining why it matters.\n\
     - Write `plan` as a numbered list of small, concrete steps; each step's \
       `text` should be at most one sentence and independently checkable. Give \
       each step a unique `id`. If the repo has tests, make the first step \
       \"run `test` to record the baseline\" and the last step \"run `test` and \
       confirm it passes\".\n\
     - Update `recent_summary` when you learn something that changes the plan.\n\
     - `notes` is kept by the harness from your previous reasoning: read it \
       before deciding; you cannot patch it.\n\
     - If you commit to a next step by setting `next`, the harness holds you to \
       it -- change `next` (and say why in `recent_summary`) if the plan changes.\n\
     - For a design fork with more than one reasonable approach, call `think` with \
       `mode: \"options\"` before writing the plan.\n\n\
     When to hand off: once the plan is solid, set `next_skill: \"implement\"` \
     and action `done`. The plan must be non-empty before you hand off to \
     `implement`.\n\n\
     History is searchable with the `recall` tool if you need to check what an \
     earlier step already found. `recent_summary` is the only memory that \
     survives across steps and across skill handoffs. If a tool output overflowed \
     (the observation says so), call `filter_last_tool_call` with a regex instead \
     of re-running the command.";

/// `ImplementSkill::new()`'s instructions, snapshotted verbatim before any
/// bullet was moved to `procedure.rs`.
const IMPLEMENT_INSTRUCTIONS_SNAPSHOT: &str = "You are implementing the plan for a coding task, one step at a time.\n\n\
     Patch only what changed this step. Never resend `task`. To move a plan step, send only \
     that entry with its `id` and the changed field, e.g. {\"plan\": [{\"id\": 3, \"status\": \
     \"done\"}]}; do not resend the other steps. Rewrite `recent_summary` only when what you \
     know changed, in at most two sentences. A step whose state did not change sends an empty \
     patch: {\"patch\": {}, \"action\": ...}.\n\n\
     What to put in the workspace:\n\
     - Mark the step you are working `in_progress`, then `done` once it is \
       finished; use `skipped` only when a step turns out unnecessary.\n\
     - Prefer `edit_file` for changes to existing files: give it a small \
       anchored `old` text copied exactly from a `read_file` result, including \
       indentation. Use `write_file` only for new files or full rewrites. If \
       `edit_file` reports the text was not found or ambiguous, re-read the \
       file and retry with a more specific anchor -- never fall back to \
       rewriting the whole file. To add code, anchor on one line you can see: \
       `edit_file` with `after` (or `before`) and `new`, instead of a \
       multi-line `old`. When a tool reports a line number, read from that \
       offset (`read_file` with `offset`) instead of paging from the top. \
       Use `shell` for other changes.\n\
     - Commit before you look: when you decide what to change, set `next` \
       (tool, path, what) in the same patch, where `tool` is the mutating call \
       you will make -- `edit_file`, `write_file`, `shell`, or `test`, never \
       `read_file`. While `next` is set you may read that file at most twice; \
       then the harness expects the call. If the plan changes, change `next` \
       and say why in `recent_summary`.\n\
     - After finishing a step that changes code, run `test` and read the \
       `result:` line; do not mark a step `done` while `test` reports `fail` \
       unless the failure predates your change (check with `recall`, e.g. \
       query \"test output\" to see the last test run).\n\
     - Keep `files` up to date as you go; update `recent_summary` when what you \
       know changed.\n\
     - `notes` is kept by the harness from your previous reasoning: read it \
       before deciding; you cannot patch it.\n\
     - `think` asks a reasoning subagent that sees what you see; use it when a \
       fix did not change the failure or you cannot tell which cause is real, \
       then act on its `decision`.\n\
     - `try_fixes` is only useful after a failing `test`: it proposes and verifies \
       candidate fixes for the most recent failure, so call `test` first if none has \
       failed yet.\n\
     - If you are blocked (missing information only the user can give you), use \
       action `ask_user` rather than guessing -- it is the only way to pause; \
       `yield` is rejected in this skill.\n\n\
     When to hand off: once every plan step is `done` or `skipped`, set \
     `next_skill: \"review\"` and action `done`. Do not hand off to `review` \
     while any step is still `pending` or `in_progress`.\n\n\
     History is searchable with the `recall` tool -- use it to recall what an \
     earlier step already tried or found. `recent_summary` is the only memory \
     that survives across steps and across skill handoffs. If a tool output \
     overflowed (the observation says so), call `filter_last_tool_call` with a \
     regex instead of re-running the command.";

/// `ReviewSkill::new()`'s instructions, snapshotted verbatim -- `review` has
/// no bullet that moved (see `procedure.rs`'s module doc), so this must
/// equal `spec()`'s output with the switch either on or off.
const REVIEW_INSTRUCTIONS_SNAPSHOT: &str = "You are reviewing the work done for a coding task. Run `test` first: \
     `result: fail` is an automatic finding -- quote the failure summary. Use \
     `search` to check that every plan step's change actually landed. Read only \
     the files in `files` unless a finding requires more. Look before you approve: \
     read the changed files or run the tests first; an approval with no tool call \
     in this activation is not accepted. Before approving, run the tests without a \
     name filter (the crate's whole test target); a pass on one filtered test does \
     not cover regressions.\n\n\
     Patch only what changed this step. Never resend `task`. To move a plan step, send only \
     that entry with its `id` and the changed field, e.g. {\"plan\": [{\"id\": 3, \"status\": \
     \"done\"}]}; do not resend the other steps. Rewrite `recent_summary` only when what you \
     know changed, in at most two sentences. A step whose state did not change sends an empty \
     patch: {\"patch\": {}, \"action\": ...}.\n\n\
     What to put in the workspace:\n\
     - Write concrete `findings` -- specific, actionable problems, not vague \
       impressions. Leave `findings` empty if the work is correct and complete.\n\
     - If you commit to a next step by setting `next`, the harness holds you to \
       it -- change `next` (and say why in `recent_summary`) if the plan changes.\n\n\
     When to hand off: if `findings` is non-empty, set `next_skill: \"implement\"` \
     and action `done` so the problems get fixed. If `findings` is empty, set \
     `done: true` and action `done` -- this ends the task. `done: true` is only \
     valid with empty `findings`. To send work back, set `findings` and send \
     `done` with `next_skill: \"implement\"`; `done: true` is an approval and \
     needs empty findings.\n\n\
     History is searchable with the `recall` tool -- use it to see what \
     `implement` actually did. `recent_summary` is the only memory that survives \
     across steps and across skill handoffs.";

#[test]
fn off_plan_spec_matches_pre_unit_26_snapshot() {
    let skill = PlanSkill::new();
    assert_eq!(skill.spec().instructions(), PLAN_INSTRUCTIONS_SNAPSHOT);
}

#[test]
fn off_implement_spec_matches_pre_unit_26_snapshot() {
    let skill = ImplementSkill::new();
    assert_eq!(skill.spec().instructions(), IMPLEMENT_INSTRUCTIONS_SNAPSHOT);
}

#[test]
fn off_review_spec_matches_pre_unit_26_snapshot() {
    let skill = ReviewSkill::new();
    assert_eq!(skill.spec().instructions(), REVIEW_INSTRUCTIONS_SNAPSHOT);
}

/// Unit 41 ("patch what changed"): all four coding skills' rendered
/// instructions contain the shared `PATCH_WHAT_CHANGED` paragraph exactly
/// once, verbatim (not paraphrased in the test -- the const is imported and
/// compared byte for byte).
#[test]
fn all_four_coding_skills_contain_patch_what_changed_exactly_once() {
    fn occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    let router = RouterSkill::new().spec().instructions().to_string();
    assert_eq!(
        occurrences(&router, PATCH_WHAT_CHANGED),
        1,
        "expected PATCH_WHAT_CHANGED exactly once in router's instructions, got:\n{router}"
    );

    let plan = PlanSkill::new().spec().instructions().to_string();
    assert_eq!(
        occurrences(&plan, PATCH_WHAT_CHANGED),
        1,
        "expected PATCH_WHAT_CHANGED exactly once in plan's instructions, got:\n{plan}"
    );

    let implement = ImplementSkill::new().spec().instructions().to_string();
    assert_eq!(
        occurrences(&implement, PATCH_WHAT_CHANGED),
        1,
        "expected PATCH_WHAT_CHANGED exactly once in implement's instructions, got:\n{implement}"
    );

    let review = ReviewSkill::new().spec().instructions().to_string();
    assert_eq!(
        occurrences(&review, PATCH_WHAT_CHANGED),
        1,
        "expected PATCH_WHAT_CHANGED exactly once in review's instructions, got:\n{review}"
    );
}

/// Unit 41: the plan skill's instructions contain the new conditional
/// `recent_summary` bullet and not the old unconditional one (exact
/// strings).
#[test]
fn plan_instructions_have_new_recent_summary_bullet_not_old() {
    let plan = PlanSkill::new().spec().instructions().to_string();
    assert!(
        plan.contains("Update `recent_summary` when you learn something that changes the plan."),
        "expected the new conditional recent_summary bullet in plan's instructions, got:\n{plan}"
    );
    assert!(
        !plan.contains("Keep `recent_summary` current as you explore."),
        "the old unconditional recent_summary bullet must be gone from plan's instructions, got:\n{plan}"
    );
}

/// Explicit `with_situational_rules(false)` must match the same snapshots
/// as `new()` (which defaults to `false`) -- the builder's `false` arm is
/// not just "whatever `new()` happens to do".
#[test]
fn explicit_situational_rules_false_matches_snapshot_too() {
    assert_eq!(
        PlanSkill::new()
            .with_situational_rules(false)
            .spec()
            .instructions(),
        PLAN_INSTRUCTIONS_SNAPSHOT
    );
    assert_eq!(
        ImplementSkill::new()
            .with_situational_rules(false)
            .spec()
            .instructions(),
        IMPLEMENT_INSTRUCTIONS_SNAPSHOT
    );
    assert_eq!(
        ReviewSkill::new()
            .with_situational_rules(false)
            .spec()
            .instructions(),
        REVIEW_INSTRUCTIONS_SNAPSHOT
    );
}

#[tokio::test]
async fn off_no_prompt_contains_now_section() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "old text\n").unwrap();

    let turns = vec![
        edit_turn("a.rs", "old text", "new text"),
        test_turn(ASSERTION_CMD),
        ask_user_turn(),
    ];
    let model = Arc::new(statie_models::MockModel::new(turns));
    let cfg = common::config(&root);
    let (mut session, log) = common::make_file_tools_session(Arc::clone(&model), cfg);

    // `with_situational_rules` not called -- `ImplementSkill::new()`
    // defaults to the switch off, same as `Guards::default()`.
    let skill: Arc<ImplementSkill> = Arc::new(ImplementSkill::new());
    let mut active = session.activate(skill).await.unwrap();

    // Steps 1-2 execute a real tool call each (`Continue`); step 3's own
    // turn is `ask_user` (`Parked`).
    let outcome1 = run_step(&mut active, &log).await;
    assert!(matches!(outcome1, Outcome::Continue), "got {outcome1:?}");
    let outcome2 = run_step(&mut active, &log).await;
    assert!(matches!(outcome2, Outcome::Continue), "got {outcome2:?}");
    let outcome3 = run_step(&mut active, &log).await;
    assert!(matches!(outcome3, Outcome::Parked(_)), "got {outcome3:?}");

    for (i, p) in model.prompts().iter().enumerate() {
        assert!(
            !p.user.contains("## Now"),
            "prompt at index {i} must not contain a `## Now` section with the switch off, got:\n{}",
            p.user
        );
    }
}
