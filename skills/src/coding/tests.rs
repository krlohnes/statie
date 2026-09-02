use std::sync::Arc;

use serde_json::json;

use statie_core::{Action, CommittedState, HarnessState, ParsedOutput, RawOutput, Skill, Version};
use statie_patch::JsonMergePatch;

use super::{
    CodingWorkspace, FileNote, ImplementSkill, PlanSkill, PlanStep, ReviewSkill, RouterSkill,
    StepStatus, coding_skills,
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
        },
        agent,
    }
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
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
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

#[test]
fn plan_handoff_to_implement_with_empty_plan_is_rejected() {
    let skill = PlanSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"next_skill": "implement"},
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

#[test]
fn implement_handoff_to_review_with_pending_step_is_rejected() {
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
    let err = parse_and_validate(&skill, &cur, content).unwrap_err();
    assert!(
        matches!(err, statie_core::PatchError::RuleViolation(_)),
        "got {err:?}"
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

#[test]
fn review_done_true_with_findings_is_rejected() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true, "findings": ["something is wrong"]},
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
fn review_done_true_with_empty_findings_is_accepted() {
    let skill = ReviewSkill::new();
    let cur = committed(CodingWorkspace::default());
    let content = json!({
        "patch": {"done": true},
        "action": {"type": "done"},
        "ack_inbox": []
    });
    parse_and_validate(&skill, &cur, content).expect("done with empty findings should be accepted");
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
