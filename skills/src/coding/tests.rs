use std::sync::Arc;

use serde_json::json;

use statie_core::{
    Action, CommittedState, HarnessState, ParsedOutput, RawOutput, Report, ReportOutcome,
    ReportStats, Skill, Version,
};
use statie_patch::JsonMergePatch;

use super::{
    CodingWorkspace, Decision, FileNote, ImplementSkill, PlanSkill, PlanStep, ReviewSkill,
    RouterSkill, SeenFile, StepStatus, coding_skills, coding_skills_with_delegates,
    common_after_tool, count_lines, has_named_handoff, merge_range, parse_edit_header,
    parse_revert_lines, seen_entry,
};

fn committed(agent: CodingWorkspace) -> CommittedState<CodingWorkspace> {
    CommittedState {
        harness: HarnessState {
            inbox: vec![],
            active_tools: vec![],
            budgets: Default::default(),
            activation: "irrelevant".to_string(),
            step: 0,
            last_overflow: None,
            pinned: None,
            green_step: None,
            green_filtered: false,
            tool_calls: 0,
            last_test_cmd: None,
            edits_since_test: 0,
            last_think_step: None,
            repeated_failure_step: None,
        },
        agent,
    }
}

/// Same as `committed`, but with `harness.tool_calls` set explicitly --
/// used by the `review`-must-look-before-it-approves tests
/// (`review_looked_note`, `skills/src/coding/mod.rs`), which gate on that
/// field.
fn committed_with_tool_calls(
    agent: CodingWorkspace,
    tool_calls: u64,
) -> CommittedState<CodingWorkspace> {
    let mut cur = committed(agent);
    cur.harness.tool_calls = tool_calls;
    cur
}

fn raw(version: u64, content: serde_json::Value) -> RawOutput {
    RawOutput::new(Version(version), None, content.to_string())
}

fn parse_and_validate<
    K: Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>,
>(
    skill: &K,
    cur: &CommittedState<CodingWorkspace>,
    content: serde_json::Value,
) -> Result<statie_core::ValidatedStep<CodingWorkspace>, statie_core::PatchError> {
    let parsed = ParsedOutput::<CodingWorkspace>::parse::<K::Patch>(raw(0, content))
        .expect("should parse as valid StepOutput JSON");
    skill.validate(cur, parsed)
}

// ---------------------------------------------------------------------
// 1. Object safety / coercion, distinct names.
// ---------------------------------------------------------------------

#[test]
fn all_four_skills_coerce_to_trait_object() {
    let _router: Arc<
        dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>,
    > = Arc::new(RouterSkill::new());
    let _plan: Arc<
        dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>,
    > = Arc::new(PlanSkill::new());
    let _implement: Arc<
        dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>,
    > = Arc::new(ImplementSkill::new());
    let _review: Arc<
        dyn Skill<Workspace = CodingWorkspace, Patch = JsonMergePatch<CodingWorkspace>>,
    > = Arc::new(ReviewSkill::new());
}

#[test]
fn coding_skills_has_four_distinct_names() {
    let skills = coding_skills();
    assert_eq!(skills.len(), 4);
    let mut names: Vec<&str> = skills.iter().map(|(n, _)| n.as_str()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names, vec!["implement", "plan", "review", "router"]);
    // Each entry's key matches the skill's own `.name()`.
    for (key, skill) in &skills {
        assert_eq!(key, skill.name());
    }
}

// ---------------------------------------------------------------------
// 2. Schema shape: patch subtree has `plan`, no `required` under `patch`.
// ---------------------------------------------------------------------

#[test]
fn schema_patch_subtree_has_plan_and_no_required() {
    use statie_core::{Schema, StepOutput};
    use statie_patch::MergePatch;

    let schema = Schema::of::<StepOutput<MergePatch<CodingWorkspace>>>();
    let value = schema.as_value().clone();

    let plan = value.pointer("/properties/patch/properties/plan");
    assert!(
        plan.is_some(),
        "expected properties.patch.properties.plan in {value}"
    );

    fn walk_no_required(value: &serde_json::Value, path: &str) {
        match value {
            serde_json::Value::Object(map) => {
                assert!(
                    !map.contains_key("required"),
                    "found `required` at {path}: {value}"
                );
                for (k, v) in map {
                    walk_no_required(v, &format!("{path}.{k}"));
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk_no_required(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }
    let patch_subtree = value
        .pointer("/properties/patch")
        .expect("patch property should exist");
    walk_no_required(patch_subtree, "properties.patch");
}

// ---------------------------------------------------------------------
// 3. Rules via the public validation path.
// ---------------------------------------------------------------------

#[test]
fn recent_summary_over_1200_chars_is_rejected() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let long = "x".repeat(1201);
    let content = json!({
        "patch": {"recent_summary": long},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
    );
}

#[test]
fn recent_summary_at_1200_chars_is_accepted() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let ok = "x".repeat(1200);
    let content = json!({
        "patch": {"recent_summary": ok, "next_skill": "plan"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    parse_and_validate(&skill, &cur, content).expect("1200 chars should be accepted");
}

#[test]
fn next_skill_bogus_is_rejected() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next_skill": "bogus"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
    );
}

#[test]
fn plan_setting_next_skill_review_is_rejected_not_allowed() {
    // Non-empty committed plan: an empty plan would take the `done`
    // conversion path (see `plan_done_with_empty_plan_continues_and_strips_next_skill`
    // below) before this transition check ever runs.
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "do the thing".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {"next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
    );
}

/// Renamed from `plan_handoff_to_implement_with_empty_plan_is_rejected`:
/// an empty-plan `done` (with or without `next_skill`) now converts to
/// `Action::Continue` instead of being rejected (statie-spec.md "the plan
/// skill: `done` with an empty plan commits and notes"). This case sends
/// `next_skill: "implement"` explicitly alongside `done` -- the shape a
/// live run sent twice at the start, since the instructions say "set
/// next_skill and send done".
#[test]
fn plan_done_with_next_skill_and_empty_plan_continues_and_strips_next_skill() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next_skill": "implement"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("empty-plan done must continue, not reject");
    let (patch, action, _) = step.into_parts();
    match action {
        statie_core::Action::Continue { note } => {
            assert!(note.contains("the plan is empty"), "{note}");
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert_eq!(
        patch.next().next_skill,
        None,
        "next_skill must have been stripped from the patch"
    );
}

#[test]
fn plan_handoff_to_implement_with_one_step_is_accepted() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "next_skill": "implement",
            "plan": [{"id": 1, "text": "do the thing", "status": "pending"}]
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    parse_and_validate(&skill, &cur, content).expect("non-empty plan should be accepted");
}

// ---------------------------------------------------------------------
// Fix 1: a bare `done` (no `next_skill`) in `plan` with a non-empty plan
// is treated as "hand off to implement", instead of being rejected forever
// -- a small local model was observed writing a full plan, then repeating
// exactly this premature `done` (reasoning "I will now proceed to
// implement") without ever adding the field the rejection asked for.
// ---------------------------------------------------------------------

#[test]
fn plan_bare_done_with_nonempty_committed_plan_injects_next_skill_implement() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "do the thing".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    // `done` with no `next_skill` in the patch at all -- exactly the
    // rejected shape from the bug report.
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("bare done with a non-empty plan should be treated as handoff to implement");
    let (patch, action, _) = step.into_parts();
    assert!(matches!(action, statie_core::Action::Done));
    assert_eq!(
        patch.next().next_skill.as_deref(),
        Some("implement"),
        "next_skill should have been injected as implement"
    );
}

#[test]
fn plan_bare_done_with_plan_written_in_same_patch_injects_next_skill_implement() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    // The plan is written and `done` sent in the very same step, with no
    // `next_skill` -- the merged (post-patch) plan is non-empty even
    // though the committed plan was empty.
    let content = json!({
        "patch": {
            "plan": [{"id": 1, "text": "do the thing", "status": "pending"}]
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect(
        "bare done with a plan written in the same patch should be treated as handoff to implement",
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
}

/// Renamed from `plan_bare_done_with_empty_plan_is_still_rejected`: a bare
/// `done` (no `next_skill` at all) with an empty plan now converts to
/// `Action::Continue` too, the same as the `next_skill`-included shape above
/// (statie-spec.md "the plan skill: `done` with an empty plan commits and
/// notes"). The empty (`{}`) patch still commits -- there is nothing in it
/// to strip.
#[test]
fn plan_bare_done_with_empty_plan_continues() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("empty-plan bare done must continue, not reject");
    let (_, action, _) = step.into_parts();
    match action {
        statie_core::Action::Continue { note } => {
            assert!(note.contains("the plan is empty"), "{note}");
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
}

#[test]
fn plan_done_with_explicit_next_skill_is_not_overridden() {
    // A model that correctly sets next_skill itself must not have its
    // choice overridden -- only a bare `done` (no next_skill at all) is
    // treated specially. Non-empty committed plan: an empty plan would take
    // the `done` conversion path instead (see
    // `plan_done_with_next_skill_and_empty_plan_continues_and_strips_next_skill`).
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "do the thing".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {"next_skill": "router"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("explicit router should validate");
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("router"));
}

// ---------------------------------------------------------------------
// `done: true` is never `plan`'s (or `router`'s) to set: the fix for the
// bench log where `plan`'s first step after activation sent
// `{"patch": {"done": true}, "action": {"type": "done"}}` with an empty
// plan and it committed `done: true`, ending the session as falsely `done`.
// See `plan_done_not_ours_note`/`router_done_not_ours_note`/
// `done_not_ours_note` in `skills/src/coding/mod.rs`. Session-level
// coverage of the exact bug-report sequence lives in
// `skills/tests/plan_done.rs`.
// ---------------------------------------------------------------------

#[test]
fn plan_done_true_with_empty_plan_gets_the_empty_plan_note_not_the_not_ours_note() {
    // The empty-plan note must win when the plan is empty -- not the more
    // general not-ours note -- even though both fire on `done: true`.
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("done:true with an empty plan must continue, not reject");
    let (patch, action, _) = step.into_parts();
    match action {
        Action::Continue { note } => {
            assert!(note.contains("the plan is empty"), "{note}");
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert!(!patch.next().done, "done must not have committed as true");
}

#[test]
fn plan_done_true_with_valid_plan_and_no_next_skill_converts_to_continue() {
    // A non-empty, valid plan with `done: true` and no `next_skill`: NOT
    // the old auto-inject-next_skill shortcut (that shortcut only applies
    // when the raw patch carries no `done` field at all -- see
    // `plan_bare_done_with_nonempty_committed_plan_injects_next_skill_implement`
    // above). `done: true` is never plan's to set, so this converts to
    // `Continue` with the not-ours note instead of silently handing off.
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "done": true,
            "plan": [{"id": 1, "text": "do the thing", "status": "pending"}],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("done:true with a valid plan and no next_skill must continue, not reject");
    let (patch, action, _) = step.into_parts();
    match action {
        Action::Continue { note } => {
            assert!(
                note.contains("not the plan skill's to set"),
                "expected the not-ours note, got: {note}"
            );
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert!(!patch.next().done, "done must not have committed as true");
    assert_eq!(
        patch.next().next_skill,
        None,
        "no next_skill was sent and none must have been injected"
    );
    // The plan itself still landed -- only `done` was stripped.
    assert_eq!(patch.next().plan.len(), 1);
}

#[test]
fn plan_done_true_with_next_skill_keeps_handoff_but_strips_done() {
    // A malformed-but-well-intentioned patch that sets both `done: true`
    // AND a valid `next_skill`: the handoff still proceeds (action stays
    // `Done`), but `done` itself is stripped -- `plan` never gets to
    // commit `done: true`, even when it also names a real handoff.
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "done": true,
            "next_skill": "implement",
            "plan": [{"id": 1, "text": "do the thing", "status": "pending"}],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("done:true with a named handoff must still validate");
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(action, Action::Done),
        "a named handoff must not be converted to Continue"
    );
    assert!(
        !patch.next().done,
        "done must not have committed as true even alongside a real handoff"
    );
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
}

#[test]
fn plan_proper_handoff_with_no_done_field_is_unaffected() {
    // The positive control: a proper handoff with no `done` field in the
    // patch at all (the ordinary shape `plan`'s instructions describe)
    // passes straight through -- `plan_done_not_ours_note` never fires
    // since the raw patch never set `done` to anything.
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "next_skill": "implement",
            "plan": [{"id": 1, "text": "do the thing", "status": "pending"}],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("proper handoff should validate");
    let (patch, action, _) = step.into_parts();
    assert!(matches!(action, Action::Done));
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
    assert!(!patch.next().done);
}

#[test]
fn router_done_true_bare_converts_to_continue_and_strips_done() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("router done:true with no next_skill must continue, not reject");
    let (patch, action, _) = step.into_parts();
    match action {
        Action::Continue { note } => {
            assert!(
                note.contains("not the router's to set"),
                "expected the router not-ours note, got: {note}"
            );
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert!(!patch.next().done, "done must not have committed as true");
}

#[test]
fn router_done_true_with_next_skill_keeps_handoff_but_strips_done() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true, "next_skill": "plan"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("router done:true with a named handoff must still validate");
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(action, Action::Done),
        "a named handoff must not be converted to Continue"
    );
    assert!(
        !patch.next().done,
        "done must not have committed as true even alongside a real handoff"
    );
    assert_eq!(patch.next().next_skill.as_deref(), Some("plan"));
}

#[test]
fn implement_bare_done_with_green_and_complete_plan_is_converted_to_handoff() {
    // The injection is specific to `plan`; `implement` (also transitions
    // via next_skill/done) must still reject a bare done with the
    // ordinary message, not silently pick a target. `green_step: Some(0)`
    // (a passing test run) keeps `implement_no_green_note`
    // (`skills/src/coding/mod.rs`) from firing instead, since that is not
    // what this test is about.
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Done,
        }],
        ..Default::default()
    });
    cur.harness.green_step = Some(0);
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    // `implement_bare_done` (`skills/src/coding/mod.rs`): a bare `done`
    // with a passing test and no open step is no longer rejected by the
    // "terminal or hands off" rule; it becomes the handoff to review.
    let step = parse_and_validate(&skill, &cur, content).expect("converted, not rejected");
    assert!(matches!(step.action(), statie_core::Action::Done));
}

/// Previously: `done` with `next_skill: "review"` while a plan step is
/// pending was rejected (`PatchError::RuleViolation`) and the patch (here,
/// the attempted `next_skill` change) was discarded. Then:
/// `premature_done_note` converted it to `Action::Continue` naming the still
/// -open step. Now (fix 3, `skills/src/coding/mod.rs`): with `green_step:
/// None` and no `test` call ever having run (`last_test_cmd: None`),
/// `implement_no_green_runs_test` runs BEFORE `premature_done_note` and
/// fires even with the plan step still open -- running the test is the
/// right next move whatever is open, and the substituted `test` call's own
/// observation says what it says. See
/// `implement_handoff_to_review_with_pending_step_and_failing_run_continues_instead_of_handoff`
/// below for the one case where `premature_done_note` keeps its old
/// priority.
#[test]
fn implement_handoff_to_review_with_pending_step_runs_test_instead_of_continuing() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {"next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("the substituted test call should not be rejected");
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(
            action,
            statie_core::Action::Tool { ref name, .. } if name == "test"
        ),
        "expected a substituted `test` tool call, got {action:?}"
    );
    assert_eq!(
        patch.next().next_skill,
        None,
        "next_skill must be stripped so the handoff does not land"
    );
}

/// The one shape where `premature_done_note`'s open-step conversion still
/// keeps priority over the test substitution (see fix 3's doc on
/// `implement_no_green_runs_test`): `green_step: None`, an open plan step,
/// a FAILING run as the most recent one, and nothing edited since. Running
/// the identical failing command again would tell the model nothing new, so
/// `implement_no_green_runs_test`'s test-substitution branch does not fire
/// (`should_run_test` is false: `edits_since_test == 0` and `last_test_cmd`
/// is `Some`), and it explicitly declines when an open step is present in
/// that case, falling through to `premature_done_note`.
#[test]
fn implement_handoff_to_review_with_pending_step_and_failing_run_continues_instead_of_handoff() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    cur.harness.last_test_cmd = Some("cargo test".to_string());
    cur.harness.edits_since_test = 0;
    // green_step stays None: the latest test run failed.
    let content = json!({
        "patch": {"next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("premature done should convert to Continue, not reject");
    let (patch, action, _) = step.into_parts();
    match action {
        statie_core::Action::Continue { note } => {
            assert!(note.contains("plan step 1 is still open"), "{note}");
            assert!(note.contains('a'), "{note}");
            assert!(note.contains("set its status to done"), "{note}");
            assert!(note.contains("run `test`"), "{note}");
        }
        other => panic!("expected Action::Continue, got {other:?}"),
    }
    assert_eq!(
        patch.next().next_skill,
        None,
        "next_skill must be stripped so the handoff does not land"
    );
}

/// Fix 4 reproduction (bench battery, live run, thinking on): `implement_
/// bare_done` should convert a `done` with neither `done: true` nor
/// `next_skill` into a handoff to review when `green_step` is `Some` and the
/// merged plan has no open step -- but in this live run it did not, 11
/// times across the battery, and the core "action `done` is only valid when
/// the workspace is terminal or hands off" rule rejected the step instead.
/// Exact reproduction from the log: a committed workspace with a plan whose
/// three steps are `done`/`in_progress`/`pending`, a maintainer-set `next`
/// commitment (`{"tool": "shell", "path": "cargo test", "what": "run cargo
/// test to verify the implementation"}`), `harness.green_step: Some(13)` (a
/// `test` pass at the previous step), and the model's output setting every
/// plan step's `status` to `"done"` (by `id` only, no `text`) with a bare
/// `done` action and empty `ack_inbox`.
#[test]
fn implement_bare_done_with_maintainer_next_and_all_steps_marked_done_hands_off_to_review() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "run cargo test to record the baseline".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "implement the change".to_string(),
                status: StepStatus::InProgress,
            },
            PlanStep {
                id: 3,
                text: "clean up".to_string(),
                status: StepStatus::Pending,
            },
        ],
        next: Some(statie_core::Commitment {
            tool: "shell".to_string(),
            path: Some("cargo test".to_string()),
            what: "run cargo test to verify the implementation".to_string(),
        }),
        ..Default::default()
    });
    cur.harness.green_step = Some(13);
    let content = json!({
        "patch": {
            "plan": [
                {"id": 1, "status": "done"},
                {"id": 2, "status": "done"},
                {"id": 3, "status": "done"},
            ]
        },
        "action": {"type": "done", "name": "", "args": {}, "message": ""},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("the bare-done handoff must not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Done),
        "expected the action to stay Done (a handoff), got {:?}",
        step.action()
    );
    let (patch, _action, _) = step.into_parts();
    assert_eq!(
        patch.next().next_skill.as_deref(),
        Some("review"),
        "implement_bare_done must inject next_skill: review"
    );
}

/// Only `review`'s approval (empty `findings`) finishes the task
/// (statie-spec.md §5/§10) -- `implement` only ever hands off, via
/// `next_skill`. `ImplementSkill::is_terminal` is unconditionally `false`,
/// even when `ws.done` is somehow `true` (it should never get there in the
/// first place -- see `implement_done_true_with_green_and_complete_plan_
/// strips_done_and_hands_off_to_review` below for the `validate`-side half
/// that keeps it from committing), mirroring `plan_is_terminal_is_always_
/// false_even_when_done_is_true`/`router_is_terminal_is_always_false_even_
/// when_done_is_true` above.
#[test]
fn implement_is_terminal_is_always_false_even_when_done_is_true() {
    let skill = ImplementSkill::new();
    let mut ws = CodingWorkspace::default();
    assert!(!skill.is_terminal(&ws));
    ws.done = true;
    assert!(
        !skill.is_terminal(&ws),
        "implement must never be terminal, whatever ws.done says"
    );
}

/// `implement_done_not_ours_strip` (`skills/src/coding/mod.rs`): a raw
/// `done: true` in the patch is stripped BEFORE the rest of `implement`'s
/// `done`-handling chain runs, so it never reaches `validate_default`'s
/// merge. Here the plan is fully done and `green_step: Some(n)` (a passing
/// test), so once `done` is stripped the chain's existing `green_done_note`/
/// `implement_bare_done` handling takes over exactly as it would for a bare
/// `done` with no `done: true` at all: the step becomes the real handoff to
/// review, action stays `Done`, and the committed patch carries no `done`
/// key.
#[test]
fn implement_done_true_with_green_and_complete_plan_strips_done_and_hands_off_to_review() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Done,
        }],
        ..Default::default()
    });
    cur.harness.green_step = Some(3);
    let content = json!({
        "patch": {
            "done": true,
            "plan": [
                {"id": 1, "status": "done"},
            ],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("must not be rejected -- done:true strips and the chain hands off to review");
    assert!(
        matches!(step.action(), statie_core::Action::Done),
        "expected the action to stay Done (a handoff), got {:?}",
        step.action()
    );
    assert_eq!(
        step.patch().raw().get("done"),
        None,
        "done must have been stripped from the committed patch: {:?}",
        step.patch().raw()
    );
    assert!(
        step.notes()
            .iter()
            .any(|n| n.contains("`done: true` is review's to set")),
        "the not-ours note must be attached: {:?}",
        step.notes()
    );
    let (patch, _action, _) = step.into_parts();
    assert_eq!(
        patch.next().next_skill.as_deref(),
        Some("review"),
        "the chain must still hand off to review once done:true is stripped"
    );
}

/// Same `done: true` strip, but with no passing test and something edited
/// since the last one (`edits_since_test > 0`, the default in `committed`'s
/// `HarnessState`): once `done` is stripped, `implement_no_green_runs_test`
/// takes over exactly as it would for a bare `done` and substitutes a real
/// `test` tool call instead of handing off.
#[test]
fn implement_done_true_without_green_and_edits_since_runs_the_test() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Done,
        }],
        ..Default::default()
    });
    cur.harness.edits_since_test = 1;
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("must not be rejected -- done:true strips and the chain runs the test");
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(
            action,
            statie_core::Action::Tool { ref name, .. } if name == "test"
        ),
        "expected a substituted `test` tool call, got {action:?}"
    );
    assert!(!patch.next().done, "done must not have committed as true");
}

/// A tool call (not `done`) that happens to carry `done: true` in its patch
/// is left as a tool call -- `implement_done_not_ours_strip` only ever
/// strips the key and attaches a note; it never touches `out.action()`, so a
/// non-`done` action the model sent is unaffected.
#[test]
fn implement_tool_call_with_done_true_in_patch_keeps_the_tool_call_and_strips_done() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true, "next": {"tool": "shell", "path": "cargo test", "what": "run it"}},
        "action": {"type": "tool", "name": "shell", "args": {"cmd": "cargo test"}},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(
            action,
            statie_core::Action::Tool { ref name, .. } if name == "shell"
        ),
        "expected the tool call to be kept unchanged, got {action:?}"
    );
    assert!(
        !patch.next().done,
        "done must have been stripped so it never commits"
    );
}

#[test]
fn implement_handoff_to_review_with_all_done_is_accepted() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::Skipped,
            },
        ],
        ..Default::default()
    });
    let content = json!({
        "patch": {"next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    parse_and_validate(&skill, &cur, content)
        .expect("all-done plan should allow handoff to review");
}

/// `done: true` with non-empty `findings` used to be a straight rejection
/// (`ReviewSkill::rules`'s "done can only be true when findings is empty").
/// A live run showed this shape sent twice in a row meaning "send the work
/// back with these findings" -- `review_sendback_note` (`skills/src/coding/
/// mod.rs`) now converts it into that handoff instead: `done` forced back
/// to `false`, `next_skill` injected as `implement` (nothing else was set),
/// action stays `Done`. See `skills/tests/review_sendback.rs` for the
/// session-level version of this same conversion.
#[test]
fn review_done_true_with_findings_converts_to_handoff_instead_of_rejecting() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true, "findings": ["something is wrong"]},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("must not be rejected -- converted to a send-back handoff instead");
    assert!(
        matches!(step.action(), Action::Done),
        "got {:?}; action stays Done so this hands off",
        step.action()
    );
    assert_eq!(
        step.patch().raw().get("done").and_then(|v| v.as_bool()),
        Some(false),
        "`done` must be forced back to false: {:?}",
        step.patch().raw()
    );
    assert_eq!(
        step.patch()
            .raw()
            .get("next_skill")
            .and_then(|v| v.as_str()),
        Some("implement"),
        "next_skill must be injected as implement: {:?}",
        step.patch().raw()
    );
}

#[test]
fn review_done_true_with_empty_findings_is_accepted() {
    // `tool_calls: 1` -- review has looked at something this activation, so
    // the look-before-approve gate (`review_looked_note`) does not fire and
    // this is an ordinary approval. `green_step: Some(0)` (a passing test
    // run) keeps `review_no_green_note` (`skills/src/coding/mod.rs`) from
    // firing instead, since that is not what this test is about.
    let skill = ReviewSkill::new();
    let mut cur = committed_with_tool_calls(CodingWorkspace::default(), 1);
    cur.harness.green_step = Some(0);
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("done with empty findings should be accepted");
    assert!(
        matches!(step.action(), Action::Done),
        "got {:?}; a real approval (tool_calls >= 1) must stay Done",
        step.action()
    );
}

// ---------------------------------------------------------------------
// Review must look before it approves. With `tool_calls: 0`, an approval
// used to convert straight to `Action::Continue` (`review_looked_note`);
// it now instead converts to a substituted `test` tool call
// (`review_blind_approval_runs_test`, `skills/src/coding/mod.rs`) -- the
// harness runs the tests itself rather than just asking for a look, since
// live runs 69/70 showed the ask-first note alone did not work on this
// model. `done` (and `next_skill`) are still stripped from the committed
// patch either way.
// ---------------------------------------------------------------------

#[test]
fn review_done_true_with_no_tool_calls_runs_test_and_strips_done() {
    let skill = ReviewSkill::new();
    let cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("must not be rejected -- the patch is kept, just not the approval");
    assert!(
        matches!(
            step.action(),
            Action::Tool { name, .. } if name == "test"
        ),
        "got {:?}; an approval with tool_calls == 0 must convert to a substituted `test` call",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[
            "you approved without looking at anything, so the harness ran the tests for you \
             (below). Read the changed files if the result is not enough to judge, then \
             approve with `done: true` or send it back with findings."
                .to_string()
        ],
        "unexpected notes: {:?}",
        step.notes()
    );
    // `done` must not be in the raw patch that would be applied/committed.
    assert!(
        step.patch().raw().get("done").is_none(),
        "`done` must be stripped from the committed patch: {:?}",
        step.patch().raw()
    );
}

#[test]
fn review_done_true_with_no_tool_calls_and_no_last_test_cmd_substitutes_auto_detect_args() {
    let skill = ReviewSkill::new();
    let cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    assert_eq!(cur.harness.last_test_cmd, None);
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).unwrap();
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "test");
            assert_eq!(
                args,
                &json!({}),
                "no last_test_cmd -> empty args (auto-detect)"
            );
        }
        other => panic!("got {other:?}; expected a substituted `test` call"),
    }
}

#[test]
fn review_done_true_with_no_tool_calls_and_last_test_cmd_substitutes_that_cmd() {
    let skill = ReviewSkill::new();
    let mut cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    cur.harness.last_test_cmd = Some("cargo test -p x --test y".to_string());
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).unwrap();
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "test");
            assert_eq!(
                args,
                &json!({"cmd": "cargo test -p x --test y"}),
                "with last_test_cmd set, the substituted call must rerun that same command"
            );
        }
        other => panic!("got {other:?}; expected a substituted `test` call"),
    }
}

#[test]
fn review_done_true_with_findings_and_next_skill_and_no_tool_calls_is_unaffected() {
    // A `done` that sends the work back (non-empty findings, next_skill:
    // implement) is not an approval -- `review_looked_note` must not touch
    // it, even with zero tool calls.
    let skill = ReviewSkill::new();
    let cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    let content = json!({
        "patch": {
            "findings": ["something is wrong"],
            "next_skill": "implement",
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("sending work back must not be rejected");
    assert!(
        matches!(step.action(), Action::Done),
        "got {:?}; sending work back must stay Done even with tool_calls == 0",
        step.action()
    );
    assert_eq!(
        step.patch()
            .raw()
            .get("next_skill")
            .and_then(|v| v.as_str()),
        Some("implement"),
        "next_skill must be unaffected"
    );
}

// `review_looked_note`'s own `tool_calls == 0` + approval combination is
// now intercepted first by `review_blind_approval_runs_test` (identical
// guard, called earlier in `ReviewSkill::validate` -- see
// `review_done_true_with_no_tool_calls_runs_test_and_strips_done` above),
// so that path through `ReviewSkill::validate` never reaches
// `review_looked_note` any more. `review_looked_note` itself is unit-tested
// directly here instead, calling it the same way `ReviewSkill::validate`
// does, to keep documenting its note text and stripping behaviour without
// depending on reachability through the full validate chain.
#[test]
fn review_looked_note_says_run_the_unfiltered_tests() {
    // A live run approved after running only a filtered test
    // (`cargo test ... child_events`) that hid a real regression the
    // unfiltered suite would have caught. The look-before-approve note
    // must steer toward the unfiltered run, not just "run `test`".
    let cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let mut parsed =
        ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
            .expect("should parse as valid StepOutput JSON");
    let note = super::review_looked_note(&cur, &mut parsed)
        .expect("tool_calls == 0 and an approval must produce a note");
    assert_eq!(
        note,
        "review has not looked at anything yet. Read the changed files (the `files` list, or \
         `shell` `git diff`) and run the unfiltered tests, then approve with `done: true` or \
         send the work back with findings."
    );
    assert!(
        parsed
            .patch_mut()
            .0
            .as_object()
            .unwrap()
            .get("done")
            .is_none(),
        "`done` must be stripped from the raw patch: {:?}",
        parsed.patch_mut().0
    );
}

#[test]
fn review_instructions_say_to_run_tests_without_a_name_filter() {
    let review_spec = ReviewSkill::new().spec();
    assert!(
        review_spec.instructions().contains(
            "Before approving, run the tests without a name filter (the crate's whole test \
             target); a pass on one filtered test does not cover regressions."
        ),
        "review instructions should tell the model to run the unfiltered tests before \
         approving: {}",
        review_spec.instructions()
    );
}

#[test]
fn duplicate_plan_ids_are_rejected() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "plan": [
                {"id": 1, "text": "a", "status": "pending"},
                {"id": 1, "text": "b", "status": "pending"}
            ]
        },
        "action": {"type": "yield"},
        "ack_inbox": []
    });
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------
// 4. handoff / is_terminal.
// ---------------------------------------------------------------------

#[test]
fn handoff_returns_target_when_set_to_other_skill() {
    let skill = ImplementSkill::new();
    let ws = CodingWorkspace {
        next_skill: Some("review".to_string()),
        ..Default::default()
    };
    assert_eq!(skill.handoff(&ws), Some("review".to_string()));
}

#[test]
fn handoff_returns_none_when_set_to_own_name() {
    let skill = ImplementSkill::new();
    let ws = CodingWorkspace {
        next_skill: Some("implement".to_string()),
        ..Default::default()
    };
    assert_eq!(skill.handoff(&ws), None);
}

#[test]
fn handoff_returns_none_when_unset() {
    let skill = ImplementSkill::new();
    let ws = CodingWorkspace {
        next_skill: None,
        ..Default::default()
    };
    assert_eq!(skill.handoff(&ws), None);
}

#[test]
fn is_terminal_follows_done_flag() {
    let skill = ReviewSkill::new();
    let mut ws = CodingWorkspace::default();
    assert!(!skill.is_terminal(&ws));
    ws.done = true;
    assert!(skill.is_terminal(&ws));
}

/// Only `review` can end the session (statie-spec.md §5/§10): `plan` hands
/// off to `implement` via `next_skill`, it never finishes the task itself.
/// `PlanSkill::is_terminal` is unconditionally `false`, even when `ws.done`
/// is somehow `true` -- unlike `ReviewSkill::is_terminal` above, which
/// tracks the flag.
#[test]
fn plan_is_terminal_is_always_false_even_when_done_is_true() {
    let skill = PlanSkill::new();
    let mut ws = CodingWorkspace::default();
    assert!(!skill.is_terminal(&ws));
    ws.done = true;
    assert!(
        !skill.is_terminal(&ws),
        "plan must never be terminal, whatever ws.done says"
    );
}

/// Same guarantee as `plan_is_terminal_is_always_false_even_when_done_is_true`,
/// for `router`: it only ever hands off via `next_skill`, never finishes the
/// task itself.
#[test]
fn router_is_terminal_is_always_false_even_when_done_is_true() {
    let skill = RouterSkill::new();
    let mut ws = CodingWorkspace::default();
    assert!(!skill.is_terminal(&ws));
    ws.done = true;
    assert!(
        !skill.is_terminal(&ws),
        "router must never be terminal, whatever ws.done says"
    );
}

// ---------------------------------------------------------------------
// 5. allowed_transitions per skill.
// ---------------------------------------------------------------------

#[test]
fn allowed_transitions_exact() {
    assert_eq!(
        RouterSkill::new().allowed_transitions(),
        vec![
            "plan".to_string(),
            "implement".to_string(),
            "review".to_string()
        ]
    );
    assert_eq!(
        PlanSkill::new().allowed_transitions(),
        vec!["implement".to_string(), "router".to_string()]
    );
    assert_eq!(
        ImplementSkill::new().allowed_transitions(),
        vec![
            "review".to_string(),
            "plan".to_string(),
            "router".to_string()
        ]
    );
    assert_eq!(
        ReviewSkill::new().allowed_transitions(),
        vec!["implement".to_string(), "router".to_string()]
    );
}

// ---------------------------------------------------------------------
// Extra: FileNote/Action sanity (fields used somewhere so imports aren't dead).
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// 6. tools() exact lists and instruction text mentions of new tools.
// ---------------------------------------------------------------------

#[test]
fn tools_exact_per_skill() {
    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    assert_eq!(sorted(RouterSkill::new().tools()), Vec::<String>::new());

    assert_eq!(
        sorted(PlanSkill::new().tools()),
        sorted(vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
            "test".to_string(),
            "think".to_string(),
        ])
    );

    assert_eq!(
        sorted(ImplementSkill::new().tools()),
        sorted(vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "revert_file".to_string(),
            "shell".to_string(),
            "test".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
            "think".to_string(),
        ])
    );

    assert_eq!(
        sorted(ReviewSkill::new().tools()),
        sorted(vec![
            "list_files".to_string(),
            "read_file".to_string(),
            "search".to_string(),
            "test".to_string(),
            "recall".to_string(),
            "filter_last_tool_call".to_string(),
            "think".to_string(),
        ])
    );
}

#[test]
fn implement_instructions_mention_edit_file() {
    let spec = ImplementSkill::new().spec();
    assert!(
        spec.instructions().contains("edit_file"),
        "implement instructions should mention edit_file: {}",
        spec.instructions()
    );
}

#[test]
fn implement_instructions_say_to_read_from_reported_offset() {
    let spec = ImplementSkill::new().spec();
    assert!(
        spec.instructions().contains(
            "When a tool reports a line number, read from that offset (`read_file` with `offset`) instead of paging from the top."
        ),
        "implement instructions should tell the model to read from a reported offset: {}",
        spec.instructions()
    );
}

#[test]
fn implement_instructions_mention_anchor_insertion() {
    let spec = ImplementSkill::new().spec();
    assert!(
        spec.instructions().contains(
            "To add code, anchor on one line you can see: `edit_file` with `after` (or `before`) and `new`, instead of a multi-line `old`."
        ),
        "implement instructions should mention anchor-based insertion: {}",
        spec.instructions()
    );
}

#[test]
fn plan_and_review_instructions_mention_search() {
    let plan_spec = PlanSkill::new().spec();
    assert!(
        plan_spec.instructions().contains("search"),
        "plan instructions should mention search: {}",
        plan_spec.instructions()
    );

    let review_spec = ReviewSkill::new().spec();
    assert!(
        review_spec.instructions().contains("search"),
        "review instructions should mention search: {}",
        review_spec.instructions()
    );
}

// ---------------------------------------------------------------------
// 7. Keyed merge-patch semantics (plan steps, file notes) via the skill's
//    public validation path.
// ---------------------------------------------------------------------

#[test]
fn partial_plan_entry_updates_only_that_step() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 3,
                text: "c".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 4,
                text: "d".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    let content = json!({
        "patch": {
            "plan": [{"id": 2, "status": "in_progress"}]
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let validated = parse_and_validate(&skill, &cur, content)
        .expect("a patch entry naming only id and the changed field should be accepted");
    let (patch, _action, _acks) = validated.into_parts();
    assert_eq!(patch.next().plan.len(), 4);
    assert_eq!(
        patch.next().plan,
        vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::InProgress,
            },
            PlanStep {
                id: 3,
                text: "c".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 4,
                text: "d".to_string(),
                status: StepStatus::Pending,
            },
        ]
    );
}

#[test]
fn new_plan_step_is_appended() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Done,
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {
            "plan": [{"id": 2, "text": "b", "status": "pending"}]
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let validated =
        parse_and_validate(&skill, &cur, content).expect("appending a new plan id should work");
    let (patch, _action, _acks) = validated.into_parts();
    assert_eq!(
        patch.next().plan,
        vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::Pending,
            },
        ]
    );
}

#[test]
fn marking_a_step_skipped_keeps_it_and_is_accepted() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 3,
                text: "c".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 4,
                text: "d".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    let content = json!({
        "patch": {
            "plan": [{"id": 2, "status": "skipped"}]
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let validated = parse_and_validate(&skill, &cur, content)
        .expect("sending only the skipped entry should keep the others unchanged and be accepted");
    let (patch, _action, _acks) = validated.into_parts();
    assert_eq!(
        patch.next().plan,
        vec![
            PlanStep {
                id: 1,
                text: "a".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "b".to_string(),
                status: StepStatus::Skipped,
            },
            PlanStep {
                id: 3,
                text: "c".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 4,
                text: "d".to_string(),
                status: StepStatus::Pending,
            },
        ]
    );
}

#[test]
fn partial_files_entry_updates_note() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        files: vec![
            FileNote {
                path: "a.rs".to_string(),
                note: "note a".to_string(),
            },
            FileNote {
                path: "b.rs".to_string(),
                note: "note b".to_string(),
            },
        ],
        ..Default::default()
    });
    let content = json!({
        "patch": {
            "files": [{"path": "b.rs", "note": "updated note b"}]
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let validated = parse_and_validate(&skill, &cur, content)
        .expect("a patch entry naming only path and the changed note should be accepted");
    let (patch, _action, _acks) = validated.into_parts();
    assert_eq!(
        patch.next().files,
        vec![
            FileNote {
                path: "a.rs".to_string(),
                note: "note a".to_string(),
            },
            FileNote {
                path: "b.rs".to_string(),
                note: "updated note b".to_string(),
            },
        ]
    );
}

// ---------------------------------------------------------------------
// 8. `yield` is rejected in every coding skill.
// ---------------------------------------------------------------------

#[test]
fn yield_is_rejected_in_every_coding_skill() {
    for (name, skill) in coding_skills() {
        let cur = committed(CodingWorkspace::default());
        let content = json!({
            "patch": {},
            "action": {"type": "yield"},
            "ack_inbox": []
        });
        let raw = raw(0, content);
        let parsed = ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw)
            .expect("should parse as valid StepOutput JSON");
        let err = skill.validate(&cur, parsed).unwrap_err();
        match err {
            statie_core::PatchError::RuleViolation(msg) => {
                assert!(msg.contains("yield"), "[{name}] {msg}");
                assert!(msg.contains(&name), "[{name}] {msg}");
            }
            other => panic!("[{name}] expected RuleViolation, got {other:?}"),
        }
    }
}

#[test]
fn file_note_round_trips_and_action_tool_parses() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "files": [{"path": "src/lib.rs", "note": "entry point"}]
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let validated = parse_and_validate(&skill, &cur, content).expect("should validate");
    let (patch, action, _acks) = validated.into_parts();
    assert_eq!(
        patch.next().files,
        vec![FileNote {
            path: "src/lib.rs".to_string(),
            note: "entry point".to_string()
        }]
    );
    assert!(matches!(action, Action::Tool { .. }));
}

// ---------------------------------------------------------------------
// 9. Subagent delegation: `allowed_delegates`, instruction paragraph,
//    `absorb_report`, and the `reports`-is-harness-only rule.
// ---------------------------------------------------------------------

fn sample_report(agent: &str) -> Report {
    Report {
        agent: agent.to_string(),
        task: "look into the thing".to_string(),
        outcome: ReportOutcome::Done,
        summary: "did the thing".to_string(),
        findings: vec![],
        files: vec![],
        data: serde_json::Map::new(),
        stats: ReportStats {
            steps: 3,
            server_prompt_tokens: 100,
            server_completion_tokens: 20,
            wall_ms: 500,
        },
    }
}

#[test]
fn coding_skills_have_empty_allowed_delegates_and_unchanged_instructions() {
    for (name, skill) in coding_skills() {
        assert!(
            skill.allowed_delegates().is_empty(),
            "[{name}] expected no delegates"
        );
        let instructions = skill.spec().instructions().to_string();
        assert!(
            !instructions.contains("Subagents:"),
            "[{name}] instructions should not mention Subagents: {instructions}"
        );
    }
}

#[test]
fn coding_skills_with_delegates_lists_them_except_router() {
    let delegates = vec!["ui-review".to_string()];
    let skills = coding_skills_with_delegates(delegates.clone());
    for (name, skill) in &skills {
        if name == "router" {
            assert!(
                skill.allowed_delegates().is_empty(),
                "router should not delegate"
            );
            assert!(
                !skill.spec().instructions().contains("Subagents:"),
                "router instructions should not mention Subagents:"
            );
        } else {
            assert_eq!(
                skill.allowed_delegates(),
                delegates,
                "[{name}] should list the configured delegates"
            );
            let instructions = skill.spec().instructions().to_string();
            assert!(
                instructions.contains("Subagents:"),
                "[{name}] instructions should mention Subagents: {instructions}"
            );
            assert!(
                instructions.contains("ui-review"),
                "[{name}] instructions should name ui-review: {instructions}"
            );
        }
    }
}

#[test]
fn absorb_report_inserts_under_agent_name_and_passes_rules() {
    for (name, skill) in coding_skills() {
        let cur = committed(CodingWorkspace::default());
        let mut next = cur.agent.clone();
        let report = sample_report("ui-review");
        skill
            .absorb_report(&mut next, report.clone())
            .unwrap_or_else(|e| panic!("[{name}] absorb_report failed: {e:?}"));
        assert_eq!(
            next.reports.get("ui-review").map(|r| r.summary.clone()),
            Some(report.summary.clone()),
            "[{name}] report should be stored under its agent name"
        );
        skill
            .rules(&cur, &next)
            .unwrap_or_else(|e| panic!("[{name}] rules rejected the absorbed report: {e:?}"));
    }
}

#[test]
fn re_delegating_overwrites_the_same_key_and_still_passes_rules() {
    let skill = ImplementSkill::new();
    let mut ws = CodingWorkspace::default();
    skill
        .absorb_report(&mut ws, sample_report("ui-review"))
        .expect("first absorb");
    let cur = committed(ws.clone());
    let mut second = ws.clone();
    let mut updated = sample_report("ui-review");
    updated.summary = "did the thing again, differently".to_string();
    skill
        .absorb_report(&mut second, updated.clone())
        .expect("second absorb");
    skill
        .rules(&cur, &second)
        .expect("overwriting via absorb_report should still pass rules");
    assert_eq!(
        second.reports.get("ui-review").map(|r| r.summary.clone()),
        Some(updated.summary)
    );
}

/// `reports` is harness-owned (see `common_harness_owned_fields`), so a
/// model patch touching it goes through `strip_harness_owned` inside each
/// coding skill's `validate` override before `validate_default`'s owned-
/// field check ever sees it: the key is dropped from the raw patch, a note
/// is recorded, and the rest of the patch (here, the tool call) proceeds --
/// the step is no longer rejected. Direct calls to `validate_default`/
/// `rules` (bypassing the `validate` override) still see the raw owned-
/// field rejection; that mechanism is exercised in `core/tests/
/// parse_validate.rs` and is unchanged by this test.
#[test]
fn model_patch_adding_bogus_empty_report_is_stripped_and_noted() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {
            "reports": {
                "ui-review": {
                    "agent": "ui-review",
                    "task": "",
                    "outcome": "done",
                    "summary": "",
                    "findings": [],
                    "files": [],
                    "data": {},
                    "stats": {
                        "steps": 0,
                        "server_prompt_tokens": 0,
                        "server_completion_tokens": 0,
                        "wall_ms": 0
                    }
                }
            }
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");
    assert_eq!(
        step.patch().next().reports.len(),
        0,
        "the bogus report must not land"
    );
    assert_eq!(
        step.notes(),
        &[
            "`reports` is kept by the harness; that part of your patch was dropped and the rest \
           was kept."
                .to_string()
        ]
    );
}

#[test]
fn model_patch_deleting_existing_report_is_stripped_and_noted() {
    // `reports` is harness-owned (see `common_harness_owned_fields`), so a
    // model patch that removes an existing entry is caught by
    // `strip_harness_owned` before `validate_default`'s owned-field check
    // ever runs -- the whole `reports` key is dropped from the raw patch,
    // leaving the committed reports untouched, rather than rejecting the
    // step.
    let skill = ImplementSkill::new();
    let mut ws = CodingWorkspace::default();
    skill
        .absorb_report(&mut ws, sample_report("ui-review"))
        .expect("absorb");
    let cur = committed(ws);

    // A JSON merge patch deletes an object's key by setting it to `null`
    // (RFC 7396); `reports` is a map merged key-by-key, so
    // `{"reports": {"ui-review": null}}` deletes just that entry -- exactly
    // what "the model's patch produced a next state with the report
    // removed" looks like on the wire.
    let content = json!({
        "patch": {"reports": {"ui-review": null}},
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");
    assert_eq!(
        step.patch()
            .next()
            .reports
            .get("ui-review")
            .map(|r| r.summary.clone()),
        Some(sample_report("ui-review").summary),
        "the existing report must survive unchanged"
    );
    assert_eq!(
        step.notes(),
        &[
            "`reports` is kept by the harness; that part of your patch was dropped and the rest \
           was kept."
                .to_string()
        ]
    );
}

#[test]
fn model_patch_altering_existing_report_is_stripped_and_noted() {
    // Any change to `reports` -- fabricated, legitimate-looking, or a
    // removal -- is stripped from the raw patch by `strip_harness_owned`
    // before `validate_default`'s owned-field check ever runs, rather than
    // trying to distinguish "looks fabricated" from "looks real". Send a
    // patch that overwrites the existing report's `summary` alongside an
    // unrelated tool call.
    let skill = ImplementSkill::new();
    let mut ws = CodingWorkspace::default();
    skill
        .absorb_report(&mut ws, sample_report("ui-review"))
        .expect("absorb");
    let cur = committed(ws);

    let content = json!({
        "patch": {
            "reports": {
                "ui-review": {
                    "agent": "ui-review",
                    "task": "look into the thing",
                    "outcome": "done",
                    "summary": "a model wrote this",
                    "findings": [],
                    "files": [],
                    "data": {},
                    "stats": {
                        "steps": 3,
                        "server_prompt_tokens": 100,
                        "server_completion_tokens": 20,
                        "wall_ms": 500
                    }
                }
            }
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");
    assert_eq!(
        step.patch()
            .next()
            .reports
            .get("ui-review")
            .map(|r| r.summary.clone()),
        Some(sample_report("ui-review").summary),
        "the existing report must be left exactly as committed, not overwritten"
    );
    assert_eq!(
        step.notes(),
        &[
            "`reports` is kept by the harness; that part of your patch was dropped and the rest \
           was kept."
                .to_string()
        ]
    );
}

#[test]
fn absorbed_reports_pass_rules_directly_now_that_rules_does_not_inspect_reports() {
    // `rules` (called directly, bypassing `validate_default`'s owned-field
    // check) no longer has any opinion on `reports` -- that check moved
    // entirely to `validate_default`. This documents the new division of
    // labor: `absorb_report`'s harness-trusted writes go through `rules`
    // alone (see `session.rs`'s delegate-absorb path) and must keep passing
    // it; only the model path (`validate_default`) gates `reports` at all
    // now.
    let skill = ImplementSkill::new();
    let mut ws = CodingWorkspace::default();
    skill
        .absorb_report(&mut ws, sample_report("ui-review"))
        .expect("absorb");
    let cur = committed(CodingWorkspace::default());
    skill
        .rules(&cur, &ws)
        .expect("rules alone no longer rejects a `reports` change of any shape");
}

#[test]
fn coding_workspace_json_without_reports_still_deserializes() {
    let json = serde_json::json!({
        "task": "fix the bug",
        "plan": [],
        "files": [],
        "findings": [],
        "recent_summary": "",
        "next_skill": null,
        "done": false
    });
    let ws: CodingWorkspace =
        serde_json::from_value(json).expect("old sessions with no reports field should still load");
    assert!(ws.reports.is_empty());
}

#[test]
fn plan_step_without_status_parses_as_pending() {
    let step: PlanStep = serde_json::from_value(serde_json::json!({"id": 1, "text": "run it"}))
        .expect("status is optional in a patch");
    assert_eq!(step.status, StepStatus::Pending);
}

#[test]
fn commitment_without_what_parses_and_is_stripped_with_a_note_no_open_commitment() {
    let c: statie_core::Commitment =
        serde_json::from_value(serde_json::json!({"tool": "edit_file", "path": "a.rs"}))
            .expect("what is optional at parse time");
    assert_eq!(c.what, "");

    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next": {"tool": c.tool, "path": c.path, "what": c.what}},
        "action": {"type": "ask_user", "message": "confirm?"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("empty `next.what` must be stripped and noted, not rejected");
    assert!(
        step.notes()
            .iter()
            .any(|n| n.contains("`next` ignored") && n.contains("`next.what` must say"))
    );
    assert!(
        step.notes()
            .iter()
            .any(|n| n.contains("Reads need no `next`; set it when you know the change")),
        "with no open commitment the note should point at setting `next` when ready: {:?}",
        step.notes()
    );
    assert_eq!(
        step.patch().next().next,
        None,
        "the committed `next` must be left as it was (null)"
    );
}

#[test]
fn commitment_without_what_over_an_open_commitment_is_stripped_and_leaves_it_unchanged() {
    let open = statie_core::Commitment {
        tool: "edit_file".to_string(),
        path: Some("b.rs".to_string()),
        what: "fix the bug".to_string(),
    };
    let cur = committed(CodingWorkspace {
        next: Some(open.clone()),
        ..Default::default()
    });

    let skill = ImplementSkill::new();
    let content = json!({
        "patch": {"next": {"tool": "edit_file", "path": "a.rs", "what": "   "}},
        "action": {"type": "ask_user", "message": "confirm?"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("empty `next.what` must be stripped and noted, not rejected");
    assert!(
        step.notes().iter().any(|n| n.contains("`next` ignored")
            && n.contains("`next.what` must say")
            && n.contains("Your open commitment is unchanged")),
        "{:?}",
        step.notes()
    );
    assert_eq!(
        step.patch().next().next,
        Some(open),
        "the committed `next` must be left exactly as it was"
    );
}

#[test]
fn commitment_missing_path_for_edit_file_is_stripped_with_a_note() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next": {"tool": "edit_file", "what": "replace fn old with fn new"}},
        "action": {"type": "ask_user", "message": "confirm?"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("missing `next.path` must be stripped and noted, not rejected");
    assert!(
        step.notes().iter().any(|n| {
            n.contains("`next` ignored")
                && n.contains("`next.path` is required when `next.tool` is `edit_file`")
        }),
        "{:?}",
        step.notes()
    );
    assert_eq!(
        step.patch().next().next,
        None,
        "the committed `next` must be left as it was (null)"
    );
}

#[test]
fn plan_skill_can_run_tests_as_its_instructions_say() {
    // The plan instructions tell the model to make its first step "run
    // `test` to record the baseline"; the tool must be active for that.
    let plan = PlanSkill::new();
    assert!(
        plan.tools().iter().any(|t| t == "test"),
        "{:?}",
        plan.tools()
    );
    assert!(
        !plan
            .tools()
            .iter()
            .any(|t| t == "shell" || t == "edit_file" || t == "write_file")
    );
}

// =======================================================================
// `seen` (milestone 10): range merge, output parsers, `seen_entry`
// eviction, `common_after_tool`, and the harness-owned refusal.
// =======================================================================

#[test]
fn merge_range_coalesces_adjacent_ranges() {
    let mut r = vec![[1, 10]];
    merge_range(&mut r, [11, 20]);
    assert_eq!(r, vec![[1, 20]]);
}

#[test]
fn merge_range_coalesces_overlapping_ranges() {
    let mut r = vec![[1, 10]];
    merge_range(&mut r, [5, 15]);
    assert_eq!(r, vec![[1, 15]]);
}

#[test]
fn merge_range_keeps_disjoint_ranges_separate() {
    let mut r = vec![[1, 10]];
    merge_range(&mut r, [20, 30]);
    assert_eq!(r, vec![[1, 10], [20, 30]]);
}

#[test]
fn merge_range_tolerates_unsorted_input_and_sorts_the_result() {
    let mut r = vec![[20, 30], [1, 10]];
    merge_range(&mut r, [11, 19]);
    assert_eq!(r, vec![[1, 30]]);
}

#[test]
fn merge_range_new_range_fully_inside_an_existing_one_is_a_noop() {
    let mut r = vec![[1, 100]];
    merge_range(&mut r, [40, 60]);
    assert_eq!(r, vec![[1, 100]]);
}

#[test]
fn parse_edit_header_reads_replaced_range() {
    let text = "edited a.rs: replaced 1 occurrence (lines 10-12)\n   10 | x\n";
    assert_eq!(parse_edit_header(text), Some([10, 12]));
}

#[test]
fn parse_edit_header_reads_deleted_range() {
    let text = "edited a.rs: deleted 3 line(s) (lines 5-7)\n";
    assert_eq!(parse_edit_header(text), Some([5, 7]));
}

#[test]
fn parse_edit_header_derives_range_for_insert_after() {
    let text = "edited a.rs: inserted 2 line(s) after line 4\n";
    // Two lines inserted right after line 4 land at 5-6.
    assert_eq!(parse_edit_header(text), Some([5, 6]));
}

#[test]
fn parse_edit_header_derives_range_for_insert_before() {
    let text = "edited a.rs: inserted 1 line(s) before line 2\n";
    // One line inserted right before line 2 lands at 1-1.
    assert_eq!(parse_edit_header(text), Some([1, 1]));
}

#[test]
fn parse_edit_header_tolerates_a_leading_note_prefix() {
    let text = "note: matched anchor ignoring whitespace differences; edited a.rs: inserted 1 line(s) after line 4\n";
    assert_eq!(parse_edit_header(text), Some([5, 5]));
}

#[test]
fn parse_edit_header_rejects_unparsable_text() {
    assert_eq!(parse_edit_header(""), None);
    assert_eq!(
        parse_edit_header("(tool `edit_file` returned no output)"),
        None
    );
    assert_eq!(parse_edit_header("wrote 12 bytes to a.rs"), None);
}

#[test]
fn parse_revert_lines_reads_the_line_count() {
    let text = "reverted a.rs to its content from before this session's first edit (42 lines)";
    assert_eq!(parse_revert_lines(text), Some(42));
}

#[test]
fn parse_revert_lines_singular_line() {
    let text = "reverted a.rs to its content from before this session's first edit (1 line)";
    assert_eq!(parse_revert_lines(text), Some(1));
}

#[test]
fn parse_revert_lines_none_for_the_removed_shape() {
    let text = "removed a.rs; it did not exist before this session";
    assert_eq!(parse_revert_lines(text), None);
}

#[test]
fn count_lines_matches_str_lines_semantics() {
    assert_eq!(count_lines(""), 0);
    assert_eq!(count_lines("a\nb\nc\n"), 3);
    assert_eq!(count_lines("a\nb\nc"), 3);
    assert_eq!(count_lines("a\n\n"), 2);
}

#[test]
fn seen_entry_evicts_the_smallest_step_when_full() {
    let mut seen: Vec<SeenFile> = (0..12u64)
        .map(|i| SeenFile {
            path: format!("f{i}.rs"),
            step: i,
            ..Default::default()
        })
        .collect();
    // f3.rs (step 3) is the smallest step.
    seen[3].step = 0;
    for (i, f) in seen.iter_mut().enumerate() {
        if i != 3 {
            f.step = (i as u64) + 10;
        }
    }
    assert_eq!(seen.len(), 12);
    let _ = seen_entry(&mut seen, "new.rs");
    assert_eq!(seen.len(), 12, "cap must not be exceeded");
    assert!(
        !seen.iter().any(|f| f.path == "f3.rs"),
        "the smallest-step entry should have been evicted: {seen:?}"
    );
    assert!(seen.iter().any(|f| f.path == "new.rs"));
}

#[test]
fn seen_entry_reuses_an_existing_path_without_evicting() {
    let mut seen: Vec<SeenFile> = (0..12u64)
        .map(|i| SeenFile {
            path: format!("f{i}.rs"),
            step: i,
            ..Default::default()
        })
        .collect();
    let entry = seen_entry(&mut seen, "f5.rs");
    entry.lines = 99;
    assert_eq!(seen.len(), 12);
    assert_eq!(
        seen.iter().find(|f| f.path == "f5.rs").map(|f| f.lines),
        Some(99)
    );
}

#[test]
fn after_tool_read_file_records_range_and_total() {
    let ws = CodingWorkspace::default();
    let output = "src/a.rs (250 lines, showing 1-100)\n";
    let patch = common_after_tool(
        &ws,
        7,
        "read_file",
        &json!({"path": "src/a.rs"}),
        output,
        true,
        true,
    )
    .expect("read_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 100]], "edited": [], "step": 7}]})
    );
}

#[test]
fn after_tool_read_file_second_read_extends_the_range() {
    let mut ws = CodingWorkspace::default();
    ws.seen.push(SeenFile {
        path: "src/a.rs".to_string(),
        lines: 250,
        read: vec![[1, 100]],
        edited: vec![],
        step: 7,
    });
    let output = "src/a.rs (250 lines, showing 101-200)\n";
    let patch = common_after_tool(
        &ws,
        9,
        "read_file",
        &json!({"path": "src/a.rs"}),
        output,
        true,
        true,
    )
    .expect("second read should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 200]], "edited": [], "step": 9}]})
    );
}

#[test]
fn after_tool_edit_file_records_edited_range() {
    let mut ws = CodingWorkspace::default();
    ws.seen.push(SeenFile {
        path: "src/a.rs".to_string(),
        lines: 250,
        read: vec![[1, 200]],
        edited: vec![],
        step: 9,
    });
    let output = "edited src/a.rs: replaced 1 occurrence (lines 150-156)\n";
    let patch = common_after_tool(
        &ws,
        10,
        "edit_file",
        &json!({"path": "src/a.rs"}),
        output,
        true,
        true,
    )
    .expect("edit_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 200]], "edited": [[150, 156]], "step": 10}]})
    );
}

#[test]
fn after_tool_write_file_replaces_the_entry() {
    let ws = CodingWorkspace::default();
    let content = (1..=12)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let patch = common_after_tool(
        &ws,
        3,
        "write_file",
        &json!({"path": "new.rs", "content": content}),
        "wrote 42 bytes to new.rs",
        true,
        true,
    )
    .expect("write_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "new.rs", "lines": 12, "read": [], "edited": [[1, 12]], "step": 3}]})
    );
}

#[test]
fn after_tool_revert_file_clears_both_ranges() {
    let mut ws = CodingWorkspace::default();
    ws.seen.push(SeenFile {
        path: "src/a.rs".to_string(),
        lines: 250,
        read: vec![[1, 200]],
        edited: vec![[150, 156]],
        step: 10,
    });
    let output =
        "reverted src/a.rs to its content from before this session's first edit (250 lines)";
    let patch = common_after_tool(
        &ws,
        11,
        "revert_file",
        &json!({"path": "src/a.rs"}),
        output,
        true,
        true,
    )
    .expect("revert_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [], "edited": [], "step": 11}]})
    );
}

#[test]
fn after_tool_revert_file_removed_drops_the_entry() {
    let mut ws = CodingWorkspace::default();
    ws.seen.push(SeenFile {
        path: "new.rs".to_string(),
        lines: 12,
        read: vec![],
        edited: vec![[1, 12]],
        step: 3,
    });
    let output = "removed new.rs; it did not exist before this session";
    let patch = common_after_tool(
        &ws,
        4,
        "revert_file",
        &json!({"path": "new.rs"}),
        output,
        true,
        true,
    )
    .expect("revert_file (removed) should still produce a seen patch");
    assert_eq!(patch, json!({"seen": []}));
}

#[test]
fn after_tool_other_tools_produce_no_patch() {
    let ws = CodingWorkspace::default();
    for name in [
        "search",
        "list_files",
        "shell",
        "test",
        "recall",
        "filter_last_tool_call",
        "think",
    ] {
        assert_eq!(
            common_after_tool(&ws, 1, name, &json!({}), "irrelevant output", true, true),
            None,
            "tool `{name}` should never produce a patch for unrelated output"
        );
    }
}

// =======================================================================
// `guards.seen_map` off (BACKLOG.md item 2, `core/src/guards.rs`):
// `common_after_tool`'s `seen_enabled` parameter. With it `false`, every
// tool that would otherwise write a `seen` patch (`read_file`/`edit_file`/
// `write_file`/`revert_file`) returns `None` instead -- the field is never
// written and stays whatever it already was (empty, for a session that
// never had the switch on).
// =======================================================================

#[test]
fn after_tool_seen_disabled_read_file_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "src/a.rs (250 lines, showing 1-100)\n";
    assert_eq!(
        common_after_tool(
            &ws,
            7,
            "read_file",
            &json!({"path": "src/a.rs"}),
            output,
            false,
            true
        ),
        None,
        "seen_enabled = false must suppress the seen patch a read_file would otherwise produce"
    );
}

#[test]
fn after_tool_seen_disabled_edit_file_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "edited src/a.rs: replaced 1 occurrence (lines 150-156)\n";
    assert_eq!(
        common_after_tool(
            &ws,
            10,
            "edit_file",
            &json!({"path": "src/a.rs"}),
            output,
            false,
            true
        ),
        None
    );
}

#[test]
fn after_tool_seen_disabled_write_file_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let content = (1..=12)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(
        common_after_tool(
            &ws,
            3,
            "write_file",
            &json!({"path": "new.rs", "content": content}),
            "wrote 42 bytes to new.rs",
            false,
            true,
        ),
        None
    );
}

#[test]
fn after_tool_seen_disabled_revert_file_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output =
        "reverted src/a.rs to its content from before this session's first edit (250 lines)";
    assert_eq!(
        common_after_tool(
            &ws,
            11,
            "revert_file",
            &json!({"path": "src/a.rs"}),
            output,
            false,
            true
        ),
        None
    );
}

#[test]
fn after_tool_seen_disabled_does_not_affect_decision() {
    // `seen_enabled = false` must not spill over into the (separate)
    // `decision_field` switch: a `think` call with `decision_enabled = true`
    // still sets `decision` even when `seen_enabled` is `false`.
    let ws = CodingWorkspace::default();
    let output = "question: why does the test fail?\nanswer:\nthe mock is stale.";
    let patch = common_after_tool(
        &ws,
        7,
        "think",
        &json!({"question": "why does the test fail?"}),
        output,
        false,
        true,
    )
    .expect("decision_enabled = true must still set decision when seen_enabled is false");
    assert_eq!(
        patch,
        json!({"decision": {"question": "why does the test fail?", "answer": "the mock is stale.", "step": 7}})
    );
}

// =======================================================================
// `common_after_tool`'s `think`/`test` handling of `CodingWorkspace::
// decision` (see `skills/tests/decision.rs`'s doc for why the `think`
// shapes are unit-tested here rather than driven through a real `Session`:
// `core::Session::execute` special-cases `Action::Tool { name: "think",
// .. }` before dispatch and never calls `Skill::after_tool` for it, so only
// a direct call to the private `common_after_tool` can exercise them).
// =======================================================================

#[test]
fn after_tool_think_output_sets_decision() {
    let ws = CodingWorkspace::default();
    let output = "question: why does the test fail?\nanswer:\nthe mock is stale.";
    let patch = common_after_tool(
        &ws,
        7,
        "think",
        &json!({"question": "why does the test fail?"}),
        output,
        true,
        true,
    )
    .expect("a think conclusion should produce a decision patch");
    assert_eq!(
        patch,
        json!({"decision": {"question": "why does the test fail?", "answer": "the mock is stale.", "step": 7}})
    );
}

#[test]
fn after_tool_think_output_over_1200_chars_is_truncated() {
    let ws = CodingWorkspace::default();
    let long_answer = "x".repeat(1300);
    let output = format!("question: what now?\nanswer:\n{long_answer}");
    let patch = common_after_tool(
        &ws,
        3,
        "think",
        &json!({"question": "what now?"}),
        &output,
        true,
        true,
    )
    .expect("a think conclusion should produce a decision patch");
    let answer = patch["decision"]["answer"]
        .as_str()
        .expect("answer should be a string");
    assert_eq!(
        answer.chars().count(),
        1200 + " …[truncated]".chars().count()
    );
    assert!(answer.starts_with(&"x".repeat(1200)));
    assert!(answer.ends_with(" …[truncated]"));
}

#[test]
fn after_tool_think_not_configured_output_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "`think` is not configured: set a `thinker` section in the config (see \
                  README, Config). Decide from what you have.";
    assert_eq!(
        common_after_tool(
            &ws,
            1,
            "think",
            &json!({"question": "q"}),
            output,
            true,
            true
        ),
        None,
        "a not-configured think output must leave an existing decision unchanged (no patch)"
    );
}

#[test]
fn after_tool_think_failed_output_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "think failed: subagent timed out";
    assert_eq!(
        common_after_tool(
            &ws,
            1,
            "think",
            &json!({"question": "q"}),
            output,
            true,
            true
        ),
        None,
        "a think-failed output must leave an existing decision unchanged (no patch)"
    );
}

#[test]
fn after_tool_test_pass_clears_an_existing_decision() {
    let ws = CodingWorkspace {
        decision: Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        ..Default::default()
    };
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    let patch = common_after_tool(&ws, 5, "test", &json!({}), output, true, true)
        .expect("a passing test should produce a clearing patch");
    assert_eq!(patch, json!({"decision": null}));
}

#[test]
fn after_tool_test_fail_leaves_an_existing_decision_alone() {
    let ws = CodingWorkspace {
        decision: Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        ..Default::default()
    };
    let output = "command: cargo test\nresult: fail (exit 1)\n";
    assert_eq!(
        common_after_tool(&ws, 5, "test", &json!({}), output, true, true),
        None,
        "a failing test must leave an existing decision alone (no patch)"
    );
}

#[test]
fn after_tool_test_pass_with_no_existing_decision_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    assert_eq!(
        common_after_tool(&ws, 5, "test", &json!({}), output, true, true),
        None,
        "there is nothing to clear when decision is already unset"
    );
}

// =======================================================================
// `guards.decision_field` off (BACKLOG.md item 2, `core/src/guards.rs`):
// `common_after_tool`'s `decision_enabled` parameter. With it `false`, a
// `think` output never sets `decision`, and a passing `test` with an
// existing decision never clears it -- both return `None`. The `think`
// tool itself is unaffected by this switch: it still runs and its answer
// still reaches the model as that call's own observation, only the second,
// persistent copy in `decision` is skipped.
// =======================================================================

#[test]
fn after_tool_decision_disabled_think_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "question: why does the test fail?\nanswer:\nthe mock is stale.";
    assert_eq!(
        common_after_tool(
            &ws,
            7,
            "think",
            &json!({"question": "why does the test fail?"}),
            output,
            true,
            false,
        ),
        None,
        "decision_enabled = false must suppress the decision patch a think output would \
         otherwise produce"
    );
}

#[test]
fn after_tool_decision_disabled_passing_test_with_existing_decision_produces_no_patch() {
    let ws = CodingWorkspace {
        decision: Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        ..Default::default()
    };
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    assert_eq!(
        common_after_tool(&ws, 5, "test", &json!({}), output, true, false),
        None,
        "decision_enabled = false must leave an existing decision alone even on a passing test"
    );
}

#[test]
fn after_tool_decision_disabled_does_not_affect_seen() {
    // `decision_enabled = false` must not spill over into the (separate)
    // `seen_map` switch: a `read_file` call with `seen_enabled = true`
    // still records `seen` even when `decision_enabled` is `false`.
    let ws = CodingWorkspace::default();
    let output = "src/a.rs (250 lines, showing 1-100)\n";
    let patch = common_after_tool(
        &ws,
        7,
        "read_file",
        &json!({"path": "src/a.rs"}),
        output,
        true,
        false,
    )
    .expect("seen_enabled = true must still record seen when decision_enabled is false");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 100]], "edited": [], "step": 7}]})
    );
}

#[test]
fn model_patch_touching_decision_is_stripped_and_noted() {
    let skill = ImplementSkill::new();
    let cur_ws = CodingWorkspace::default();
    let cur = committed(cur_ws.clone());

    let content = json!({
        "patch": {"decision": {"question": "q", "answer": "a", "step": 5}},
        "action": {"type": "ask_user", "message": "?"},
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");

    assert_eq!(
        step.patch().next().decision,
        None,
        "`decision` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        step.notes(),
        &[
            "`decision` is kept by the harness; that part of your patch was dropped and the \
             rest was kept."
                .to_string()
        ]
    );
}

#[test]
fn model_patch_touching_seen_is_stripped_and_noted() {
    let skill = ImplementSkill::new();
    let mut cur_ws = CodingWorkspace::default();
    cur_ws.seen.push(SeenFile {
        path: "a.rs".to_string(),
        lines: 10,
        read: vec![[1, 10]],
        edited: vec![],
        step: 1,
    });
    let cur = committed(cur_ws.clone());
    // A non-empty replacement (an empty-array patch to a keyed array is a
    // structural no-op under `merge_keyed` -- see `patch/src/merge.rs` --
    // so it wouldn't exercise the strip at all; a real attempt to rewrite
    // the field must carry content, the same way the `reports` tests above
    // use a real report object rather than `{}`).
    let content = json!({
        "patch": {"seen": [{"path": "b.rs", "lines": 1, "read": [], "edited": [], "step": 99}]},
        "action": {"type": "tool", "name": "list_files", "args": {}},
        "ack_inbox": []
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");
    assert_eq!(
        step.patch().next().seen,
        cur_ws.seen,
        "`seen` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        step.notes(),
        &[
            "`seen` is kept by the harness; that part of your patch was dropped and the rest \
           was kept."
                .to_string()
        ]
    );
}

// ---------------------------------------------------------------------
// `has_named_handoff` (the self-handoff helper, `skills/src/coding/
// mod.rs`): a stale `next_skill` left over from the handoff that landed
// THIS activation (normally cleared by `Skill::on_activate`, but this
// helper is the second line of defense) must not be misread as "a handoff
// is already named" by a bare-done conversion like `implement_bare_done`.
// ---------------------------------------------------------------------

/// A merged workspace whose `next_skill` names a DIFFERENT skill is a real
/// handoff: `true`, and the raw patch (if it carried `next_skill`) is left
/// untouched.
#[test]
fn has_named_handoff_true_for_a_different_target() {
    let merged = CodingWorkspace {
        next_skill: Some("review".to_string()),
        ..Default::default()
    };
    let content = json!({
        "patch": {"next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let mut out =
        ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
            .unwrap();

    assert!(has_named_handoff("implement", &merged, &mut out));
    assert_eq!(
        out.patch().0.get("next_skill").and_then(|v| v.as_str()),
        Some("review"),
        "a real handoff target must not be stripped from the raw patch"
    );
}

/// A merged workspace with no `next_skill` at all: `false`, raw patch
/// untouched (it has nothing to strip).
#[test]
fn has_named_handoff_false_for_none() {
    let merged = CodingWorkspace::default();
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let mut out =
        ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
            .unwrap();

    assert!(!has_named_handoff("implement", &merged, &mut out));
    assert!(out.patch().0.get("next_skill").is_none());
}

/// A merged workspace whose `next_skill` names THIS skill itself (the stale
/// self-handoff shape this helper exists for): `false`, and if the raw
/// patch also carried that same self-referential value, it is stripped so
/// the step doesn't commit it back.
#[test]
fn has_named_handoff_false_and_strips_a_stale_self_reference() {
    let merged = CodingWorkspace {
        next_skill: Some("implement".to_string()),
        ..Default::default()
    };
    // The model's own patch this step didn't touch `next_skill` at all --
    // the stale value is purely inherited from the committed workspace
    // (the shape `Skill::on_activate` is meant to have already cleared).
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let mut out =
        ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
            .unwrap();

    assert!(!has_named_handoff("implement", &merged, &mut out));
    assert!(
        out.patch().0.get("next_skill").is_none(),
        "nothing to strip: the raw patch never carried next_skill"
    );
}

/// Same self-referential case, but the raw patch itself also carries the
/// stale value (e.g. the model echoed it back) -- it must be stripped so
/// the step's commit doesn't write it again.
#[test]
fn has_named_handoff_strips_self_reference_carried_in_the_raw_patch() {
    let merged = CodingWorkspace {
        next_skill: Some("implement".to_string()),
        ..Default::default()
    };
    let content = json!({
        "patch": {"next_skill": "implement"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let mut out =
        ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
            .unwrap();

    assert!(!has_named_handoff("implement", &merged, &mut out));
    assert!(
        out.patch().0.get("next_skill").is_none(),
        "the self-referential next_skill must be stripped from the raw patch"
    );
}

/// End-to-end at the `implement_bare_done`/`ImplementSkill::validate` level
/// (not just the helper in isolation): a bare `done` with verification
/// green, plan complete, and a STALE self-referential `next_skill:
/// "implement"` already committed in the workspace (the shape a missing
/// `Skill::on_activate` clear would leave behind) must still be converted
/// to the handoff to review -- not stood aside because `next_skill` looked
/// "already set".
#[test]
fn implement_bare_done_with_stale_self_next_skill_still_hands_off_to_review() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "a".to_string(),
            status: StepStatus::Done,
        }],
        // The stale value `on_activate` should have cleared.
        next_skill: Some("implement".to_string()),
        ..Default::default()
    });
    cur.harness.green_step = Some(0);
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("a stale self-handoff must not block the bare-done conversion");
    assert!(matches!(step.action(), statie_core::Action::Done));
    assert_eq!(
        step.patch().next().next_skill.as_deref(),
        Some("review"),
        "must hand off to review, not stay stuck on the stale self-reference"
    );
}

// ---------------------------------------------------------------------
// `implement_repeat_failure_thinks` (`skills/src/coding/mod.rs`): after a
// `test` call's failure has recurred identically, the harness substitutes a
// `think` call for the model's next edit, once per failure streak.
// ---------------------------------------------------------------------

/// The exact note `implement_repeat_failure_thinks` attaches when it
/// substitutes `think` for an edit right after an identical failure.
const REPEAT_THINK_NOTE: &str = "your last edit did not change the test failure, so the harness \
     asked the reasoning subagent first (its conclusion is below, and in `decision`). Act on it: \
     if it names a different cause, fix that; if it agrees with you, make your edit.";

/// The exact note `implement_repeat_failure_thinks` attaches when it
/// substitutes `think` for an idle `test` rerun (nothing edited since).
const IDLE_TEST_THINK_NOTE: &str = "you re-ran the test with nothing edited, so the harness \
     asked the reasoning subagent instead (its conclusion is below, and in `decision`). Act on \
     it.";

/// The exact note `implement_repeat_failure_thinks` attaches when it
/// substitutes `think` for a non-question `ask_user` with a repeat open.
const ASK_USER_THINK_NOTE: &str = "you had nothing to ask and a repeated test failure is open, \
     so the harness asked the reasoning subagent (its conclusion is below, and in `decision`). \
     Act on it.";

fn edit_file_turn_content() -> serde_json::Value {
    json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "edit_file",
            "args": {"path": "a.rs", "old": "x", "new": "y"},
        },
        "ack_inbox": []
    })
}

fn write_file_turn_content() -> serde_json::Value {
    json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "write_file",
            "args": {"path": "a.rs", "content": "y"},
        },
        "ack_inbox": []
    })
}

#[test]
fn repeat_failure_thinks_fires_for_edit_file_with_no_prior_think() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let step = parse_and_validate(&skill, &cur, edit_file_turn_content())
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "think"
                    && args == &json!({"question": super::REPEAT_FAILURE_QUESTION})
        ),
        "expected a substituted `think` call with REPEAT_FAILURE_QUESTION, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[REPEAT_THINK_NOTE.to_string()],
        "the exact repeat-think note must be attached"
    );
    // The patch (empty here) survives untouched.
    let (patch, _action, _acks) = step.into_parts();
    assert_eq!(patch.next().recent_summary, "");
}

#[test]
fn repeat_failure_thinks_fires_for_write_file_with_no_prior_think() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let step = parse_and_validate(&skill, &cur, write_file_turn_content())
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "think"
                    && args == &json!({"question": super::REPEAT_FAILURE_QUESTION})
        ),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
    assert_eq!(step.notes(), &[REPEAT_THINK_NOTE.to_string()]);
}

#[test]
fn repeat_failure_thinks_fires_when_last_think_predates_the_repeat() {
    let skill = ImplementSkill::new();
    // An edit after the last think call (step 5): the trigger only re-arms
    // once something has actually been edited since that call.
    let agent = CodingWorkspace {
        seen: vec![SeenFile {
            path: "a.rs".to_string(),
            edited: vec![[1, 3]],
            step: 7,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cur = committed(agent);
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = Some(5);

    let step = parse_and_validate(&skill, &cur, edit_file_turn_content())
        .expect("a think call before the repeat must not block the substitution");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "think"),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
}

/// The trigger's re-arm gate (added after a second live battery showed the
/// original edit-since-last-think-agnostic version fire on every stuck
/// repeat, running the reasoning subagent five times with nothing new to say
/// -- all five came back empty): `last_think_step: Some(5)`,
/// `repeated_failure_step: Some(10)`, with an edit recorded in `seen` at
/// step 7 (after the last think, before the repeat) DOES re-arm the
/// trigger.
#[test]
fn repeat_failure_thinks_rearms_when_seen_has_an_edit_after_last_think_step() {
    let skill = ImplementSkill::new();
    let agent = CodingWorkspace {
        seen: vec![SeenFile {
            path: "a.rs".to_string(),
            edited: vec![[1, 3]],
            step: 7,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cur = committed(agent);
    cur.harness.last_think_step = Some(5);
    cur.harness.repeated_failure_step = Some(10);

    let step =
        parse_and_validate(&skill, &cur, edit_file_turn_content()).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "think"),
        "an edit since the last think call must re-arm the trigger, got {:?}",
        step.action()
    );
}

/// Same as above, but the recorded edit is BEFORE `last_think_step` (step 3,
/// not after): nothing new has happened since the last `think` call
/// answered, so the trigger must not fire again.
#[test]
fn repeat_failure_thinks_does_not_rearm_when_seen_edit_predates_last_think_step() {
    let skill = ImplementSkill::new();
    let agent = CodingWorkspace {
        seen: vec![SeenFile {
            path: "a.rs".to_string(),
            edited: vec![[1, 3]],
            step: 3,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cur = committed(agent);
    cur.harness.last_think_step = Some(5);
    cur.harness.repeated_failure_step = Some(10);

    let step =
        parse_and_validate(&skill, &cur, edit_file_turn_content()).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "edit_file"),
        "an edit predating the last think call must not re-arm the trigger, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

/// Same gating, but `seen` is empty entirely: nothing has been edited at
/// all since the last think call, so the trigger must not fire.
#[test]
fn repeat_failure_thinks_does_not_rearm_when_seen_is_empty() {
    let skill = ImplementSkill::new();
    let cur_agent = CodingWorkspace::default();
    assert!(cur_agent.seen.is_empty());
    let mut cur = committed(cur_agent);
    cur.harness.last_think_step = Some(5);
    cur.harness.repeated_failure_step = Some(10);

    let step =
        parse_and_validate(&skill, &cur, edit_file_turn_content()).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "edit_file"),
        "an empty seen list must not re-arm the trigger, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

/// `last_think_step: None` (no think call has ever run) fires regardless of
/// `seen` -- the edit-since gate only applies once a `think` call has
/// actually run for this streak.
#[test]
fn repeat_failure_thinks_fires_with_no_prior_think_even_when_seen_is_empty() {
    let skill = ImplementSkill::new();
    let cur_agent = CodingWorkspace::default();
    assert!(cur_agent.seen.is_empty());
    let mut cur = committed(cur_agent);
    cur.harness.last_think_step = None;
    cur.harness.repeated_failure_step = Some(10);

    let step = parse_and_validate(&skill, &cur, edit_file_turn_content())
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "think"),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
}

#[test]
fn repeat_failure_thinks_does_not_fire_when_think_ran_at_the_repeat_step() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = Some(10);

    let step =
        parse_and_validate(&skill, &cur, edit_file_turn_content()).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "edit_file"),
        "the model's own edit_file call must commit unchanged, got {:?}",
        step.action()
    );
    assert!(
        step.notes().is_empty(),
        "no repeat-think note expected: {:?}",
        step.notes()
    );
}

#[test]
fn repeat_failure_thinks_does_not_fire_when_think_ran_after_the_repeat_step() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = Some(11);

    let step = parse_and_validate(&skill, &cur, write_file_turn_content())
        .expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "write_file"),
        "the model's own write_file call must commit unchanged, got {:?}",
        step.action()
    );
    assert!(
        step.notes().is_empty(),
        "no repeat-think note expected: {:?}",
        step.notes()
    );
}

#[test]
fn repeat_failure_thinks_does_not_fire_without_a_repeated_failure() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = None;
    cur.harness.last_think_step = None;

    let step =
        parse_and_validate(&skill, &cur, edit_file_turn_content()).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "edit_file"),
        "got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

#[test]
fn repeat_failure_thinks_does_not_fire_for_read_file() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {},
        "action": {"type": "tool", "name": "read_file", "args": {"path": "a.rs"}},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "read_file"),
        "a read must not be substituted, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

/// The idle-test shape (no edits since the last `test`) DOES fire -- see
/// `repeat_failure_thinks_fires_for_idle_test_with_no_edits_since_test`
/// below. This test covers the shape that must NOT fire: a `test` call with
/// real edits since the last one, which is `implement_no_green_runs_test`'s
/// and the model's own territory.
#[test]
fn repeat_failure_thinks_does_not_fire_for_test_with_edits_since_test() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.edits_since_test = 2;

    let content = json!({
        "patch": {},
        "action": {"type": "tool", "name": "test", "args": {"cmd": "cargo test"}},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "test"),
        "a test call with edits since the last one must not be substituted, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

/// The stuck shape a first live battery showed the original (edit-only)
/// trigger miss entirely: the model re-runs the failing test with nothing
/// edited since, five times running on the trap fixture. `edits_since_test
/// == 0` on a `test` call, with a repeated failure open, must now be
/// substituted for `think` the same as an edit would be.
#[test]
fn repeat_failure_thinks_fires_for_idle_test_with_no_edits_since_test() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.edits_since_test = 0;

    let content = json!({
        "patch": {},
        "action": {"type": "tool", "name": "test", "args": {"cmd": "cargo test"}},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "think"
                    && args == &json!({"question": super::REPEAT_FAILURE_QUESTION})
        ),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[IDLE_TEST_THINK_NOTE.to_string()],
        "the idle-test repeat-think note must be attached"
    );
}

#[test]
fn repeat_failure_thinks_does_not_fire_for_the_models_own_think_call() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {},
        "action": {"type": "tool", "name": "think", "args": {"question": "why?"}},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        statie_core::Action::Tool { name, args } => {
            assert_eq!(name, "think");
            assert_eq!(
                args,
                &json!({"question": "why?"}),
                "the model's own think question must not be overwritten"
            );
        }
        other => panic!("expected Action::Tool(think), got {other:?}"),
    }
    assert!(step.notes().is_empty());
}

/// The patch survives even when it carries real content (not just an empty
/// object) -- the substitution only replaces the action, never the patch.
#[test]
fn repeat_failure_thinks_leaves_a_non_empty_patch_intact() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {"recent_summary": "tried fixing tax.rs"},
        "action": {
            "type": "tool",
            "name": "edit_file",
            "args": {"path": "a.rs", "old": "x", "new": "y"},
        },
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("the substituted think call must not be rejected");
    assert!(matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "think"));
    let (patch, _action, _acks) = step.into_parts();
    assert_eq!(patch.next().recent_summary, "tried fixing tax.rs");
}

/// Live battery shape: with a repeat open, the model sends `ask_user` with a
/// statement of intent instead of a question -- `ask_user_empty_note`/
/// `ask_user_statement_note` would otherwise just convert this into a
/// `Continue` note, which the model has already shown it does not act on.
/// `implement_repeat_failure_thinks` must take precedence and substitute
/// `think` instead.
#[test]
fn repeat_failure_thinks_fires_for_non_question_ask_user() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {},
        "action": {"type": "ask_user", "message": "I will look at tax.rs next"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "think"
                    && args == &json!({"question": super::REPEAT_FAILURE_QUESTION})
        ),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[ASK_USER_THINK_NOTE.to_string()],
        "the ask-user repeat-think note must be attached"
    );
}

/// A real question (contains `?`) with a repeat open must NOT be
/// substituted -- the model asking the user something it genuinely cannot
/// find out itself is left alone even when a repeated failure is open.
#[test]
fn repeat_failure_thinks_does_not_fire_for_a_real_question_ask_user() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {},
        "action": {"type": "ask_user", "message": "Which file should I fix?"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::AskUser { message } if message == "Which file should I fix?"),
        "a real question must not be substituted, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}
