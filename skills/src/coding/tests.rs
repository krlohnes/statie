use std::sync::Arc;

use serde_json::{Value, json};

use statie_core::{
    Action, CommittedState, HarnessState, ParsedOutput, RawOutput, Report, ReportOutcome,
    ReportStats, Skill, SkippedRead, Version,
};
use statie_patch::JsonMergePatch;

use super::{
    CodingWorkspace, Decision, FINDINGS_OPEN_NOTE_PREFIX, FileNote, GroundedFindingsCase,
    INFERRED_TOOL_NOTE_PREFIX, ImplementSkill, OPEN_STEP_NOTE_FIRST, OPEN_STEP_NOTE_REPEAT,
    PlanSkill, PlanStep, REVIEW_ROUNDS_CAP_QUESTION, ROUTER_ROUTED_NOTE_PREFIX, ReadFileArgs,
    ReviewSkill, RouterSkill, STEP_READ_ALREADY_NOTE_PREFIX, SeenFile, StepStatus, TestRecord,
    UNGROUNDED_FINDINGS_NOTE_PREFIX, coding_skills, coding_skills_with_delegates,
    common_after_tool, count_lines, failing_test_names, finding_is_grounded, first_error_line,
    has_named_handoff, infer_tool_name, merge_range, parse_edit_header, parse_finding_location,
    parse_revert_lines, review_grounded_findings, seen_entry, shell_read_as_read_file,
    write_file_confirmed_by_seen,
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
            try_fixes_available: false,
            last_try_fixes_step: None,
            consecutive_non_question_asks: 0,
            consecutive_continues: 0,
            last_failure_summary: None,
            last_skip: None,
            last_tool_error: None,
            last_stall: None,
            last_tool: None,
            fails_since_green: 0,
            notes_step: None,
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

// ---------------------------------------------------------------------
// Item 55: the router's bare `done` (no `next_skill`, or a self-
// referential `"router"`) is routed by state instead of being rejected by
// `validate_default`'s handoff rule. See `router_bare_done_routes`
// (`skills/src/coding/mod.rs`) for the exact ordering and reasons.
// ---------------------------------------------------------------------

#[test]
fn router_bare_done_with_empty_plan_routes_to_plan() {
    // The live shape: `{"patch": {"task": "..."}, "action": {"type":
    // "done"}}`, no `next_skill` at all.
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"task": "fix the bug"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("bare done with an empty plan must route, not reject");
    assert!(
        matches!(step.action(), Action::Done),
        "a routed done is a real handoff, not a Continue: {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        [format!(
            "{ROUTER_ROUTED_NOTE_PREFIX}`plan`: the plan is empty."
        )]
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("plan"));
    assert_eq!(
        patch.next().task,
        "fix the bug",
        "the rest of the patch must have landed"
    );
}

#[test]
fn router_bare_done_with_step_two_open_routes_to_implement_and_notes_step_two() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "first step".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "second step".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("bare done with an open step must route, not reject");
    assert!(matches!(step.action(), Action::Done));
    assert_eq!(
        step.notes(),
        [format!(
            "{ROUTER_ROUTED_NOTE_PREFIX}`implement`: plan step 2 is open."
        )]
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
}

#[test]
fn router_bare_done_with_all_steps_done_and_findings_open_routes_to_implement() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "only step".to_string(),
            status: StepStatus::Done,
        }],
        findings: vec!["a.rs:1 duplicated doc-comment line".to_string()],
        ..Default::default()
    });
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("bare done with open findings must route, not reject");
    assert!(matches!(step.action(), Action::Done));
    assert_eq!(
        step.notes(),
        [format!(
            "{ROUTER_ROUTED_NOTE_PREFIX}`implement`: findings are open."
        )]
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
}

#[test]
fn router_bare_done_with_all_done_and_no_findings_routes_to_review() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "only step".to_string(),
            status: StepStatus::Done,
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("bare done with a finished plan and no findings must route, not reject");
    assert!(matches!(step.action(), Action::Done));
    assert_eq!(
        step.notes(),
        [format!(
            "{ROUTER_ROUTED_NOTE_PREFIX}`review`: the plan is done and unreviewed."
        )]
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("review"));
}

#[test]
fn router_done_with_next_skill_already_implement_is_untouched() {
    let skill = RouterSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next_skill": "implement"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content)
        .expect("a done that already names a valid next_skill must validate unchanged");
    assert!(matches!(step.action(), Action::Done));
    assert!(
        step.notes().is_empty(),
        "no routing note when next_skill was already named: {:?}",
        step.notes()
    );
    let (patch, _, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
}

#[test]
fn router_done_with_merged_done_true_keeps_router_done_not_ours_note_unchanged() {
    // `done: true` is `router_done_not_ours_note`'s case, not this unit's:
    // that function strips `done` and converts the step to `Continue`
    // before `router_bare_done_routes` ever sees an action that is still
    // `Done`, so this test is the regression guard that the new routing
    // logic left that existing behaviour alone.
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
            assert!(
                note.contains("A step is finished by a tool call, not by marking it done"),
                "{note}"
            );
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

// ---------------------------------------------------------------------
// Unit 37: `premature_done_note`'s first-refusal/repeat-refusal split on
// `HarnessState::consecutive_continues`. Called directly (not through
// `ImplementSkill::validate`'s full chain): `green_done_note` fires ahead
// of `premature_done_note` whenever `green_step.is_some()` AND an open step
// is present -- forcing every open step done and handing off -- so a
// `green_step: Some(_)` scenario (acceptance (b)'s second case) can never
// reach `premature_done_note` through the full chain. Calling the function
// itself isolates exactly the unit this brief specifies.
// ---------------------------------------------------------------------

/// Parses `content` into a `ParsedOutput<CodingWorkspace>` the same way the
/// session would, for a direct call into `premature_done_note` below.
fn parsed_output(content: Value) -> ParsedOutput<CodingWorkspace> {
    ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(0, content))
        .expect("should parse as valid StepOutput JSON")
}

/// Acceptance (a): first refusal (`consecutive_continues: 0`, the default
/// `committed` gives). The note equals `OPEN_STEP_NOTE_FIRST` formatted
/// with the open step, and the patch's plan status changes are applied in
/// full (assert the merged plan statuses exactly): step 1 (sent as
/// `status: done` in the patch) reads `Done`, step 2 (untouched, still
/// `pending` in the committed state) reads `Pending`.
#[test]
fn premature_done_first_refusal_note_is_exact_and_patch_status_changes_land() {
    let cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "first change".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 2,
                text: "second change".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    assert_eq!(cur.harness.consecutive_continues, 0);
    let mut out = parsed_output(json!({
        "patch": {"plan": [{"id": 1, "status": "done"}], "next_skill": "review"},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("first refusal should convert");

    let expected_note =
        OPEN_STEP_NOTE_FIRST
            .replacen("{}", "2", 1)
            .replacen("{}", "second change", 1);
    assert_eq!(note, expected_note, "note text must match exactly");

    // The patch's plan status change landed in full, merged with the
    // committed workspace.
    let merged = super::merged_workspace(&cur.agent, &out).expect("patch should merge cleanly");
    assert_eq!(
        merged
            .plan
            .iter()
            .map(|s| (s.id, s.status))
            .collect::<Vec<_>>(),
        vec![(1, StepStatus::Done), (2, StepStatus::Pending)],
        "the patch's plan status change must have landed in full: {:?}",
        merged.plan
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        None,
        "next_skill must be stripped so the handoff does not land"
    );
}

/// Acceptance (b) first case: repeat refusal (`consecutive_continues: 1`)
/// with a patch marking step 2 done. The merged plan still has step 2
/// `Pending` (its status change was dropped, exact), and the note equals
/// `OPEN_STEP_NOTE_REPEAT` with `{call}` = "`test`" (since
/// `edits_since_test: 1` here).
#[test]
fn premature_done_repeat_refusal_drops_status_change_and_note_says_test_when_edits_pending() {
    // Step 1 done, step 3 pending and untouched by the patch -- keeps
    // `premature_done_note`'s top-level open-step gate satisfied both
    // before AND after step 2's status change is dropped, so the test
    // isolates step 2 being reverted rather than accidentally relying on
    // it to be the only thing keeping the plan open.
    let mut cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "first change".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "second change".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 3,
                text: "third change".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    cur.harness.consecutive_continues = 1;
    cur.harness.edits_since_test = 1;
    let mut out = parsed_output(json!({
        "patch": {"plan": [{"id": 2, "status": "done"}]},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("repeat refusal should convert");

    let merged = super::merged_workspace(&cur.agent, &out).expect("dropped patch should merge");
    assert_eq!(
        merged
            .plan
            .iter()
            .map(|s| (s.id, s.status))
            .collect::<Vec<_>>(),
        vec![
            (1, StepStatus::Done),
            (2, StepStatus::Pending),
            (3, StepStatus::Pending)
        ],
        "step 2's status change must have been dropped: {:?}",
        merged.plan
    );

    let expected_note = OPEN_STEP_NOTE_REPEAT
        .replacen("{}", "2", 1)
        .replacen("{}", "second change", 1)
        .replacen("{}", "`test`", 1);
    assert_eq!(note, expected_note, "note text must match exactly");
}

/// Acceptance (b) second case: same repeat refusal, but
/// `edits_since_test: 0` and `green_step: Some(3)` (nothing edited since a
/// passing run) -- `{call}` becomes the `read_file`/`edit_file` form
/// instead of `` `test` ``.
#[test]
fn premature_done_repeat_refusal_note_says_read_or_edit_when_nothing_edited_since_green() {
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 1,
            text: "only step".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    cur.harness.consecutive_continues = 1;
    cur.harness.edits_since_test = 0;
    cur.harness.green_step = Some(3);
    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("repeat refusal should convert");

    let expected_note = OPEN_STEP_NOTE_REPEAT
        .replacen("{}", "1", 1)
        .replacen("{}", "only step", 1)
        .replacen(
            "{}",
            "`read_file` or `edit_file` on the file that step names",
            1,
        );
    assert_eq!(note, expected_note, "note text must match exactly");
}

/// Acceptance (c): a repeat refusal whose patch has only a plan status
/// change (a single entry, `{"id": 1, "status": "done"}`, with no other
/// field) results in a patch with no `plan` key at all (assert the raw
/// patch exactly): dropping `status` leaves nothing but `id`, a no-op, so
/// the entry -- and then the now-empty `plan` array -- is removed.
#[test]
fn fill_template_keeps_braces_inside_a_value_out_of_later_placeholders() {
    let got = super::fill_template("a {} b {} c {}", &["fn f() {}", "two", "three"]);
    assert_eq!(got, "a fn f() {} b two c three");
    let short = super::fill_template("x {} y {}", &["only"]);
    assert_eq!(
        short, "x only y ",
        "a missing value drops the placeholder and its tail"
    );
}

#[test]
fn premature_done_repeat_refusal_status_only_entry_drops_plan_key_entirely() {
    // Two committed steps: step 1 (the patch marks it done, and only that)
    // and step 2 (untouched, stays open) -- so the pre-drop merge still has
    // an open step (step 2) and `premature_done_note`'s top-level gate
    // fires, but the raw patch's own `plan` array holds only the one entry
    // this test cares about.
    let mut cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "first step".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 2,
                text: "second step".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    cur.harness.consecutive_continues = 1;
    let mut out = parsed_output(json!({
        "patch": {"plan": [{"id": 1, "status": "done"}]},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let _note = super::premature_done_note(&cur, &mut out).expect("repeat refusal should convert");

    assert_eq!(
        out.patch().0.clone(),
        json!({}),
        "the plan key must be dropped entirely, leaving an empty raw patch: {:?}",
        out.patch().0
    );
}

// ---------------------------------------------------------------------
// Unit 50: `premature_done_note`'s pre-check, `step_read_already_note` --
// a `done` with an open step whose text names nothing but a path the
// harness already refused to re-read as redundant (`HarnessState::
// last_skip`) closes that step instead of refusing.
// ---------------------------------------------------------------------

/// A `SkippedRead` for a `read_file` call of `path`, with the same `key`
/// core's own guard would compute (`statie_core::read_call_key`), so the
/// fixture matches production exactly rather than hand-writing the JSON
/// shape.
fn skipped_read(path: &str) -> SkippedRead {
    SkippedRead {
        tool: "read_file".to_string(),
        key: statie_core::read_call_key("read_file", &json!({"path": path})),
        count: 1,
        step: 3,
    }
}

/// Acceptance (a): plan step 2 open, its text naming `src/tax.rs`;
/// `last_skip` a `read_file` of that same path; `done` with an empty patch.
/// The note is exact, the merged plan reads step 2 `done`, and the action
/// is still `Done` (never converted to `Continue` -- `premature_done_note`
/// itself never sets the action; only its caller in `ImplementSkill::
/// validate` does, and this test calls the function directly).
#[test]
fn premature_done_redundant_read_of_open_steps_file_marks_it_done() {
    let mut cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 1,
                text: "first change".to_string(),
                status: StepStatus::Done,
            },
            PlanStep {
                id: 2,
                text: "examine `src/tax.rs` to understand the rounding logic".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    cur.harness.last_skip = Some(skipped_read("src/tax.rs"));
    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out)
        .expect("a redundant read of the open step's file must convert");

    let expected_prefix = STEP_READ_ALREADY_NOTE_PREFIX
        .replace("{id}", "2")
        .replace("{path}", "src/tax.rs");
    let expected_note =
        format!("{expected_prefix}examine `src/tax.rs` to understand the rounding logic");
    assert_eq!(note, expected_note, "note text must match exactly");

    let merged = super::merged_workspace(&cur.agent, &out).expect("patch should merge cleanly");
    assert_eq!(
        merged
            .plan
            .iter()
            .map(|s| (s.id, s.status))
            .collect::<Vec<_>>(),
        vec![(1, StepStatus::Done), (2, StepStatus::Done)],
        "step 2 must have been marked done by the rewritten patch: {:?}",
        merged.plan
    );
    assert!(
        matches!(out.action(), Action::Done),
        "the action must stay Done, not be converted to Continue: {:?}",
        out.action()
    );
}

/// Acceptance (b): the same open step, but `last_skip` names a DIFFERENT
/// path than the one the step's text mentions -- the ordinary
/// `OPEN_STEP_NOTE_FIRST` refusal fires unchanged.
#[test]
fn premature_done_redundant_read_of_a_different_path_still_refuses() {
    let mut cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 2,
            text: "examine `src/tax.rs` to understand the rounding logic".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    cur.harness.last_skip = Some(skipped_read("src/other.rs"));
    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("first refusal should convert");

    let expected_note = OPEN_STEP_NOTE_FIRST.replacen("{}", "2", 1).replacen(
        "{}",
        "examine `src/tax.rs` to understand the rounding logic",
        1,
    );
    assert_eq!(
        note, expected_note,
        "a skip of a different path must not close the step: unchanged refusal"
    );
    let merged = super::merged_workspace(&cur.agent, &out).expect("patch should merge cleanly");
    assert_eq!(
        merged.plan.first().map(|s| s.status),
        Some(StepStatus::Pending),
        "the step must still be open: {:?}",
        merged.plan
    );
}

/// Acceptance (c): the same open step, but `last_skip == None` (nothing was
/// ever skipped) -- the ordinary refusal fires unchanged.
#[test]
fn premature_done_no_last_skip_still_refuses() {
    let cur = committed(CodingWorkspace {
        plan: vec![PlanStep {
            id: 2,
            text: "examine `src/tax.rs` to understand the rounding logic".to_string(),
            status: StepStatus::Pending,
        }],
        ..Default::default()
    });
    assert_eq!(cur.harness.last_skip, None);
    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("first refusal should convert");

    let expected_note = OPEN_STEP_NOTE_FIRST.replacen("{}", "2", 1).replacen(
        "{}",
        "examine `src/tax.rs` to understand the rounding logic",
        1,
    );
    assert_eq!(
        note, expected_note,
        "no last_skip must not close the step: unchanged refusal"
    );
}

/// Acceptance (d): the skipped path appears in step 3's text, but step 2 is
/// the FIRST open step (and its text does not mention that path) -- the
/// ordinary refusal fires unchanged, naming step 2, not step 3.
#[test]
fn premature_done_path_in_a_later_steps_text_does_not_close_the_first_open_step() {
    let mut cur = committed(CodingWorkspace {
        plan: vec![
            PlanStep {
                id: 2,
                text: "wire up the CLI flag".to_string(),
                status: StepStatus::Pending,
            },
            PlanStep {
                id: 3,
                text: "examine `src/tax.rs` to understand the rounding logic".to_string(),
                status: StepStatus::Pending,
            },
        ],
        ..Default::default()
    });
    cur.harness.last_skip = Some(skipped_read("src/tax.rs"));
    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::premature_done_note(&cur, &mut out).expect("first refusal should convert");

    let expected_note =
        OPEN_STEP_NOTE_FIRST
            .replacen("{}", "2", 1)
            .replacen("{}", "wire up the CLI flag", 1);
    assert_eq!(
        note, expected_note,
        "the path matching a LATER step must not close the FIRST open step: unchanged refusal \
         naming step 2"
    );
    let merged = super::merged_workspace(&cur.agent, &out).expect("patch should merge cleanly");
    assert_eq!(
        merged
            .plan
            .iter()
            .map(|s| (s.id, s.status))
            .collect::<Vec<_>>(),
        vec![(2, StepStatus::Pending), (3, StepStatus::Pending)],
        "neither step must have been touched: {:?}",
        merged.plan
    );
}

// ---------------------------------------------------------------------
// Unit 47: `REFUSAL_QUESTION`'s exact formatting on a single refusal note,
// and `refusal_thinks`'s substitution called directly (isolating the
// function the same way the `premature_done_note` unit tests above do --
// `refusal_thinks` is the caller-agnostic second half of the same
// mechanism, run right after whichever refusal note function produced the
// `Action::Continue` it substitutes).
// ---------------------------------------------------------------------

/// `REFUSAL_QUESTION`'s `{note}` placeholder filled with one refusal note
/// text, exact -- the brief's own spec text, reproduced verbatim here so a
/// change to either the template or this test's fill-in breaks a visible
/// assertion, not just a `contains` check.
#[test]
fn refusal_question_formats_one_refusal_note_exactly() {
    let note = "plan step 1 is still open: the first change. A step is finished by a tool call, \
                not by marking it done; make the change with edit_file/write_file and run \
                `test`, or if that step's work is already done, run `test` to show it. `done` \
                is for a finished plan; your patch was kept.";

    let question = super::REFUSAL_QUESTION.replace("{note}", note);

    let expected = format!(
        "The harness refused your last two actions for the same reason: {note}. You must not \
         send that action again. Which single tool call moves this step forward now: `test`, \
         `read_file` on the step's file, `edit_file`, or, if you are truly blocked, `ask_user` \
         with a real question? Answer with the exact call."
    );
    assert_eq!(question, expected);
}

/// `refusal_thinks` called directly on a hand-built `Action::Continue` (the
/// shape any of the refusal note functions leaves `out` in): with
/// `consecutive_continues == 1` (the second refusal in a row) and no
/// `think` since, substitutes `Action::Tool { name: "think", args:
/// {"question": REFUSAL_QUESTION filled} }` and returns
/// `REFUSAL_THINK_NOTE_PREFIX` plus the refusal note, exact.
#[test]
fn refusal_thinks_substitutes_think_with_the_exact_question_and_prefixed_note() {
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_continues = 1;
    cur.harness.last_think_step = None;

    let refusal_note = "plan step 1 is still open: the first change. No tool has run since                          your last `done`, and plan steps are not marked done without one; the                          status changes in this patch were dropped. Your next action must be a                          tool call: `test`. `done` is for a finished plan.";

    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));
    out.set_action(Action::Continue {
        note: refusal_note.to_string(),
    });

    let note =
        super::refusal_thinks(&cur, &mut out).expect("the second refusal must substitute think");

    let expected_question = super::REFUSAL_QUESTION.replace("{note}", refusal_note);
    assert!(
        matches!(
            out.action(),
            Action::Tool { name, args }
                if name == "think" && args == &json!({"question": expected_question})
        ),
        "expected a substituted think call with the filled REFUSAL_QUESTION, got {:?}",
        out.action()
    );
    let expected_note = format!("{}{refusal_note}", super::REFUSAL_THINK_NOTE_PREFIX);
    assert_eq!(
        note, expected_note,
        "note must be the prefix plus the refusal note, exact"
    );
}

/// `refusal_thinks` does not fire on a FIRST refusal
/// (`consecutive_continues == 0`): the caller's `Action::Continue` is left
/// untouched.
#[test]
fn refusal_thinks_does_not_fire_on_the_first_refusal() {
    let cur = committed(CodingWorkspace::default());
    assert_eq!(cur.harness.consecutive_continues, 0);

    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));
    out.set_action(Action::Continue {
        note: "still open".to_string(),
    });

    let note = super::refusal_thinks(&cur, &mut out);
    assert_eq!(note, None, "the first refusal must not be substituted");
    assert!(matches!(out.action(), Action::Continue { .. }));
}

/// "Once per streak": with `consecutive_continues == 1` but `last_think_
/// step` at or after the current step (a `think` -- this trigger's or any
/// other -- ran on the immediately preceding committed step), `refusal_
/// thinks` does not fire again.
#[test]
fn refusal_thinks_once_per_streak_does_not_fire_right_after_a_think() {
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_continues = 1;
    cur.harness.step = 5;
    cur.harness.last_think_step = Some(4);

    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));
    out.set_action(Action::Continue {
        note: "still open".to_string(),
    });

    let note = super::refusal_thinks(&cur, &mut out);
    assert_eq!(
        note, None,
        "once per streak: a think on the immediately preceding step must block a second one"
    );
    assert!(matches!(out.action(), Action::Continue { .. }));
}

/// `refusal_thinks` only fires on `Action::Continue`: a `Done`/`Tool`/
/// `AskUser` action is left untouched even with `consecutive_continues ==
/// 1`.
#[test]
fn refusal_thinks_ignores_a_non_continue_action() {
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_continues = 1;

    let mut out = parsed_output(json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let note = super::refusal_thinks(&cur, &mut out);
    assert_eq!(note, None);
    assert!(matches!(out.action(), Action::Done));
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

// ---------------------------------------------------------------------
// Unit 49: `implement_findings_open_note` -- a `done` sent with `review`'s
// findings still open and no tool call this activation is parked as a
// harness `Continue` instead of handed straight back to `review`.
// ---------------------------------------------------------------------

/// (a) fires on `done` with non-empty `findings` and `tool_calls == 0`:
/// exact note, action `Continue`, and the patch commits untouched (the
/// model sent an empty patch here, so the merged workspace afterward still
/// carries the same open findings).
#[test]
fn implement_findings_open_note_fires_on_bare_done_with_open_findings_and_no_tool_calls() {
    let skill = ImplementSkill::new();
    let cur = committed_with_tool_calls(
        CodingWorkspace {
            findings: vec!["a.rs:1 duplicated doc-comment line".to_string()],
            ..Default::default()
        },
        0,
    );
    let content = json!({
        "patch": {"next_skill": "review", "recent_summary": "handing off"},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    let expected_note = format!(
        "{FINDINGS_OPEN_NOTE_PREFIX}a.rs:1 duplicated doc-comment line Fix them with \
         `edit_file` and `test`, or `ask_user` why they are wrong. Your patch was kept."
    );
    let (patch, action, _) = step.into_parts();
    assert!(
        matches!(action, statie_core::Action::Continue { ref note } if *note == expected_note),
        "expected Continue with the exact findings-open note, got {action:?}"
    );
    assert_eq!(
        patch.next().findings,
        vec!["a.rs:1 duplicated doc-comment line".to_string()],
        "the patch must be left untouched -- findings still carried on the committed workspace"
    );
    assert_eq!(
        patch.next().next_skill,
        None,
        "next_skill must be stripped: a Continue carrying a changed next_skill is rejected by core"
    );
    assert_eq!(
        patch.next().recent_summary,
        "handing off",
        "the rest of the patch is kept"
    );
}

/// (b) does not fire once a tool call has run this activation
/// (`tool_calls == 1`): the `done` falls through to the ordinary chain
/// instead (here, `implement_bare_done` hands off to review, since the plan
/// is empty and there is nothing open -- see that test's sibling for the
/// exact shape). The point under test is only that `implement_findings_
/// open_note`'s own note never appears.
#[test]
fn implement_findings_open_note_does_not_fire_with_one_tool_call_this_activation() {
    let skill = ImplementSkill::new();
    let mut cur = committed_with_tool_calls(
        CodingWorkspace {
            findings: vec!["a.rs:1 duplicated doc-comment line".to_string()],
            ..Default::default()
        },
        1,
    );
    cur.harness.green_step = Some(0);
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    assert!(
        !step
            .notes()
            .iter()
            .any(|n| n.starts_with(FINDINGS_OPEN_NOTE_PREFIX)),
        "the findings-open note must not fire once a tool call has run: {:?}",
        step.notes()
    );
}

/// (c) does not fire with empty `findings`, even with `tool_calls == 0`:
/// falls through to the ordinary chain (`implement_bare_done` hands off to
/// review here, plan empty and verification green).
#[test]
fn implement_findings_open_note_does_not_fire_with_empty_findings() {
    let skill = ImplementSkill::new();
    let mut cur = committed_with_tool_calls(CodingWorkspace::default(), 0);
    cur.harness.green_step = Some(0);
    let content = json!({
        "patch": {},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    assert!(
        !step
            .notes()
            .iter()
            .any(|n| n.starts_with(FINDINGS_OPEN_NOTE_PREFIX)),
        "the findings-open note must not fire with empty findings: {:?}",
        step.notes()
    );
    assert!(
        matches!(step.action(), statie_core::Action::Done),
        "expected the ordinary bare-done handoff to review, got {:?}",
        step.action()
    );
}

// ---------------------------------------------------------------------
// Unit 49: the review-rounds cap (`review_rounds_cap_note`) -- a hand-built
// `CommittedState` with `review_rounds` past the cap turns a send-back into
// `Action::AskUser` with the exact park question; at the cap, it stays a
// send-back.
// ---------------------------------------------------------------------

/// A `review` workspace already carrying open findings (as if a prior
/// activation had already sent them back), with `review_rounds` set
/// directly (as `on_activate` would have left it committed). `seen` grounds
/// the `a.rs:1` citation (item 12: grounded review findings, on by default)
/// so the finding is not itself dropped by that unrelated guard.
fn review_workspace_with_rounds(review_rounds: u32) -> CodingWorkspace {
    CodingWorkspace {
        findings: vec!["a.rs:1 duplicated doc-comment line".to_string()],
        review_rounds,
        seen: vec![SeenFile {
            path: "a.rs".to_string(),
            lines: 10,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// (d), over cap: `review_rounds == 4`, cap 3 (`ReviewSkill::default()`'s
/// own default) -- a plain model-sent send-back (`done: false`, `findings`
/// non-empty, no `next_skill`) is parked as `Action::AskUser` with the exact
/// question, `{n}` filled with 3 (`review_rounds - 1`, the number of
/// send-backs so far) and `{findings}` with the joined findings.
#[test]
fn review_rounds_cap_over_cap_parks_the_sendback_with_the_exact_question() {
    let skill = ReviewSkill::new();
    let cur = committed(review_workspace_with_rounds(4));
    let content = json!({
        "patch": {
            "findings": ["a.rs:1 duplicated doc-comment line"],
            "done": false,
            "next_skill": "implement",
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    let expected_message = REVIEW_ROUNDS_CAP_QUESTION
        .replace("{n}", "3")
        .replace("{findings}", "a.rs:1 duplicated doc-comment line");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::AskUser { message } if *message == expected_message
        ),
        "expected AskUser with the exact review-rounds-cap question, got {:?}",
        step.action()
    );
    assert!(
        step.notes()
            .iter()
            .any(|n| n == "review rounds cap (3) reached: parked with the findings"),
        "expected the exact park note: {:?}",
        step.notes()
    );
}

/// (d), at cap: `review_rounds == 3`, cap 3 -- the same send-back shape
/// stays a send-back (action `Done`, `next_skill: implement` via
/// `review_sendback_note`'s own normalizing), not parked.
#[test]
fn review_rounds_cap_at_cap_stays_a_sendback() {
    let skill = ReviewSkill::new();
    let cur = committed(review_workspace_with_rounds(3));
    let content = json!({
        "patch": {
            "findings": ["a.rs:1 duplicated doc-comment line"],
            "done": false,
            "next_skill": "implement",
        },
        "action": {"type": "done"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("must not be rejected");
    assert!(
        matches!(step.action(), statie_core::Action::Done),
        "expected the ordinary send-back (Done, handing off to implement), got {:?}",
        step.action()
    );
    let (patch, _action, _) = step.into_parts();
    assert_eq!(patch.next().next_skill.as_deref(), Some("implement"));
    assert!(!patch.next().done);
}

// ---------------------------------------------------------------------
// Unit 20: `implement_try_fixes_needs_failing_test`
// (`ImplementSkill::validate`'s `validate`-level gate for `try_fixes`) --
// see `skills/tests/try_fixes_gate.rs` for the end-to-end proof that this
// keeps the call from ever reaching `core/src/session.rs`'s
// `execute_try_fixes` (which would otherwise log a `tool_error`).
// ---------------------------------------------------------------------

fn try_fixes_action() -> serde_json::Value {
    json!({
        "patch": {},
        "action": {"type": "tool", "name": "try_fixes", "args": {}},
        "ack_inbox": []
    })
}

/// (a) No `tests` record at all: `try_fixes` becomes `test` at the same
/// step, with the exact note text, and the patch is otherwise untouched.
#[test]
fn implement_try_fixes_with_no_tests_record_is_substituted_for_test() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        tests: None,
        ..Default::default()
    });
    let step = parse_and_validate(&skill, &cur, try_fixes_action())
        .expect("the substituted test call should not be rejected");
    let notes = step.notes().to_vec();
    let (_patch, action, _acks) = step.into_parts();
    match action {
        Action::Tool { name, args } => {
            assert_eq!(
                name, "test",
                "try_fixes with no tests record must be substituted for test"
            );
            assert_eq!(args, json!({}));
        }
        other => panic!("expected Action::Tool {{ name: \"test\", .. }}, got {other:?}"),
    }
    assert_eq!(
        notes,
        vec![
            "`try_fixes` needs a failing test to work from, so the harness ran `test` instead."
                .to_string()
        ]
    );
}

/// (b) A failing `tests` record (`pass: false`): `try_fixes` passes
/// through unchanged.
#[test]
fn implement_try_fixes_with_a_failing_tests_record_passes_through_unchanged() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        tests: Some(TestRecord {
            cmd: "cargo test".to_string(),
            pass: false,
            failing: vec!["a_test".to_string()],
            first_error: None,
            step: 3,
        }),
        ..Default::default()
    });
    let step = parse_and_validate(&skill, &cur, try_fixes_action())
        .expect("try_fixes with a failing tests record must not be rejected");
    let notes = step.notes().to_vec();
    let (_patch, action, _acks) = step.into_parts();
    match action {
        Action::Tool { name, args } => {
            assert_eq!(
                name, "try_fixes",
                "try_fixes must pass through unchanged when a failing tests record exists"
            );
            assert_eq!(args, json!({}));
        }
        other => panic!("expected Action::Tool {{ name: \"try_fixes\", .. }}, got {other:?}"),
    }
    assert!(
        notes.is_empty(),
        "no gate note should be attached when try_fixes passes through, got {notes:?}"
    );
}

/// (c) A passing `tests` record (`pass: true`, nothing to fix): `try_fixes`
/// is substituted for `test`, exactly like (a).
#[test]
fn implement_try_fixes_with_a_passing_tests_record_is_substituted_for_test() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace {
        tests: Some(TestRecord {
            cmd: "cargo test".to_string(),
            pass: true,
            failing: vec![],
            first_error: None,
            step: 3,
        }),
        ..Default::default()
    });
    let step = parse_and_validate(&skill, &cur, try_fixes_action())
        .expect("the substituted test call should not be rejected");
    let notes = step.notes().to_vec();
    let (_patch, action, _acks) = step.into_parts();
    match action {
        Action::Tool { name, args } => {
            assert_eq!(
                name, "test",
                "try_fixes with a passing (nothing-to-fix) tests record must be substituted \
                 for test"
            );
            assert_eq!(args, json!({}));
        }
        other => panic!("expected Action::Tool {{ name: \"test\", .. }}, got {other:?}"),
    }
    assert_eq!(
        notes,
        vec![
            "`try_fixes` needs a failing test to work from, so the harness ran `test` instead."
                .to_string()
        ]
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
///
/// The finding cites `src/foo.rs:1` (grounded via the `files` entry set on
/// `cur`; item 12: grounded review findings) so this test's own concern --
/// the `done`/`next_skill` conversion -- is not accidentally defeated by the
/// unrelated grounding guard (on by default) dropping the finding first.
#[test]
fn review_done_true_with_findings_converts_to_handoff_instead_of_rejecting() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace {
        files: vec![FileNote {
            path: "src/foo.rs".to_string(),
            note: String::new(),
        }],
        ..Default::default()
    });
    let content = json!({
        "patch": {"done": true, "findings": ["src/foo.rs:1 something is wrong"]},
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
    //
    // The finding cites `src/foo.rs:1` (grounded via the `files` entry set
    // on `cur`; item 12: grounded review findings) so this test's own
    // concern is not accidentally defeated by the unrelated grounding guard
    // (on by default) dropping the finding first.
    let skill = ReviewSkill::new();
    let cur = committed_with_tool_calls(
        CodingWorkspace {
            files: vec![FileNote {
                path: "src/foo.rs".to_string(),
                note: String::new(),
            }],
            ..Default::default()
        },
        0,
    );
    let content = json!({
        "patch": {
            "findings": ["src/foo.rs:1 something is wrong"],
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
            "try_fixes".to_string(),
            "probe".to_string(),
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

// ---------------------------------------------------------------------
// `parse_finding_location`/`finding_is_grounded` (item 12: grounded review
// findings).
// ---------------------------------------------------------------------

#[test]
fn parse_finding_location_reads_the_first_path_colon_digits_token() {
    assert_eq!(
        parse_finding_location("src/foo.rs:42 the loop never terminates"),
        Some(("src/foo.rs", 42))
    );
}

#[test]
fn parse_finding_location_reads_a_citation_mid_sentence() {
    assert_eq!(
        parse_finding_location("the bug is in src/foo.rs:7, a stray unwrap"),
        Some(("src/foo.rs", 7))
    );
}

#[test]
fn parse_finding_location_strips_wrapping_punctuation() {
    assert_eq!(
        parse_finding_location("see (src/foo.rs:12) for the bug"),
        Some(("src/foo.rs", 12))
    );
    assert_eq!(
        parse_finding_location("\"src/foo.rs:12\" is wrong"),
        Some(("src/foo.rs", 12))
    );
}

#[test]
fn parse_finding_location_digit_run_stops_at_the_first_non_digit() {
    // A second `:` right after the line number (a `path:line:col` shape)
    // does not extend the digit run or get parsed as part of it.
    assert_eq!(
        parse_finding_location("src/foo.rs:12:5 bad column too"),
        Some(("src/foo.rs", 12))
    );
    // Trailing punctuation right after the digits (no separating space)
    // does not disqualify the match either.
    assert_eq!(
        parse_finding_location("src/foo.rs:12. that's the bug"),
        Some(("src/foo.rs", 12))
    );
}

#[test]
fn parse_finding_location_skips_a_word_with_no_digits_after_the_colon() {
    // "note:" has a `:` but nothing but letters after it -- not a match --
    // so parsing continues to the next word instead of failing outright.
    assert_eq!(
        parse_finding_location("note: see src/foo.rs:9 for details"),
        Some(("src/foo.rs", 9))
    );
}

#[test]
fn parse_finding_location_rejects_a_colon_with_an_empty_path() {
    // A leading bare `:42` has no path before the colon -- not a match --
    // so parsing continues to the next word.
    assert_eq!(
        parse_finding_location(":42 src/foo.rs:9 is the real citation"),
        Some(("src/foo.rs", 9))
    );
}

#[test]
fn parse_finding_location_none_when_no_word_has_a_path_colon_digits_shape() {
    assert_eq!(parse_finding_location("this finding cites nothing"), None);
    assert_eq!(parse_finding_location(""), None);
}

fn workspace_with_seen(path: &str, lines: u32) -> CodingWorkspace {
    CodingWorkspace {
        seen: vec![SeenFile {
            path: path.to_string(),
            lines,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn workspace_with_files(path: &str) -> CodingWorkspace {
    CodingWorkspace {
        files: vec![FileNote {
            path: path.to_string(),
            note: String::new(),
        }],
        ..Default::default()
    }
}

#[test]
fn finding_is_grounded_true_when_line_is_within_the_seen_line_count() {
    let ws = workspace_with_seen("src/foo.rs", 100);
    assert!(finding_is_grounded(
        &ws,
        "src/foo.rs:42 the loop never terminates"
    ));
    // The last line itself is in range.
    assert!(finding_is_grounded(&ws, "src/foo.rs:100 the last line"));
}

#[test]
fn finding_is_grounded_false_when_line_is_past_the_seen_line_count() {
    let ws = workspace_with_seen("src/foo.rs", 100);
    assert!(!finding_is_grounded(&ws, "src/foo.rs:101 past the end"));
}

#[test]
fn finding_is_grounded_false_when_line_is_zero() {
    // A `path:0` citation is never grounded, `seen`/`files` entry or not --
    // there is no line 0.
    let ws = workspace_with_seen("src/foo.rs", 100);
    assert!(!finding_is_grounded(&ws, "src/foo.rs:0 nonsense"));
}

#[test]
fn finding_is_grounded_true_on_path_alone_when_files_has_it_but_seen_does_not() {
    // `files` carries no line count at all -- any positive line grounds the
    // citation on the path alone (see `known_file_line_count`'s doc).
    let ws = workspace_with_files("src/foo.rs");
    assert!(finding_is_grounded(&ws, "src/foo.rs:999999 whatever line"));
}

#[test]
fn finding_is_grounded_true_on_path_alone_when_seen_entry_has_no_recorded_length() {
    // `seen` has the path, but `lines` is 0 (`SeenFile::lines`'s doc: never
    // learned) -- accepted on the path alone, same as a `files`-only entry.
    let ws = workspace_with_seen("src/foo.rs", 0);
    assert!(finding_is_grounded(&ws, "src/foo.rs:5 whatever line"));
}

#[test]
fn finding_is_grounded_false_when_the_path_is_unknown() {
    let ws = CodingWorkspace::default();
    assert!(!finding_is_grounded(
        &ws,
        "src/foo.rs:1 nothing knows this file"
    ));
}

#[test]
fn finding_is_grounded_false_when_the_finding_has_no_citation_at_all() {
    let ws = workspace_with_seen("src/foo.rs", 100);
    assert!(!finding_is_grounded(&ws, "the loop never terminates"));
}

#[test]
fn finding_is_grounded_prefers_seens_line_count_over_files_when_both_have_the_path() {
    // `src/foo.rs` is in both `files` (no line count) and `seen` (lines:
    // 10) -- `seen`'s count wins, so a citation past it is ungrounded even
    // though `files` alone would have accepted any line.
    let ws = CodingWorkspace {
        files: vec![FileNote {
            path: "src/foo.rs".to_string(),
            note: String::new(),
        }],
        seen: vec![SeenFile {
            path: "src/foo.rs".to_string(),
            lines: 10,
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(finding_is_grounded(&ws, "src/foo.rs:10 in range"));
    assert!(!finding_is_grounded(&ws, "src/foo.rs:11 past the end"));
}

// ---------------------------------------------------------------------
// Unit 53: `review_grounded_findings` -- (a) all findings uncited returns
// `Some((AllDropped, note))` with the exact prefix and list, `findings`
// removed from the raw patch, other patch keys kept; (b) one cited, one
// not, is unchanged (`Some((PartialDrop, note))`, the pre-unit-53 note
// text); (c) all cited returns `None`.
// ---------------------------------------------------------------------

/// (a) Every finding in the raw patch is ungrounded (no `path:line`
/// citation at all): `Some((AllDropped, note))`, the note carrying
/// [`UNGROUNDED_FINDINGS_NOTE_PREFIX`] followed by the same quoted,
/// `; `-joined drop list the partial-drop case uses. `findings` is removed
/// from the raw patch entirely; the other patch key (`recent_summary`) is
/// kept untouched.
#[test]
fn review_grounded_findings_all_uncited_is_all_dropped_with_the_exact_note_and_findings_removed() {
    let cur = CodingWorkspace::default();
    let mut out = parsed_output(json!({
        "patch": {
            "findings": [
                "something is wrong somewhere, no idea where",
                "another vague complaint with no location",
            ],
            "recent_summary": "looked at the crate",
        },
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let result = review_grounded_findings(true, &cur, &mut out);

    let expected_note = format!(
        "{UNGROUNDED_FINDINGS_NOTE_PREFIX}\"something is wrong somewhere, no idea where\"; \
         \"another vague complaint with no location\"."
    );
    assert_eq!(
        result,
        Some((GroundedFindingsCase::AllDropped, expected_note)),
        "expected the all-dropped case with the exact note"
    );

    let patch_obj = out.patch().0.as_object().expect("patch must be an object");
    assert!(
        !patch_obj.contains_key("findings"),
        "findings must be removed from the raw patch entirely: {patch_obj:?}"
    );
    assert_eq!(
        patch_obj.get("recent_summary").and_then(Value::as_str),
        Some("looked at the crate"),
        "the rest of the patch must be kept untouched"
    );
}

/// (b) One cited, one not: unchanged from before unit 53 -- the ungrounded
/// finding is dropped, the grounded one survives in the raw patch's
/// `findings`, and the case returned is `PartialDrop` with the existing
/// partial-drop note text.
#[test]
fn review_grounded_findings_one_cited_one_not_is_partial_drop_unchanged() {
    let cur = CodingWorkspace {
        seen: vec![SeenFile {
            path: "src/foo.rs".to_string(),
            lines: 10,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut out = parsed_output(json!({
        "patch": {
            "findings": [
                "src/foo.rs:1 the loop never terminates",
                "something is wrong somewhere, no idea where",
            ],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let result = review_grounded_findings(true, &cur, &mut out);

    let expected_note = "Dropped 1 finding with no file:line citation. Every finding must name \
                          the line it is about. Dropped: \"something is wrong somewhere, no \
                          idea where\"."
        .to_string();
    assert_eq!(
        result,
        Some((GroundedFindingsCase::PartialDrop, expected_note)),
        "expected the partial-drop case with the pre-unit-53 note text, unchanged"
    );

    let patch_obj = out.patch().0.as_object().expect("patch must be an object");
    assert_eq!(
        patch_obj.get("findings"),
        Some(&json!(["src/foo.rs:1 the loop never terminates"])),
        "only the grounded finding must survive in the raw patch"
    );
}

/// (c) All findings are grounded: `None`, `findings` in the raw patch
/// untouched.
#[test]
fn review_grounded_findings_all_cited_is_none() {
    let cur = CodingWorkspace {
        seen: vec![SeenFile {
            path: "src/foo.rs".to_string(),
            lines: 10,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut out = parsed_output(json!({
        "patch": {
            "findings": ["src/foo.rs:1 the loop never terminates"],
        },
        "action": {"type": "done"},
        "ack_inbox": []
    }));

    let result = review_grounded_findings(true, &cur, &mut out);

    assert_eq!(result, None, "no finding was dropped, so this must be None");
    let patch_obj = out.patch().0.as_object().expect("patch must be an object");
    assert_eq!(
        patch_obj.get("findings"),
        Some(&json!(["src/foo.rs:1 the loop never terminates"])),
        "findings must be left exactly as sent"
    );
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
        true,
    )
    .expect("read_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 100]], "edited": [], "read_step": 7, "step": 7}]})
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
        read_step: Some(7),
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
        true,
    )
    .expect("second read should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 200]], "edited": [], "read_step": 9, "step": 9}]})
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
        read_step: Some(9),
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
        true,
    )
    .expect("edit_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 200]], "edited": [[150, 156]], "read_step": 9, "step": 10}]})
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
        true,
    )
    .expect("write_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "new.rs", "lines": 12, "read": [], "edited": [[1, 12]], "read_step": null, "step": 3}]})
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
        read_step: Some(9),
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
        true,
    )
    .expect("revert_file should produce a seen patch");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [], "edited": [], "read_step": null, "step": 11}]})
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
        read_step: None,
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
        true,
    )
    .expect("revert_file (removed) should still produce a seen patch");
    assert_eq!(patch, json!({"seen": []}));
}

#[test]
fn after_tool_other_tools_produce_no_patch() {
    // `test` is deliberately excluded here: with `tests_enabled = true` it
    // now always produces a `tests` patch (BACKLOG.md item 5, see
    // `after_tool_test_*` below); `tests_enabled = false` and no existing
    // `decision` covers the "nothing at all" case for `test` explicitly in
    // `after_tool_test_disabled_and_no_decision_produces_no_patch`.
    let ws = CodingWorkspace::default();
    for name in [
        "search",
        "list_files",
        "shell",
        "recall",
        "filter_last_tool_call",
        "think",
    ] {
        assert_eq!(
            common_after_tool(
                &ws,
                1,
                name,
                &json!({}),
                "irrelevant output",
                true,
                true,
                true
            ),
            None,
            "tool `{name}` should never produce a patch for unrelated output"
        );
    }
}

#[test]
fn after_tool_test_disabled_and_no_decision_produces_no_patch() {
    let ws = CodingWorkspace::default();
    assert_eq!(
        common_after_tool(
            &ws,
            1,
            "test",
            &json!({}),
            "irrelevant output",
            true,
            true,
            false
        ),
        None,
        "with tests_enabled = false and no existing decision, `test` should produce no patch \
         at all"
    );
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
            true,
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
            true,
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
            true,
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
        true,
    )
    .expect("a think conclusion should produce a decision patch");
    assert_eq!(
        patch,
        json!({"decision": {"question": "why does the test fail?", "answer": "the mock is stale.", "step": 7}})
    );
}

// =======================================================================
// BACKLOG.md item 4: `common_after_tool`'s `try_fixes` handling of
// `CodingWorkspace::decision` -- `try_fixes` is intercepted by
// `core::Session::execute_try_fixes` the same way `think` is, so this is
// unit-tested directly for the same reason as the `think` tests just above.
// =======================================================================

#[test]
fn after_tool_try_fixes_winner_output_sets_decision() {
    let ws = CodingWorkspace::default();
    let output = "try_fixes: 3 candidates; candidate 2 passed `cargo test` and was applied\n\
                  applied: src/tax.rs — use the 2024 bracket\n\
                  diff --git a/src/tax.rs b/src/tax.rs\n\
                  -old line\n\
                  +new line\n\
                  losers:\n\
                  1. src/tax.rs — try a different bracket -> assertion `left == right` failed\n\
                  3. src/tax.rs — a third guess -> not tried";
    let patch = common_after_tool(&ws, 9, "try_fixes", &json!({}), output, true, true, true)
        .expect("a try_fixes winner output should produce a decision patch");
    let decision = patch.get("decision").expect("decision key present");
    assert_eq!(
        decision.get("question").and_then(Value::as_str),
        Some("try_fixes: cargo test")
    );
    let answer = decision
        .get("answer")
        .and_then(Value::as_str)
        .expect("answer should be a string");
    assert!(
        answer.contains("candidate 2 passed `cargo test` and was applied"),
        "answer should keep the verdict line: {answer}"
    );
    assert!(
        answer.contains("applied: src/tax.rs — use the 2024 bracket"),
        "answer should keep the applied line: {answer}"
    );
    assert!(
        !answer.contains("diff --git"),
        "answer should drop the diff block: {answer}"
    );
    assert!(
        answer.contains("losers:") && answer.contains("assertion `left == right` failed"),
        "answer should keep the losers block: {answer}"
    );
    assert_eq!(decision.get("step").and_then(Value::as_u64), Some(9));
}

#[test]
fn after_tool_try_fixes_none_passed_output_sets_decision() {
    let ws = CodingWorkspace::default();
    let output = "try_fixes: 2 candidates; none passed `cargo test`\n\
                  1. src/tax.rs — guess 1 -> assertion failed: left != right\n\
                  2. src/tax.rs — guess 2 -> error: cannot find value `x`\n\
                  think:\n\
                  the real bug is in a third file";
    let patch = common_after_tool(&ws, 4, "try_fixes", &json!({}), output, true, true, true)
        .expect("a try_fixes none-passed output should produce a decision patch");
    let decision = patch.get("decision").expect("decision key present");
    assert_eq!(
        decision.get("question").and_then(Value::as_str),
        Some("try_fixes: cargo test")
    );
    let answer = decision
        .get("answer")
        .and_then(Value::as_str)
        .expect("answer should be a string");
    assert!(answer.contains("none passed `cargo test`"));
    assert!(answer.contains("think:\nthe real bug is in a third file"));
    assert_eq!(decision.get("step").and_then(Value::as_u64), Some(4));
}

#[test]
fn after_tool_try_fixes_not_configured_output_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "`try_fixes` is not configured: set a `fixer` section in the config (see \
                  README, Config). Decide from what you have.";
    assert_eq!(
        common_after_tool(&ws, 1, "try_fixes", &json!({}), output, true, true, true),
        None,
        "a not-configured try_fixes output must leave an existing decision unchanged (no patch)"
    );
}

#[test]
fn after_tool_try_fixes_decision_disabled_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "try_fixes: 1 candidates; none passed `cargo test`\n\
                  1. src/tax.rs — guess 1 -> assertion failed";
    assert_eq!(
        common_after_tool(&ws, 1, "try_fixes", &json!({}), output, true, false, true),
        None,
        "decision_enabled = false must suppress the decision patch a try_fixes output would \
         otherwise produce"
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
            true,
            true
        ),
        None,
        "a think-failed output must leave an existing decision unchanged (no patch)"
    );
}

#[test]
fn after_tool_test_pass_clears_an_existing_decision() {
    // `tests_enabled = false` isolates this test to the `decision`-clearing
    // half of a `test` call (see `after_tool_test_pass_*` below,
    // BACKLOG.md item 5, for the `tests` half).
    let ws = CodingWorkspace {
        decision: Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        ..Default::default()
    };
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    let patch = common_after_tool(&ws, 5, "test", &json!({}), output, true, true, false)
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
        common_after_tool(&ws, 5, "test", &json!({}), output, true, true, false),
        None,
        "a failing test must leave an existing decision alone (no patch)"
    );
}

#[test]
fn after_tool_test_pass_with_no_existing_decision_produces_no_patch() {
    let ws = CodingWorkspace::default();
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    assert_eq!(
        common_after_tool(&ws, 5, "test", &json!({}), output, true, true, false),
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
            true,
        ),
        None,
        "decision_enabled = false must suppress the decision patch a think output would \
         otherwise produce"
    );
}

#[test]
fn after_tool_decision_disabled_passing_test_with_existing_decision_produces_no_patch() {
    // `tests_enabled = false` isolates this to the `decision` half; see
    // `after_tool_test_pass_*` for the `tests_enabled` switch's own tests.
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
        common_after_tool(&ws, 5, "test", &json!({}), output, true, false, false),
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
        true,
    )
    .expect("seen_enabled = true must still record seen when decision_enabled is false");
    assert_eq!(
        patch,
        json!({"seen": [{"path": "src/a.rs", "lines": 250, "read": [[1, 100]], "edited": [], "read_step": 7, "step": 7}]})
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
        read_step: Some(1),
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

#[test]
fn model_patch_touching_tests_is_stripped_and_noted() {
    let skill = ImplementSkill::new();
    let cur_ws = CodingWorkspace {
        tests: Some(TestRecord {
            cmd: "cargo test".to_string(),
            pass: true,
            failing: vec![],
            first_error: None,
            step: 1,
        }),
        ..CodingWorkspace::default()
    };
    let cur = committed(cur_ws.clone());

    let content = json!({
        "patch": {
            "tests": {"cmd": "sh -c true", "pass": false, "failing": ["x"], "first_error": null, "step": 99},
            "plan": [{"id": 1, "text": "do the thing", "status": "done"}],
        },
        "action": {"type": "tool", "name": "list_files", "args": {}},
    });
    let step =
        parse_and_validate(&skill, &cur, content).expect("patch should be kept, not rejected");

    assert_eq!(
        step.patch().next().tests,
        cur_ws.tests,
        "`tests` must be left unchanged by the dropped patch"
    );
    assert_eq!(
        step.patch().next().plan.len(),
        1,
        "the real plan-step change must land"
    );
    assert_eq!(
        step.notes(),
        &[
            "`tests` is kept by the harness; that part of your patch was dropped and the rest \
             was kept."
                .to_string()
        ]
    );
}

// =======================================================================
// `CodingWorkspace::tests` (BACKLOG.md item 5): `failing_test_names`/
// `first_error_line` (pure functions) and `common_after_tool`'s `tests`
// patch.
// =======================================================================

#[test]
fn failing_test_names_cargo_single_failure() {
    let output = "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                  test tests::adds ... FAILED\n\nfailures:\n\n---- tests::adds stdout ----\n\
                  thread 'tests::adds' panicked at src/lib.rs:5:5:\nassertion failed\n\n\
                  failures:\n    tests::adds\n\ntest result: FAILED. 0 passed; 1 failed\n";
    assert_eq!(failing_test_names(output), vec!["tests::adds".to_string()]);
}

#[test]
fn failing_test_names_cargo_three_failures_with_failures_list() {
    let output = "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                  test tests::a ... FAILED\ntest tests::b ... FAILED\ntest tests::c ... FAILED\n\n\
                  failures:\n    tests::a\n    tests::b\n    tests::c\n\n\
                  test result: FAILED. 0 passed; 3 failed\n";
    assert_eq!(
        failing_test_names(output),
        vec![
            "tests::a".to_string(),
            "tests::b".to_string(),
            "tests::c".to_string()
        ],
        "the `failures:` list's plain names must not duplicate the `... FAILED` names"
    );
}

#[test]
fn failing_test_names_cargo_compile_error_no_tests_run() {
    let output = "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                  error[E0433]: failed to resolve: use of undeclared crate\n \
                  --> src/lib.rs:3:5\n";
    assert_eq!(
        failing_test_names(output),
        Vec::<String>::new(),
        "a compile error with no tests run has no failing test names"
    );
}

#[test]
fn failing_test_names_unittest_fail_and_error() {
    let output = "command: python -m unittest\nresult: fail (exit 1)\n\
                  --- failure summary ---\n\
                  FAIL: test_adds (mymod.MyTest)\nERROR: test_setup (mymod.OtherTest)\n";
    assert_eq!(
        failing_test_names(output),
        vec![
            "mymod.MyTest.test_adds".to_string(),
            "mymod.OtherTest.test_setup".to_string(),
        ]
    );
}

#[test]
fn failing_test_names_pytest() {
    let output = "command: pytest\nresult: fail (exit 1)\n--- failure summary ---\n\
                  FAILED tests/test_x.py::test_adds - AssertionError\n\
                  FAILED tests/test_y.py::test_other\n";
    assert_eq!(
        failing_test_names(output),
        vec![
            "tests/test_x.py::test_adds".to_string(),
            "tests/test_y.py::test_other".to_string(),
        ]
    );
}

#[test]
fn failing_test_names_all_pass_is_empty() {
    let output = "command: cargo test\nresult: pass\n\n(12 passing test lines omitted)\n\
                  test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
    assert_eq!(failing_test_names(output), Vec::<String>::new());
}

#[test]
fn failing_test_names_caps_at_eight_and_dedupes() {
    let mut output = String::from("command: cargo test\nresult: fail (exit 101)\n");
    for i in 0..12 {
        output.push_str(&format!("test tests::t{i} ... FAILED\n"));
    }
    // A repeat of an already-collected name must not grow the cap-limited
    // vec or count twice.
    output.push_str("test tests::t0 ... FAILED\n");
    let names = failing_test_names(&output);
    assert_eq!(names.len(), 8, "at most 8 names: {names:?}");
    assert_eq!(
        names,
        vec![
            "tests::t0",
            "tests::t1",
            "tests::t2",
            "tests::t3",
            "tests::t4",
            "tests::t5",
            "tests::t6",
            "tests::t7"
        ]
    );
}

#[test]
fn first_error_line_cargo_panic() {
    let output = "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                  thread 'tests::adds' (0x1234) panicked at src/lib.rs:5:5:\n\
                  assertion `left == right` failed\n  left: 3\n right: 4\n";
    assert_eq!(
        first_error_line(output),
        Some("thread 'tests::adds' (0x1234) panicked at src/lib.rs:5:5:".to_string())
    );
}

#[test]
fn first_error_line_rustc_error() {
    let output = "command: cargo build\nresult: fail (exit 101)\n--- failure summary ---\n\
                  error[E0433]: failed to resolve: use of undeclared crate or module `foo`\n \
                  --> src/lib.rs:3:5\n";
    assert_eq!(
        first_error_line(output),
        Some(
            "error[E0433]: failed to resolve: use of undeclared crate or module `foo`".to_string()
        )
    );
}

#[test]
fn first_error_line_pytest_traceback() {
    let output = "command: pytest\nresult: fail (exit 1)\n--- failure summary ---\n\
                  Traceback (most recent call last):\n  File \"test_x.py\", line 3\n\
                  AssertionError: expected 1, got 2\n";
    assert_eq!(
        first_error_line(output),
        Some("Traceback (most recent call last):".to_string())
    );
}

#[test]
fn first_error_line_caps_at_200_chars_with_ellipsis() {
    let long = "error: ".to_string() + &"x".repeat(300);
    let output = format!("command: cargo build\nresult: fail (exit 1)\n{long}\n");
    let line = first_error_line(&output).expect("should find the error line");
    assert_eq!(line.chars().count(), 201, "200 chars + the trailing …");
    assert!(line.ends_with('…'));
    assert!(line.starts_with("error: "));
}

#[test]
fn first_error_line_all_pass_is_none() {
    let output = "command: cargo test\nresult: pass\n\ntest result: ok. 3 passed; 0 failed\n";
    assert_eq!(first_error_line(output), None);
}

#[test]
fn after_tool_test_pass_writes_tests_record() {
    let ws = CodingWorkspace::default();
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    let patch = common_after_tool(
        &ws,
        5,
        "test",
        &json!({"cmd": "cargo test"}),
        output,
        true,
        true,
        true,
    )
    .expect("a test call should always write a tests patch when tests_enabled");
    assert_eq!(
        patch,
        json!({
            "tests": {
                "cmd": "cargo test",
                "pass": true,
                "failing": [],
                "first_error": null,
                "step": 5,
            }
        })
    );
}

#[test]
fn after_tool_test_fail_writes_tests_record_with_names_and_error() {
    let ws = CodingWorkspace::default();
    let output = "command: cargo test\nresult: fail (exit 101)\n--- failure summary ---\n\
                  test tests::adds ... FAILED\n\nfailures:\n    tests::adds\n\n\
                  ---- tests::adds stdout ----\n\
                  thread 'tests::adds' panicked at src/lib.rs:5:5:\n\
                  assertion `left == right` failed\n\n\
                  test result: FAILED. 0 passed; 1 failed\n";
    let patch = common_after_tool(
        &ws,
        7,
        "test",
        &json!({"cmd": "cargo test"}),
        output,
        true,
        true,
        true,
    )
    .expect("a failing test call should write a tests patch");
    assert_eq!(
        patch,
        json!({
            "tests": {
                "cmd": "cargo test",
                "pass": false,
                "failing": ["tests::adds"],
                "first_error": "thread 'tests::adds' panicked at src/lib.rs:5:5:",
                "step": 7,
            }
        })
    );
}

#[test]
fn after_tool_test_disabled_writes_no_tests_patch() {
    let ws = CodingWorkspace::default();
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    assert_eq!(
        common_after_tool(
            &ws,
            5,
            "test",
            &json!({"cmd": "cargo test"}),
            output,
            true,
            true,
            false
        ),
        None,
        "tests_enabled = false must suppress the tests patch entirely when there is no \
         existing decision to clear either"
    );
}

#[test]
fn after_tool_test_pass_clears_decision_and_writes_tests_in_one_patch() {
    let ws = CodingWorkspace {
        decision: Some(Decision {
            question: "why?".to_string(),
            answer: "because.".to_string(),
            step: 0,
        }),
        ..Default::default()
    };
    let output = "command: cargo test\nresult: pass\nexit: 0\n";
    let patch = common_after_tool(
        &ws,
        5,
        "test",
        &json!({"cmd": "cargo test"}),
        output,
        true,
        true,
        true,
    )
    .expect("both decision-clearing and tests should apply");
    assert_eq!(
        patch,
        json!({
            "decision": null,
            "tests": {
                "cmd": "cargo test",
                "pass": true,
                "failing": [],
                "first_error": null,
                "step": 5,
            }
        })
    );
}

#[test]
fn after_tool_test_uses_command_alias_when_cmd_absent() {
    let ws = CodingWorkspace::default();
    let output = "command: pytest\nresult: pass\nexit: 0\n";
    let patch = common_after_tool(
        &ws,
        2,
        "test",
        &json!({"command": "pytest"}),
        output,
        true,
        true,
        true,
    )
    .expect("a test call should write a tests patch");
    assert_eq!(patch["tests"]["cmd"], json!("pytest"));
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

/// A `last_failure_summary` that `classify_failure` classifies as
/// `FailureKind::Assertion`, so `repeat_failure_question` returns
/// `REPEAT_FAILURE_QUESTION` -- used by the substitution-mechanism tests
/// below, which assert on that exact question text and predate
/// `classify_failure`'s existence (BACKLOG.md item 18).
const ASSERTION_SUMMARY: &str = "assertion failed: `(left == right)`\n  left: 1\n right: 2";

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
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

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

// =======================================================================
// BACKLOG.md item 4: with `try_fixes_available`, the trigger substitutes
// `try_fixes {}` instead of `think`; without it, `think` as before.
// =======================================================================

#[test]
fn repeat_failure_thinks_substitutes_try_fixes_when_available() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.try_fixes_available = true;

    let step = parse_and_validate(&skill, &cur, edit_file_turn_content())
        .expect("the substituted try_fixes call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "try_fixes" && args == &json!({})
        ),
        "expected a substituted `try_fixes {{}}` call, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[REPEAT_THINK_NOTE.to_string()],
        "the note text is unchanged by which tool it substitutes"
    );
}

#[test]
fn repeat_failure_thinks_substitutes_think_when_try_fixes_unavailable() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.try_fixes_available = false;
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

    let step = parse_and_validate(&skill, &cur, edit_file_turn_content())
        .expect("the substituted think call must not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "think"
                    && args == &json!({"question": super::REPEAT_FAILURE_QUESTION})
        ),
        "expected a substituted `think` call when no fixer is attached, got {:?}",
        step.action()
    );
}

#[test]
fn repeat_failure_thinks_fires_for_write_file_with_no_prior_think() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

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
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

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
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

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

// ---------------------------------------------------------------------
// `implement_early_park_thinks` (DECISIONS.md D4, BACKLOG.md item 8): the
// SECOND consecutive non-question `ask_user`, with no repeated test
// failure open, is substituted for `think` one strike before
// `CONTINUE_REPEAT_CAP` (`core/src/session.rs`) would park the run on the
// third.
// ---------------------------------------------------------------------

/// The exact note `implement_early_park_thinks` attaches.
const EARLY_PARK_THINK_NOTE: &str = "you sent ask_user with nothing to ask twice in a row, so \
     the harness asked the reasoning subagent before a third repeat could park the run (its \
     conclusion is below, and in `decision`). Act on it.";

/// The second consecutive non-question `ask_user`
/// (`consecutive_non_question_asks >= 1`, meaning the previous step was
/// already one) with no repeated test failure open and no `think` call
/// having run: must fire.
#[test]
fn early_park_thinks_fires_on_the_second_consecutive_non_question_ask() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 1;
    cur.harness.repeated_failure_step = None;
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
                    && args == &json!({"question": super::EARLY_PARK_QUESTION})
        ),
        "expected a substituted `think` call, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[EARLY_PARK_THINK_NOTE.to_string()],
        "the early-park think note must be attached"
    );
}

/// The FIRST non-question `ask_user`
/// (`consecutive_non_question_asks == 0`, meaning the previous step was
/// NOT one) must not fire -- only the second in a row does.
#[test]
fn early_park_thinks_does_not_fire_on_the_first_non_question_ask() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 0;
    cur.harness.repeated_failure_step = None;
    cur.harness.last_think_step = None;

    let content = json!({
        "patch": {},
        "action": {"type": "ask_user", "message": "I will look at tax.rs next"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    // `ask_user_statement_note` (via `premature_done_note`'s fallthrough)
    // still converts a non-question `ask_user` into `Continue` -- just not
    // the `think` substitution.
    assert!(
        matches!(step.action(), statie_core::Action::Continue { .. }),
        "expected a plain Continue conversion (not think), got {:?}",
        step.action()
    );
    if let statie_core::Action::Continue { note } = step.action() {
        assert_ne!(
            note, EARLY_PARK_THINK_NOTE,
            "must not use the early-park note on the first non-question ask"
        );
    }
}

/// A real question (contains `?`) must never be substituted, even with
/// `consecutive_non_question_asks >= 1` -- the field only ever climbs from
/// a non-question `ask_user` in the first place (`core/src/session.rs`),
/// but the trigger itself must also refuse to act on a real question.
#[test]
fn early_park_thinks_does_not_fire_when_the_message_contains_a_question_mark() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 1;
    cur.harness.repeated_failure_step = None;
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

/// When a `think` call has already run and no tool call (`seen` entry) has
/// landed since it, the trigger must not fire again -- "once per streak",
/// the same rule `implement_repeat_failure_thinks` enforces with
/// `last_think_step`.
#[test]
fn early_park_thinks_does_not_fire_when_a_think_already_ran_since_the_last_tool_call() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 1;
    cur.harness.repeated_failure_step = None;
    cur.harness.last_think_step = Some(5);
    // No `seen` entry past step 5: nothing has been edited/read since the
    // last `think` call.
    cur.agent.seen = vec![SeenFile {
        path: "a.rs".to_string(),
        step: 5,
        ..Default::default()
    }];

    let content = json!({
        "patch": {},
        "action": {"type": "ask_user", "message": "I will look at tax.rs next"},
        "ack_inbox": []
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    assert!(
        !matches!(step.action(), statie_core::Action::Tool { name, .. } if name == "think"),
        "must not substitute think again with nothing new since the last think call, got {:?}",
        step.action()
    );
}

/// Once a tool call has landed after the last `think` call (a fresh `seen`
/// entry past `last_think_step`), the trigger re-arms for the next stuck
/// streak.
#[test]
fn early_park_thinks_fires_again_after_a_tool_call_resets_the_streak() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 1;
    cur.harness.repeated_failure_step = None;
    cur.harness.last_think_step = Some(5);
    // A `seen` entry past step 5: a tool call has landed since the last
    // `think` call, so the trigger re-arms.
    cur.agent.seen = vec![SeenFile {
        path: "a.rs".to_string(),
        step: 8,
        ..Default::default()
    }];

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
                    && args == &json!({"question": super::EARLY_PARK_QUESTION})
        ),
        "expected a substituted `think` call after the streak reset, got {:?}",
        step.action()
    );
    assert_eq!(step.notes(), &[EARLY_PARK_THINK_NOTE.to_string()]);
}

/// When a repeated test failure is ALSO open, `implement_repeat_failure_
/// thinks` takes precedence (it runs first in `ImplementSkill::validate`)
/// -- the substituted action is still `think` with `REPEAT_FAILURE_
/// QUESTION`, not `EARLY_PARK_QUESTION`, and `implement_early_park_thinks`
/// never gets a chance to act (the action is no longer `AskUser` by the
/// time it would run).
#[test]
fn repeat_failure_trigger_takes_precedence_over_early_park_when_both_conditions_hold() {
    let skill = ImplementSkill::new();
    let mut cur = committed(CodingWorkspace::default());
    cur.harness.consecutive_non_question_asks = 1;
    cur.harness.repeated_failure_step = Some(10);
    cur.harness.last_think_step = None;
    cur.harness.last_failure_summary = Some(ASSERTION_SUMMARY.to_string());

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
        "the repeat-failure trigger must take precedence, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[ASK_USER_THINK_NOTE.to_string()],
        "the repeat-failure note must be attached, not the early-park one"
    );
}

// ---------------------------------------------------------------------
// `classify_failure`/`repeat_failure_question` (`skills/src/coding/mod.rs`,
// DECISIONS.md D9, BACKLOG.md item 18): the repeated-failure question is
// shaped to the kind of failure instead of asking every repeat the same
// arithmetic-trace question.
// ---------------------------------------------------------------------

use super::{FailureKind, classify_failure, repeat_failure_question};

#[test]
fn classify_failure_recognizes_a_rustc_compile_error() {
    let summary = "error[E0308]: mismatched types\n --> src/lib.rs:3:5\n  |\n3 |     5\n  |     ^ \
                    expected `()`, found integer";
    assert_eq!(classify_failure(summary), FailureKind::CompileError);
}

#[test]
fn classify_failure_recognizes_a_python_name_error() {
    let summary = "NameError: name 'tax_rate' is not defined";
    assert_eq!(classify_failure(summary), FailureKind::CompileError);
}

#[test]
fn classify_failure_recognizes_an_assertion_with_numbers() {
    let summary = "assertion `left == right` failed\n  left: 4600\n right: 4140";
    assert_eq!(classify_failure(summary), FailureKind::Assertion);
}

#[test]
fn classify_failure_recognizes_a_panic_without_numbers() {
    let summary = "thread 'main' panicked at src/lib.rs:10:5:\ncalled `Option::unwrap()` on a \
                    `None` value";
    assert_eq!(classify_failure(summary), FailureKind::Panic);
}

#[test]
fn classify_failure_treats_a_digit_free_assertion_panic_as_panic_not_assertion() {
    // "assertion failed: a == b" contains the word "assertion" but no
    // digit ANYWHERE in the summary (no file:line location either) and no
    // left:/right:/assert_eq -- a digit must appear somewhere in the
    // summary for the `assertion`/`AssertionError`/`assert_eq`/`left:`/
    // `right:`/`expected` needles to count as an assertion, and this
    // summary has none, so it must fall through to the `panicked at`
    // check instead.
    let summary = "thread 'main' panicked at 'assertion failed: a == b', src/lib.rs";
    assert_eq!(classify_failure(summary), FailureKind::Panic);
}

#[test]
fn classify_failure_recognizes_a_timeout() {
    let summary = "test slow_test has been running for over 60 seconds\ntimed out after 600s";
    assert_eq!(classify_failure(summary), FailureKind::Timeout);
}

#[test]
fn classify_failure_empty_string_is_other() {
    assert_eq!(classify_failure(""), FailureKind::Other);
}

#[test]
fn repeat_failure_question_for_assertion_is_the_original_constant() {
    assert_eq!(
        repeat_failure_question(FailureKind::Assertion),
        super::REPEAT_FAILURE_QUESTION
    );
}

// ---------------------------------------------------------------------
// `shell_read_as_read_file` (BACKLOG.md item 16): a pure `shell` command
// classifier that recognises a single-file, pipe-free, redirect-free read
// and returns the `read_file` call it is equivalent to.
// ---------------------------------------------------------------------

fn args(path: &str, offset: Option<u64>, limit: Option<u64>) -> ReadFileArgs {
    ReadFileArgs {
        path: path.to_string(),
        offset,
        limit,
    }
}

#[test]
fn cat_plain_path_is_a_plain_read() {
    assert_eq!(
        shell_read_as_read_file("cat pricing.py"),
        Some(args("pricing.py", None, None))
    );
}

#[test]
fn cat_dash_n_is_a_plain_read() {
    assert_eq!(
        shell_read_as_read_file("cat -n pricing.py"),
        Some(args("pricing.py", None, None))
    );
}

#[test]
fn nl_is_a_plain_read() {
    assert_eq!(
        shell_read_as_read_file("nl pricing.py"),
        Some(args("pricing.py", None, None))
    );
}

#[test]
fn head_with_no_flag_defaults_to_limit_ten() {
    assert_eq!(
        shell_read_as_read_file("head a.rs"),
        Some(args("a.rs", None, Some(10)))
    );
}

#[test]
fn head_dash_n_number_maps_to_offset_one_limit_n() {
    assert_eq!(
        shell_read_as_read_file("head -n 5 a.rs"),
        Some(args("a.rs", Some(1), Some(5)))
    );
}

#[test]
fn head_dash_number_maps_to_offset_one_limit_n() {
    assert_eq!(
        shell_read_as_read_file("head -20 a.rs"),
        Some(args("a.rs", Some(1), Some(20)))
    );
}

#[test]
fn tail_with_no_flag_is_a_plain_read() {
    // The harness cannot know the file's length ahead of time, so this
    // cannot be turned into an offset -- treated as a plain read.
    assert_eq!(
        shell_read_as_read_file("tail a.rs"),
        Some(args("a.rs", None, None))
    );
}

#[test]
fn tail_dash_n_number_is_a_plain_read() {
    assert_eq!(
        shell_read_as_read_file("tail -n 5 a.rs"),
        Some(args("a.rs", None, None))
    );
}

#[test]
fn tail_dash_number_is_a_plain_read() {
    assert_eq!(
        shell_read_as_read_file("tail -5 a.rs"),
        Some(args("a.rs", None, None))
    );
}

#[test]
fn sed_range_single_quoted() {
    assert_eq!(
        shell_read_as_read_file("sed -n '10,20p' src/a.rs"),
        Some(args("src/a.rs", Some(10), Some(11)))
    );
}

#[test]
fn sed_range_double_quoted() {
    assert_eq!(
        shell_read_as_read_file("sed -n \"10,20p\" src/a.rs"),
        Some(args("src/a.rs", Some(10), Some(11)))
    );
}

#[test]
fn sed_range_unquoted() {
    assert_eq!(
        shell_read_as_read_file("sed -n 10,20p src/a.rs"),
        Some(args("src/a.rs", Some(10), Some(11)))
    );
}

#[test]
fn cat_with_single_quoted_path() {
    assert_eq!(
        shell_read_as_read_file("cat 'src/a.rs'"),
        Some(args("src/a.rs", None, None))
    );
}

#[test]
fn cat_with_double_quoted_path() {
    assert_eq!(
        shell_read_as_read_file("cat \"src/a.rs\""),
        Some(args("src/a.rs", None, None))
    );
}

#[test]
fn leading_and_trailing_whitespace_is_trimmed() {
    assert_eq!(
        shell_read_as_read_file("   cat a.rs   "),
        Some(args("a.rs", None, None))
    );
}

// --- negatives ---

#[test]
fn cat_piped_to_grep_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat a.rs | grep x"), None);
}

#[test]
fn cat_two_paths_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat a.rs b.rs"), None);
}

#[test]
fn cat_redirect_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat > a.rs"), None);
}

#[test]
fn cat_semicolon_chain_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat a.rs; ls"), None);
}

#[test]
fn cat_glob_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat *.rs"), None);
}

#[test]
fn cat_dash_dash_weird_path_is_rejected() {
    assert_eq!(shell_read_as_read_file("cat -- -weird"), None);
}

#[test]
fn ls_is_rejected() {
    assert_eq!(shell_read_as_read_file("ls"), None);
}

#[test]
fn cargo_test_is_rejected() {
    assert_eq!(shell_read_as_read_file("cargo test"), None);
}

#[test]
fn sed_in_place_edit_is_rejected() {
    assert_eq!(shell_read_as_read_file("sed -i 's/a/b/' x"), None);
}

#[test]
fn head_piped_to_tail_is_rejected() {
    assert_eq!(shell_read_as_read_file("head -n 5 a.rs | tail -2"), None);
}

#[test]
fn empty_string_is_rejected() {
    assert_eq!(shell_read_as_read_file(""), None);
}

// ---------------------------------------------------------------------
// `shell_read_substitution` wired into `PlanSkill`/`ImplementSkill`/
// `ReviewSkill::validate` (BACKLOG.md item 16): a `shell` action recognised
// by `shell_read_as_read_file` is replaced with the equivalent `read_file`
// call, with the substitution note attached; any other `shell` command
// commits unchanged.
// ---------------------------------------------------------------------

/// The exact note `shell_read_substitution` attaches, for the given `cmd`.
fn shell_read_note_for(cmd: &str) -> String {
    format!(
        "`shell {cmd}` was a plain read, so the harness ran it as `read_file` instead: the \
         read pages, lands in `seen`, and counts for the read guards. Use `read_file` directly \
         next time."
    )
}

fn shell_turn_content(cmd: &str) -> serde_json::Value {
    json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "shell",
            "args": {"cmd": cmd},
        },
    })
}

#[test]
fn implement_substitutes_read_file_for_shell_cat() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cat src/a.rs"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "read_file" && args == &json!({"path": "src/a.rs"})
        ),
        "expected a substituted read_file call, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[shell_read_note_for("cat src/a.rs")],
        "expected the substitution note"
    );
}

#[test]
fn implement_leaves_shell_cargo_test_untouched() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cargo test"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "shell" && args == &json!({"cmd": "cargo test"})
        ),
        "a non-read shell command must commit unchanged, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

#[test]
fn implement_substitution_carries_offset_and_limit_when_present() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("sed -n '10,20p' src/a.rs"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "read_file"
                    && args == &json!({"path": "src/a.rs", "offset": 10, "limit": 11})
        ),
        "expected offset/limit to be carried through, got {:?}",
        step.action()
    );
    assert_eq!(
        step.notes(),
        &[shell_read_note_for("sed -n '10,20p' src/a.rs")]
    );
}

#[test]
fn plan_substitutes_read_file_for_shell_cat() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cat src/a.rs"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "read_file" && args == &json!({"path": "src/a.rs"})
        ),
        "expected a substituted read_file call, got {:?}",
        step.action()
    );
    assert_eq!(step.notes(), &[shell_read_note_for("cat src/a.rs")]);
}

#[test]
fn plan_leaves_shell_cargo_test_untouched() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cargo test"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "shell" && args == &json!({"cmd": "cargo test"})
        ),
        "a non-read shell command must commit unchanged, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

#[test]
fn review_substitutes_read_file_for_shell_cat() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cat src/a.rs"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "read_file" && args == &json!({"path": "src/a.rs"})
        ),
        "expected a substituted read_file call, got {:?}",
        step.action()
    );
    assert_eq!(step.notes(), &[shell_read_note_for("cat src/a.rs")]);
}

#[test]
fn review_leaves_shell_cargo_test_untouched() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(&skill, &cur, shell_turn_content("cargo test"))
        .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            statie_core::Action::Tool { name, args }
                if name == "shell" && args == &json!({"cmd": "cargo test"})
        ),
        "a non-read shell command must commit unchanged, got {:?}",
        step.action()
    );
    assert!(step.notes().is_empty());
}

// ---------------------------------------------------------------------
// `write_file_confirmed_by_seen` (BACKLOG.md item 15): a `write_file` on an
// existing file past `tools/src/write_file.rs`'s line cap is allowed
// without a rewrite-from-memory risk when `seen` already shows a full read
// of that file since the last time any part of it was edited. The rule:
// `read` (coalesced) covers every line 1..=lines, AND either `edited` is
// empty (never edited this session) or `read_step == Some(entry.step)`
// (the entry's most recent change was that full read, i.e. it came after
// whatever edit(s) preceded it) -- see `write_file_confirmed_by_seen`'s doc.
// ---------------------------------------------------------------------

fn seen_file(
    path: &str,
    lines: u32,
    read: Vec<[u32; 2]>,
    edited: Vec<[u32; 2]>,
    read_step: Option<u64>,
    step: u64,
) -> SeenFile {
    SeenFile {
        path: path.to_string(),
        lines,
        read,
        edited,
        read_step,
        step,
    }
}

#[test]
fn confirmed_true_for_a_single_range_covering_the_whole_file() {
    let seen = vec![seen_file("a.rs", 62, vec![[1, 62]], vec![], Some(1), 1)];
    assert!(write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_true_for_coalesced_ranges_covering_the_whole_file() {
    let seen = vec![seen_file(
        "a.rs",
        62,
        vec![[1, 30], [31, 62]],
        vec![],
        Some(1),
        1,
    )];
    assert!(write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_false_when_the_read_is_one_line_short() {
    let seen = vec![seen_file("a.rs", 62, vec![[1, 61]], vec![], Some(1), 1)];
    assert!(!write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_false_when_edited_is_non_empty_and_the_last_change_was_an_edit() {
    // The full read (step 1) predates the edit (step 2, the entry's
    // current `step`): the last change to this entry was the edit, not the
    // read, so this must be refused.
    let seen = vec![seen_file(
        "a.rs",
        62,
        vec![[1, 62]],
        vec![[10, 12]],
        Some(1),
        2,
    )];
    assert!(!write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_true_when_edited_is_non_empty_but_the_last_change_was_the_read() {
    // An edit (step 1) followed by a full reread (step 2, the entry's
    // current `step`, and also `read_step`): `edited` is left non-empty
    // (`edit_file` never clears it) but the full read came after the edit,
    // so this must be confirmed.
    let seen = vec![seen_file(
        "a.rs",
        62,
        vec![[1, 62]],
        vec![[10, 12]],
        Some(2),
        2,
    )];
    assert!(write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_false_when_last_change_was_the_read_but_coverage_is_short() {
    // `read_step == step` (the read was the latest change) but the read
    // itself doesn't cover the whole file -- coverage still governs.
    let seen = vec![seen_file(
        "a.rs",
        62,
        vec![[1, 61]],
        vec![[10, 12]],
        Some(2),
        2,
    )];
    assert!(!write_file_confirmed_by_seen(&seen, "a.rs"));
}

#[test]
fn confirmed_false_for_a_path_not_in_seen() {
    let seen = vec![seen_file("other.rs", 62, vec![[1, 62]], vec![], Some(1), 1)];
    assert!(!write_file_confirmed_by_seen(&seen, "a.rs"));
    assert!(!write_file_confirmed_by_seen(&[], "a.rs"));
}

// ---------------------------------------------------------------------
// `write_file_auto_confirm` wired into `PlanSkill`/`ImplementSkill`/
// `ReviewSkill::validate` (BACKLOG.md item 15): on a `write_file` action,
// strips a model-supplied `confirmed_full_read` unconditionally, then sets
// it back to `true` only when `write_file_confirmed_by_seen` says so --
// silently, no note either way.
// ---------------------------------------------------------------------

fn write_file_confirm_turn_content(path: &str, extra_args: serde_json::Value) -> serde_json::Value {
    let mut args = json!({"path": path, "content": "new content"});
    if let (Value::Object(map), Value::Object(extra)) = (&mut args, extra_args) {
        for (k, v) in extra {
            map.insert(k, v);
        }
    }
    json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "write_file",
            "args": args,
        },
    })
}

#[test]
fn implement_sets_confirmed_full_read_on_a_fully_seen_path() {
    let skill = ImplementSkill::new();
    let ws = CodingWorkspace {
        seen: vec![seen_file("a.rs", 62, vec![[1, 62]], vec![], Some(1), 1)],
        ..Default::default()
    };
    let cur = committed(ws);

    let step = parse_and_validate(
        &skill,
        &cur,
        write_file_confirm_turn_content("a.rs", json!({})),
    )
    .expect("should not be rejected");
    assert!(
        matches!(
            step.action(),
            Action::Tool { name, args }
                if name == "write_file"
                    && args.get("confirmed_full_read") == Some(&Value::Bool(true))
        ),
        "expected confirmed_full_read: true, got {:?}",
        step.action()
    );
}

#[test]
fn implement_does_not_set_confirmed_full_read_on_an_unseen_path() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(
        &skill,
        &cur,
        write_file_confirm_turn_content("a.rs", json!({})),
    )
    .expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert!(
                args.get("confirmed_full_read").is_none(),
                "expected no confirmed_full_read key, got {args:?}"
            );
        }
        other => panic!("expected a write_file tool call, got {other:?}"),
    }
}

#[test]
fn implement_strips_a_model_supplied_confirmed_full_read_on_an_unseen_path() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());

    let step = parse_and_validate(
        &skill,
        &cur,
        write_file_confirm_turn_content("a.rs", json!({"confirmed_full_read": true})),
    )
    .expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert!(
                args.get("confirmed_full_read").is_none(),
                "a model-supplied confirmed_full_read on an unseen path must be stripped, got {args:?}"
            );
        }
        other => panic!("expected a write_file tool call, got {other:?}"),
    }
}

#[test]
fn implement_strips_a_model_supplied_confirmed_full_read_even_on_a_seen_path() {
    // The model must never be trusted to set the flag itself, even when the
    // harness would have set it too -- the harness's own decision, not the
    // model's claim, is what reaches the tool.
    let skill = ImplementSkill::new();
    let ws = CodingWorkspace {
        seen: vec![seen_file("a.rs", 62, vec![[1, 62]], vec![], Some(1), 1)],
        ..Default::default()
    };
    let cur = committed(ws);

    let step = parse_and_validate(
        &skill,
        &cur,
        write_file_confirm_turn_content("a.rs", json!({"confirmed_full_read": false})),
    )
    .expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert_eq!(
                args.get("confirmed_full_read"),
                Some(&Value::Bool(true)),
                "the harness's own true must win over the model's false, got {args:?}"
            );
        }
        other => panic!("expected a write_file tool call, got {other:?}"),
    }
}

#[test]
fn plan_write_file_auto_confirm_is_a_no_op_without_write_file_tool() {
    // `plan` has no `write_file` tool, but `write_file_auto_confirm` still
    // runs harmlessly in its `validate` (it only acts on a `Tool { name:
    // "write_file", .. }` action, which `plan` never legitimately produces).
    let skill = PlanSkill::new();
    let ws = CodingWorkspace {
        seen: vec![seen_file("a.rs", 62, vec![[1, 62]], vec![], Some(1), 1)],
        ..Default::default()
    };
    let cur = committed(ws);

    let step = parse_and_validate(
        &skill,
        &cur,
        write_file_confirm_turn_content("a.rs", json!({})),
    )
    .expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "write_file");
            assert_eq!(args.get("confirmed_full_read"), Some(&Value::Bool(true)));
        }
        other => panic!("expected a write_file tool call, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Unit 32: `infer_tool_name` -- every arm and the `None` cases.
// ---------------------------------------------------------------------

#[test]
fn infer_tool_name_question_present_is_think() {
    assert_eq!(infer_tool_name(&json!({"question": "why?"})), Some("think"));
}

#[test]
fn infer_tool_name_question_present_with_other_keys_is_still_think() {
    // "any other keys ignored" -- question wins even alongside a path/new
    // shape that would otherwise read as edit_file.
    assert_eq!(
        infer_tool_name(&json!({"question": "why?", "path": "a.rs", "new": "x"})),
        Some("think")
    );
}

#[test]
fn infer_tool_name_path_and_content_no_new_is_write_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "content": "hello"})),
        Some("write_file")
    );
}

#[test]
fn infer_tool_name_path_content_and_new_is_not_write_file() {
    // `new` present disqualifies the write_file arm even with `content` also
    // present.
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "content": "hello", "new": "x"})),
        None
    );
}

#[test]
fn infer_tool_name_path_new_and_old_is_edit_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "new": "b", "old": "a"})),
        Some("edit_file")
    );
}

#[test]
fn infer_tool_name_path_new_and_after_is_edit_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "new": "b", "after": "anchor"})),
        Some("edit_file")
    );
}

#[test]
fn infer_tool_name_path_new_and_before_is_edit_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "new": "b", "before": "anchor"})),
        Some("edit_file")
    );
}

#[test]
fn infer_tool_name_path_new_and_lines_is_edit_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a.rs", "new": "b", "lines": "1-2"})),
        Some("edit_file")
    );
}

#[test]
fn infer_tool_name_pattern_present_is_search() {
    assert_eq!(
        infer_tool_name(&json!({"pattern": "foo.*bar"})),
        Some("search")
    );
}

#[test]
fn infer_tool_name_path_alone_is_none() {
    assert_eq!(infer_tool_name(&json!({"path": "a.rs"})), None);
}

#[test]
fn infer_tool_name_path_and_new_with_two_anchors_is_none() {
    assert_eq!(
        infer_tool_name(&json!({
            "path": "a.rs",
            "new": "b",
            "old": "a",
            "after": "anchor"
        })),
        None
    );
}

#[test]
fn infer_tool_name_path_and_new_with_zero_anchors_is_none() {
    assert_eq!(infer_tool_name(&json!({"path": "a.rs", "new": "b"})), None);
}

#[test]
fn infer_tool_name_cmd_alone_is_none() {
    // `shell`/`test` calls are never namable from `args` alone: `cmd`
    // doesn't distinguish the two tools.
    assert_eq!(infer_tool_name(&json!({"cmd": "cargo test"})), None);
}

#[test]
fn infer_tool_name_empty_object_is_none() {
    assert_eq!(infer_tool_name(&json!({})), None);
}

// ---------------------------------------------------------------------
// Unit 42: `infer_tool_name`'s new `read_file` arm -- `path` with at least
// one of `offset`/`limit`, and none of `content`/`new`/`pattern`. Bench
// `qwen_json_long_b24`: 72 of 106 unnamed calls had exactly this shape.
// ---------------------------------------------------------------------

#[test]
fn infer_tool_name_path_offset_and_limit_is_read_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a", "offset": 10, "limit": 50})),
        Some("read_file")
    );
}

#[test]
fn infer_tool_name_path_and_limit_only_is_read_file() {
    assert_eq!(
        infer_tool_name(&json!({"path": "a", "limit": 20})),
        Some("read_file")
    );
}

#[test]
fn infer_tool_name_path_only_is_none() {
    // A bare `{path}` (or `{}`) still returns `None` -- unit 42 only claims
    // the offset/limit shape actually seen in the bench.
    assert_eq!(infer_tool_name(&json!({"path": "a"})), None);
}

#[test]
fn infer_tool_name_path_offset_new_and_old_is_still_edit_file() {
    // The existing edit_file rule wins even with `offset` also present.
    assert_eq!(
        infer_tool_name(&json!({"path": "a", "offset": 1, "new": "x", "old": "y"})),
        Some("edit_file")
    );
}

#[test]
fn infer_tool_name_path_offset_and_content_is_still_write_file() {
    // The existing write_file rule wins even with `offset` also present.
    assert_eq!(
        infer_tool_name(&json!({"path": "a", "offset": 1, "content": "x"})),
        Some("write_file")
    );
}

// ---------------------------------------------------------------------
// Unit 32: `infer_empty_tool_name`'s wiring into `validate` -- exercised
// through `PlanSkill`/`ImplementSkill`/`ReviewSkill` directly (the
// `validate`-level substitution), complementing the end-to-end proof in
// `skills/tests/it/infer_tool.rs`.
// ---------------------------------------------------------------------

#[test]
fn implement_infers_edit_file_from_a_nameless_call_and_notes_it() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "",
            "args": {"path": "a.rs", "old": "a", "new": "b"},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "edit_file");
            assert_eq!(args, &json!({"path": "a.rs", "old": "a", "new": "b"}));
        }
        other => panic!("expected an edit_file tool call, got {other:?}"),
    }
    assert_eq!(
        step.notes(),
        &[format!("{INFERRED_TOOL_NOTE_PREFIX}`edit_file`.")]
    );
}

#[test]
fn implement_leaves_a_named_tool_call_alone() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "search",
            "args": {"pattern": "foo"},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, .. } => assert_eq!(name, "search"),
        other => panic!("expected a search tool call, got {other:?}"),
    }
    assert!(
        step.notes().iter().all(|n| !n.contains("had no name")),
        "a named call must not get the inferred-name note, got {:?}",
        step.notes()
    );
}

#[test]
fn implement_leaves_an_unguessable_empty_name_call_alone() {
    // No `infer_tool_name` arm matches an empty `args`; the name stays
    // empty and falls through to the ordinary empty-name rejection
    // (core/src/session.rs, item 29) at execution time -- `validate` itself
    // does not reject it.
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {"type": "tool", "name": "", "args": {}},
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, .. } => assert_eq!(name, ""),
        other => panic!("expected a tool call with an empty name, got {other:?}"),
    }
    assert!(
        step.notes().iter().all(|n| !n.contains("had no name")),
        "an unguessable call must not get the inferred-name note, got {:?}",
        step.notes()
    );
}

// ---------------------------------------------------------------------
// Unit 52: `infer_empty_tool_name`'s path-named branch, bare-path fallback
// -- a path-named call whose `args`, after the `path` insertion, has `path`
// as its only key infers `read_file`. Exercised the same way as the unit
// 32/42 tests above, through `validate` directly.
// ---------------------------------------------------------------------

#[test]
fn implement_infers_read_file_from_a_bare_path_named_call() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "src/error.rs",
            "args": {"path": "src/error.rs"},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "read_file");
            assert_eq!(args, &json!({"path": "src/error.rs"}));
        }
        other => panic!("expected a read_file tool call, got {other:?}"),
    }
    assert_eq!(
        step.notes(),
        &[format!(
            "{INFERRED_TOOL_NOTE_PREFIX}`read_file` (the name was `src/error.rs`)."
        )]
    );
}

#[test]
fn implement_infers_read_file_from_a_path_named_call_with_empty_args() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "src/error.rs",
            "args": {},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "read_file");
            assert_eq!(
                args,
                &json!({"path": "src/error.rs"}),
                "path must be inserted from the original name"
            );
        }
        other => panic!("expected a read_file tool call, got {other:?}"),
    }
    assert_eq!(
        step.notes(),
        &[format!(
            "{INFERRED_TOOL_NOTE_PREFIX}`read_file` (the name was `src/error.rs`)."
        )]
    );
}

#[test]
fn implement_leaves_a_path_named_call_with_an_extra_key_unguessed() {
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "src/error.rs",
            "args": {"path": "src/error.rs", "cmd": "ls"},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(
                name, "src/error.rs",
                "an extra key must leave the action untouched"
            );
            assert_eq!(args, &json!({"path": "src/error.rs", "cmd": "ls"}));
        }
        other => panic!("expected the untouched tool call, got {other:?}"),
    }
    assert!(
        step.notes().iter().all(|n| !n.contains("had no name")),
        "an unguessable path-named call must get no INFERRED note, got {:?}",
        step.notes()
    );
}

#[test]
fn implement_leaves_an_empty_name_call_with_a_bare_path_unguessed() {
    // Unit 52's fallback only applies to the path-named branch (case 2 of
    // `infer_empty_tool_name`); the empty-name branch (case 1) and
    // `infer_tool_name` itself are unchanged -- a bare `{"path"}` with an
    // empty `name` still infers nothing, since it could be a `write_file`
    // missing `content`.
    let skill = ImplementSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {},
        "action": {
            "type": "tool",
            "name": "",
            "args": {"path": "src/error.rs"},
        },
        "ack_inbox": [],
    });
    let step = parse_and_validate(&skill, &cur, content).expect("should not be rejected");
    match step.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "", "an empty name with a bare path must stay empty");
            assert_eq!(args, &json!({"path": "src/error.rs"}));
        }
        other => panic!("expected a tool call with an empty name, got {other:?}"),
    }
    assert!(
        step.notes().iter().all(|n| !n.contains("had no name")),
        "an unguessable call must not get the inferred-name note, got {:?}",
        step.notes()
    );
}

#[test]
fn infer_empty_tool_name_infers_read_file_for_a_bare_path_call_with_no_active_tools() {
    // `infer_empty_tool_name` never checks whether the INFERRED tool is
    // active for this skill (only whether `name` itself already names one
    // of `active`) -- the existing inactive-tool rejection (item 29,
    // `core/src/session.rs`) handles that separately, at execution time,
    // through `validate_default`, not here. `RouterSkill::tools()` is
    // empty (router's `validate` doesn't even wire this function in -- see
    // its impl), so an empty `active` list, the shape `router` would pass
    // if it did, stands in for it directly: called with `active: &[]`, the
    // bare-path fallback still fires the same way it does for `implement`.
    assert!(
        RouterSkill::new().tools().is_empty(),
        "router must have no tools for this case to be meaningful"
    );
    let cur = committed(CodingWorkspace::default());
    let mut out = ParsedOutput::<CodingWorkspace>::parse::<JsonMergePatch<CodingWorkspace>>(raw(
        0,
        json!({
            "patch": {},
            "action": {
                "type": "tool",
                "name": "src/error.rs",
                "args": {"path": "src/error.rs"},
            },
            "ack_inbox": [],
        }),
    ))
    .expect("should parse as valid StepOutput JSON");

    let note = super::infer_empty_tool_name(&[], &cur, &mut out);

    match out.action() {
        Action::Tool { name, args } => {
            assert_eq!(name, "read_file");
            assert_eq!(args, &json!({"path": "src/error.rs"}));
        }
        other => panic!("expected a read_file tool call, got {other:?}"),
    }
    assert_eq!(
        note,
        Some(format!(
            "{INFERRED_TOOL_NOTE_PREFIX}`read_file` (the name was `src/error.rs`)."
        ))
    );
}

// ---------------------------------------------------------------------
// Unit 51: `ask_user_permission_note`, case (b'') of the
// `ask_user_empty_note`/`ask_user_statement_note`/`ask_user_permission_note`
// chain -- a real question (contains `?`, so the first two siblings leave
// it alone) that only asks permission to do the work the task already
// asked for. Called directly through `super::ask_user_empty_note`, the
// chain's entry point every caller (`plan`/`review` directly, `implement`
// via `premature_done_note`'s fallthrough) uses -- isolating exactly the
// unit this brief specifies, the same way the `premature_done_note` tests
// above call that function directly.
// ---------------------------------------------------------------------

/// Builds an `AskUser` `ParsedOutput` with `next_skill` present in the raw
/// patch (so a real conversion's stripping is observable) plus an
/// unrelated patch field (so "patch otherwise kept" is observable too).
fn ask_user_output(message: &str) -> ParsedOutput<CodingWorkspace> {
    parsed_output(json!({
        "patch": {"recent_summary": "kept", "next_skill": "review"},
        "action": {"type": "ask_user", "message": message},
        "ack_inbox": []
    }))
}

/// Acceptance (a): each of the three live messages converts through
/// `ask_user_empty_note`'s chain to `PERMISSION_ASK_NOTE` exactly,
/// `next_skill` is stripped from the raw patch, and the rest of the patch
/// (`recent_summary`) is kept untouched.
#[test]
fn permission_ask_converts_would_you_like_me_to_proceed_with_this_change() {
    let mut out = ask_user_output("Would you like me to proceed with this change now?");
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(
        note,
        super::PERMISSION_ASK_NOTE,
        "note text must match exactly"
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        None,
        "next_skill must be stripped"
    );
    assert_eq!(
        out.patch().0.get("recent_summary"),
        Some(&json!("kept")),
        "the rest of the patch must be kept"
    );
}

#[test]
fn permission_ask_converts_when_the_sentence_holds_a_dotted_path() {
    // The `.` in `src/lib.rs` is not a sentence end: the question sentence
    // is the whole "Would you like me to edit src/lib.rs now?".
    let mut out = ask_user_output("Would you like me to edit src/lib.rs now?");
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(note, super::PERMISSION_ASK_NOTE);
    assert_eq!(out.patch().0.get("next_skill"), None);
}

#[test]
fn permission_ask_converts_do_you_want_me_to_in_mid_sentence() {
    // The live third message: the permission phrase sits after "or", so
    // it is the " want me to" marker, not a prefix, that catches it.
    let mut out = ask_user_output(
        "Should I create it now with the required functions, or do you want me to first \
         check what specific implementation details are needed?",
    );
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(note, super::PERMISSION_ASK_NOTE);
}

#[test]
fn permission_ask_converts_would_you_like_me_to_proceed_with_adding_arms() {
    let mut out = ask_user_output("Would you like me to proceed with adding these missing arms?");
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(
        note,
        super::PERMISSION_ASK_NOTE,
        "note text must match exactly"
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        None,
        "next_skill must be stripped"
    );
    assert_eq!(
        out.patch().0.get("recent_summary"),
        Some(&json!("kept")),
        "the rest of the patch must be kept"
    );
}

/// The third live message: no listed prefix matches at the sentence start
/// ("should i create it now..." is not `should i proceed`/`should i go
/// ahead"), so this one converts only via the `" proceed"` marker inside
/// the sentence -- exercising `PERMISSION_ASK_MARKERS` rather than
/// `PERMISSION_ASK_PREFIXES`.
#[test]
fn permission_ask_converts_should_i_create_it_now_via_proceed_marker() {
    let mut out = ask_user_output(
        "Should I create it now with the required functions, or do you want me to first \
         check the existing tests before I proceed?",
    );
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(
        note,
        super::PERMISSION_ASK_NOTE,
        "note text must match exactly"
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        None,
        "next_skill must be stripped"
    );
    assert_eq!(
        out.patch().0.get("recent_summary"),
        Some(&json!("kept")),
        "the rest of the patch must be kept"
    );
}

/// Acceptance (b): a genuine question the harness cannot answer itself --
/// matches neither a prefix nor a marker -- stays `AskUser`, untouched.
#[test]
fn permission_ask_leaves_a_real_config_question_alone() {
    let mut out = ask_user_output("Which of the two configs is the production one?");
    let note = super::ask_user_empty_note(&mut out);
    assert_eq!(note, None, "a real question must not be converted");
    assert!(
        matches!(
            out.action(),
            statie_core::Action::AskUser { message }
                if message == "Which of the two configs is the production one?"
        ),
        "action must stay AskUser, got {:?}",
        out.action()
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        Some(&json!("review")),
        "next_skill must be left in place when nothing converts"
    );
}

#[test]
fn permission_ask_leaves_a_real_default_choice_question_alone() {
    let mut out = ask_user_output("Should the default be 3 or 5?");
    let note = super::ask_user_empty_note(&mut out);
    assert_eq!(note, None, "a real question must not be converted");
    assert!(
        matches!(
            out.action(),
            statie_core::Action::AskUser { message } if message == "Should the default be 3 or 5?"
        ),
        "action must stay AskUser, got {:?}",
        out.action()
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        Some(&json!("review")),
        "next_skill must be left in place when nothing converts"
    );
}

/// Acceptance (c): a message with no `?` at all still gets
/// `ask_user_statement_note`'s note (case (b')), not
/// `PERMISSION_ASK_NOTE` -- the permission check never runs on it, since
/// `ask_user_statement_note` already converted the step before falling
/// through.
#[test]
fn permission_ask_note_does_not_fire_for_a_message_with_no_question_mark() {
    let mut out = ask_user_output("Would you like me to proceed with this change now");
    let note = super::ask_user_empty_note(&mut out).expect("a non-question must convert");
    assert_ne!(
        note,
        super::PERMISSION_ASK_NOTE,
        "a message with no `?` must get ask_user_statement_note's note, not this one"
    );
    assert!(
        note.contains("carried a statement of what you will do next"),
        "expected ask_user_statement_note's note text, got {note:?}"
    );
}

/// Acceptance (d): the prefix match is case-insensitive and tolerates
/// leading whitespace.
#[test]
fn permission_ask_prefix_match_is_case_insensitive_and_tolerates_leading_whitespace() {
    let mut out = ask_user_output("  Would You Like Me To add it?");
    let note =
        super::ask_user_empty_note(&mut out).expect("a permission ask must convert to Continue");
    assert_eq!(
        note,
        super::PERMISSION_ASK_NOTE,
        "note text must match exactly"
    );
    assert_eq!(
        out.patch().0.get("next_skill"),
        None,
        "next_skill must be stripped"
    );
}
