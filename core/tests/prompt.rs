use std::sync::Arc;

use statie_core::{
    AgentState, Augmentation, Budgets, Capabilities, HarnessState, HarnessView,
    HeuristicTokenizer, InboxItem, OverBudget, PromptBuilder, Promptable, SkillSpec,
    SystemPrompt, ToolSet, Version,
};

fn budgets() -> Budgets {
    Budgets {
        total: 1_000_000,
        observation: 50,
        augmentation: 50,
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
    )
}

fn harness_state() -> HarnessState {
    HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: budgets(),
        activation: "toy".to_string(),
        step: 0,
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
    let agent_idx = user_text.find("## Agent State").expect("Agent State heading");
    let harness_idx = user_text.find("## Harness View").expect("Harness View heading");
    let aug_idx = user_text.find("## Augmentation").expect("Augmentation heading");
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
