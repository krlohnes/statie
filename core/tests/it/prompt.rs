use std::sync::Arc;

use statie_core::{
    AgentState, Augmentation, Budgets, Capabilities, Commitment, HarnessState, HarnessView,
    HeuristicTokenizer, InboxItem, LogEntry, LogKind, MemoryLog, OverBudget, Pinned, PromptBuilder,
    Promptable, Schema, SessionLog, SkillSpec, StepOutput, SystemPrompt, ToolSet, Version,
    WorkingSet, WorkingSetConfig, restrict_action_types,
};

fn budgets() -> Budgets {
    Budgets {
        total: 1_000_000,
        observation: 50,
        augmentation: 50,
        notes: 400,
        working_set: 1_000_000,
    }
}

fn system_prompt() -> SystemPrompt {
    SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        None,
        &[],
    )
}

fn harness_state() -> HarnessState {
    HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: budgets(),
        activation: "toy".to_string(),
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
    }
}

// The Observation-over-budget scenario is exercised as an in-crate unit
// test in `core/src/prompt.rs` because `Observation`'s constructors are
// `pub(crate)` and cannot be reached from an integration test.

#[test]
fn total_over_budget_reports_a_section() {
    let tok = HeuristicTokenizer;
    let b = Budgets {
        total: 5,
        observation: 1_000_000,
        augmentation: 1_000_000,
        notes: 1_000_000,
        working_set: 1_000_000,
    };
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);

    let result = PromptBuilder::new(&tok, b, Version(0))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build();

    let err: OverBudget = result.expect_err("expected OverBudget");
    assert!(err.tokens > err.limit);
    assert_eq!(err.limit, 5);
}

#[test]
fn within_budget_prompt_reports_tokens_and_version() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);

    let prompt = PromptBuilder::new(&tok, b, Version(7))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build()
        .expect("should be within budget");

    assert!(prompt.tokens() > 0);
    assert_eq!(prompt.version(), Version(7));
}

#[test]
fn sections_render_in_fixed_order_regardless_of_call_order() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);
    let augmentation = Augmentation::default();

    // Call builder methods in reverse order.
    let prompt = PromptBuilder::new(&tok, b, Version(0))
        .augmentation(&augmentation)
        .harness(&harness_view)
        .agent(&agent_state)
        .tools(&tools)
        .spec(&spec)
        .system(&sys)
        .build()
        .expect("within budget");

    let system_text = prompt.system();
    let sys_idx = system_text.find("## System").expect("System heading");
    let spec_idx = system_text.find("## Spec").expect("Spec heading");
    let tools_idx = system_text.find("## Tools").expect("Tools heading");
    assert!(sys_idx < spec_idx);
    assert!(spec_idx < tools_idx);

    let user_text = prompt.user();
    let agent_idx = user_text
        .find("## Agent State")
        .expect("Agent State heading");
    let harness_idx = user_text
        .find("## Harness View")
        .expect("Harness View heading");
    let aug_idx = user_text
        .find("## Augmentation")
        .expect("Augmentation heading");
    assert!(agent_idx < harness_idx);
    assert!(harness_idx < aug_idx);
}

#[test]
fn harness_view_renders_only_unacked_inbox_items() {
    let mut harness = harness_state();
    harness.inbox = vec![
        InboxItem {
            id: 1,
            text: "acked message".to_string(),
            acked: true,
        },
        InboxItem {
            id: 2,
            text: "unacked message".to_string(),
            acked: false,
        },
    ];
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(rendered.contains("unacked message"));
    assert!(!rendered.contains("[1] acked message"));
    // The rendered inbox listing must not carry an entry for id 1 at all.
    assert!(!rendered.contains("[1]"));
}

#[test]
fn harness_view_renders_last_observation_when_set() {
    let mut harness = harness_state();
    harness.last_overflow = Some("step 3: `echo` output overflowed".to_string());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        rendered.contains("last_observation: step 3: `echo` output overflowed"),
        "expected a last_observation line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_last_observation_when_none() {
    let harness = harness_state();
    assert!(harness.last_overflow.is_none());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        !rendered.contains("last_observation:"),
        "expected no last_observation line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_renders_pinned_when_set() {
    let mut harness = harness_state();
    harness.step = 4;
    harness.pinned = Some(Pinned {
        name: "ui-review".to_string(),
        expires_at_step: 10,
    });
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        rendered.contains("pinned: ui-review (6 steps left)"),
        "expected a pinned line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_pinned_when_none() {
    let harness = harness_state();
    assert!(harness.pinned.is_none());
    let view = HarnessView::new(&harness);
    let rendered = view.render();
    assert!(
        !rendered.contains("pinned:"),
        "expected no pinned line, got:\n{rendered}"
    );
}

#[test]
fn harness_view_renders_commitment_with_reads_left() {
    let harness = harness_state();
    let commitment = Commitment {
        tool: "edit_file".to_string(),
        path: Some("a.rs".to_string()),
        what: "replace `old` with `new`".to_string(),
    };
    let view = HarnessView::new(&harness).with_commitment(Some((commitment, 2)));
    let rendered = view.render();
    assert!(
        rendered.contains(
            "next: edit_file a.rs — replace `old` with `new` (2 reads left before you must act)"
        ),
        "expected a next: line with reads-left, got:\n{rendered}"
    );
}

#[test]
fn harness_view_omits_next_line_when_no_commitment() {
    let harness = harness_state();
    let view = HarnessView::new(&harness).with_commitment(None);
    let rendered = view.render();
    assert!(
        !rendered.contains("next:"),
        "expected no next: line, got:\n{rendered}"
    );
}

#[test]
fn for_harness_omits_delegate_text_when_no_delegates() {
    let sys = system_prompt();
    let rendered = sys.render();
    assert!(!rendered.contains("\"type\": \"delegate\""));
    assert!(!rendered.contains("delegate`"));
}

/// `SystemPrompt::for_harness` given a schema whose `action.type` enum is
/// `["tool", "done"]` (the shape `restrict_action_types` produces for a
/// skill with `allows_ask_user() == false` and `allows_yield() == false`,
/// e.g. `RouterSkill`, item 28): the "Exact shapes" block must contain no
/// `ask_user` example line, and must still contain the `tool` and `done`
/// example lines.
#[test]
fn for_harness_with_tool_and_done_only_schema_omits_ask_user_example() {
    let mut schema = Schema::of::<StepOutput<serde_json::Value>>();
    restrict_action_types(&mut schema, &["tool", "done"]);

    let sys = SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        Some(&schema),
        &[],
    );
    let rendered = sys.render();

    assert!(
        !rendered.contains(r#""type": "ask_user""#),
        "expected no ask_user example shape, got:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "{\"patch\": {...}, \"action\": {\"type\": \"tool\", \"name\": \"list_files\", \"args\": {\"path\": \".\"}, \"message\": \"\"}, \"ack_inbox\": [1]}"
        ),
        "expected the tool example shape, got:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "{\"patch\": {...}, \"action\": {\"type\": \"done\", \"name\": \"\", \"args\": {}, \"message\": \"\"}, \"ack_inbox\": []}"
        ),
        "expected the done example shape, got:\n{rendered}"
    );
}

#[test]
fn for_harness_includes_delegate_text_when_delegates_given() {
    let sys = SystemPrompt::for_harness(
        &Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: true,
        },
        "json-merge-patch (RFC 7386)",
        "merge patch semantics",
        None,
        &["ui-review".to_string()],
    );
    let rendered = sys.render();
    assert!(rendered.contains("\"type\": \"delegate\""));
    assert!(rendered.contains("ui-review"));
}

#[test]
fn working_set_section_renders_between_harness_view_and_augmentation() {
    let tok = HeuristicTokenizer;
    let b = budgets();
    let sys = system_prompt();
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = harness_state();
    let harness_view = HarnessView::new(&harness);
    let augmentation = Augmentation::default();

    let log = MemoryLog::new();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::Action,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": "a.rs"}}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 1,
        kind: LogKind::ToolOutput,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"name": "read_file", "output": "1: fn a() {}", "truncated": false}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 2,
        kind: LogKind::Action,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"type": "tool", "name": "search", "args": {"pattern": "fn"}}),
        agent: None,
    })
    .unwrap();
    log.append(LogEntry {
        step: 2,
        kind: LogKind::ToolOutput,
        ts: "2026-01-01T00:00:00Z".to_string(),
        payload: serde_json::json!({"name": "search", "output": "a.rs:1: fn a() {}", "truncated": false}),
        agent: None,
    })
    .unwrap();

    let cfg = WorkingSetConfig::default();
    let ws =
        WorkingSet::from_log(&log, &tok, b.working_set, &cfg, 3).expect("expected a working set");

    let prompt = PromptBuilder::new(&tok, b, Version(0))
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .working_set(Some(&ws))
        .augmentation(&augmentation)
        .build()
        .expect("within budget");

    let user_text = prompt.user();
    let harness_idx = user_text
        .find("## Harness View")
        .expect("Harness View heading");
    let ws_idx = user_text
        .find("## working set")
        .expect("working set heading");
    let aug_idx = user_text
        .find("## Augmentation")
        .expect("Augmentation heading");
    assert!(harness_idx < ws_idx);
    assert!(ws_idx < aug_idx);
    assert!(
        user_text
            .contains("working set (what you read recently; derived from the log, not state):")
    );
}
