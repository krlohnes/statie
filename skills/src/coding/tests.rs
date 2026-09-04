use std::sync::Arc;

use serde_json::json;

use statie_core::{
    Action, CommittedState, HarnessState, ParsedOutput, PatchError, RawOutput, Report,
    ReportOutcome, ReportStats, Skill, Version,
};
use statie_patch::JsonMergePatch;

use super::{
    CodingWorkspace, FileNote, ImplementSkill, PlanSkill, PlanStep, ReviewSkill, RouterSkill,
    StepStatus, coding_skills, coding_skills_with_delegates,
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

#[test]
fn implement_bare_done_is_not_affected_by_plan_only_fix() {
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
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
    );
}

/// Previously: `done` with `next_skill: "review"` while a plan step is
/// pending was rejected (`PatchError::RuleViolation`) and the patch (here,
/// the attempted `next_skill` change) was discarded. Now: the premature-done
/// conversion (`premature_done_note`) fires first -- the step is no longer
/// rejected, `next_skill` is stripped so the handoff does not silently
/// land, and the action becomes the harness-internal `Action::Continue`
/// naming the still-open step.
#[test]
fn implement_handoff_to_review_with_pending_step_continues_instead_of_handoff() {
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

#[test]
fn model_patch_adding_bogus_empty_report_is_rejected() {
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
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    match err {
        PatchError::RuleViolation(msg) => {
            assert!(
                msg.contains("`reports`") && msg.contains("harness-owned"),
                "expected the harness-owned rejection message: {msg}"
            );
        }
        other => panic!("expected RuleViolation, got {other:?}"),
    }
}

#[test]
fn model_patch_deleting_existing_report_is_rejected() {
    // `reports` is harness-owned (see `common_harness_owned_fields`), so a
    // model patch that removes an existing entry is caught by
    // `validate_default`'s owned-field check, not by `rules` -- `rules` no
    // longer inspects `reports` at all (see
    // `absorbed_reports_pass_rules_directly_now_that_rules_does_not_inspect_reports`
    // for the corresponding statement about the bare `rules` call).
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
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    match err {
        PatchError::RuleViolation(msg) => {
            assert!(
                msg.contains("`reports`") && msg.contains("harness-owned"),
                "expected the harness-owned rejection message: {msg}"
            );
        }
        other => panic!("expected RuleViolation, got {other:?}"),
    }
}

#[test]
fn model_patch_altering_existing_report_is_rejected() {
    // Any change to `reports` -- fabricated, legitimate-looking, or a
    // removal -- is rejected by the harness-owned-field check in
    // `validate_default`, which compares committed vs. post-patch values
    // rather than trying to distinguish "looks fabricated" from "looks
    // real". Send a patch that overwrites the existing report's `summary`.
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
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    match err {
        PatchError::RuleViolation(msg) => {
            assert!(
                msg.contains("`reports`") && msg.contains("harness-owned"),
                "expected the harness-owned rejection message: {msg}"
            );
        }
        other => panic!("expected RuleViolation, got {other:?}"),
    }
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
