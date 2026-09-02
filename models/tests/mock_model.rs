use std::sync::Arc;
use std::time::Duration;

use statie_core::{
    AgentState, BoundedPrompt, Budgets, Capabilities, HarnessState, HarnessView,
    HeuristicTokenizer, Model, ModelError, NullSink, PromptBuilder, Schema, SkillSpec, StepOutput,
    StepSink, SystemPrompt, ToolSet, Usage, Version,
};
use statie_models::{MockModel, MockTurn};
use tokio_util::sync::CancellationToken;

fn budgets() -> Budgets {
    Budgets {
        total: 1_000_000,
        observation: 1_000_000,
        augmentation: 1_000_000,
    }
}

fn schema() -> Schema {
    Schema::of::<StepOutput<serde_json::Value>>()
}

fn build_prompt(version: Version) -> BoundedPrompt {
    let tok = HeuristicTokenizer;
    let caps = Capabilities {
        native_thinking: true,
        schema_constrained: true,
        prefix_cache: true,
    };
    let sys = SystemPrompt::for_harness(&caps, "json-merge-patch (RFC 7386)", "merge patch", None);
    let spec = Arc::new(SkillSpec::new("toy", "instructions"));
    let tools = ToolSet::new().list_for(&[]);
    let agent = serde_json::json!({"x": 1});
    let agent_state = AgentState::new(&agent);
    let harness = HarnessState {
        inbox: vec![],
        active_tools: vec![],
        budgets: budgets(),
        activation: "toy".to_string(),
        step: 0,
        last_overflow: None,
    };
    let harness_view = HarnessView::new(&harness);

    PromptBuilder::new(&tok, budgets(), version)
        .system(&sys)
        .spec(&spec)
        .tools(&tools)
        .agent(&agent_state)
        .harness(&harness_view)
        .build()
        .expect("prompt within budget")
}

struct CollectingSink {
    reasoning: String,
    content: String,
    content_deltas: usize,
}

impl CollectingSink {
    fn new() -> Self {
        CollectingSink {
            reasoning: String::new(),
            content: String::new(),
            content_deltas: 0,
        }
    }
}

impl StepSink for CollectingSink {
    fn reasoning_delta(&mut self, s: &str) {
        self.reasoning.push_str(s);
    }
    fn content_delta(&mut self, s: &str) {
        self.content.push_str(s);
        self.content_deltas += 1;
    }
}

#[tokio::test]
async fn scripted_turns_return_in_order_and_record_prompts() {
    let model = MockModel::new(vec![
        MockTurn::new("first content").with_reasoning("first reasoning"),
        MockTurn::new("second content").with_reasoning("second reasoning"),
    ]);
    let schema = schema();

    let p0 = build_prompt(Version(0));
    let out0 = model
        .step(p0, &schema, &mut NullSink, CancellationToken::new())
        .await
        .expect("first call succeeds");
    assert_eq!(out0.content, "first content");
    assert_eq!(
        out0.reasoning.expect("reasoning").as_ref(),
        "first reasoning"
    );

    let p1 = build_prompt(Version(1));
    let out1 = model
        .step(p1, &schema, &mut NullSink, CancellationToken::new())
        .await
        .expect("second call succeeds");
    assert_eq!(out1.content, "second content");
    assert_eq!(
        out1.reasoning.expect("reasoning").as_ref(),
        "second reasoning"
    );

    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].version, Version(0));
    assert_eq!(prompts[1].version, Version(1));
    assert_eq!(model.calls(), 2);

    // Third call: script exhausted, no repeat.
    let p2 = build_prompt(Version(2));
    let err = model
        .step(p2, &schema, &mut NullSink, CancellationToken::new())
        .await
        .expect_err("script exhausted");
    match err {
        ModelError::Backend(msg) => assert!(msg.contains("exhausted")),
        other => panic!("expected Backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn repeat_last_keeps_returning_final_turn() {
    let model =
        MockModel::new(vec![MockTurn::new("first"), MockTurn::new("second")]).repeat_last(true);
    let schema = schema();

    for v in 0..2 {
        model
            .step(
                build_prompt(Version(v)),
                &schema,
                &mut NullSink,
                CancellationToken::new(),
            )
            .await
            .expect("scripted call succeeds");
    }

    let out2 = model
        .step(
            build_prompt(Version(2)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("repeat_last returns the last turn instead of erroring");
    assert_eq!(out2.content, "second");
}

#[tokio::test]
async fn sink_sees_reassembled_deltas_in_at_least_two_content_chunks() {
    let model = MockModel::new(vec![
        MockTurn::new("hello world content").with_reasoning("some reasoning text"),
    ]);
    let schema = schema();
    let mut sink = CollectingSink::new();

    let out = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .expect("call succeeds");

    assert_eq!(sink.reasoning, "some reasoning text");
    assert_eq!(sink.content, "hello world content");
    assert_eq!(out.content, "hello world content");
    assert!(
        sink.content_deltas >= 2,
        "expected >=2 content deltas, got {}",
        sink.content_deltas
    );
}

#[tokio::test]
async fn with_usage_round_trips_to_raw_output() {
    let model = MockModel::new(vec![
        MockTurn::new("no usage"),
        MockTurn::new("has usage").with_usage(123, 45),
    ]);
    let schema = schema();

    let out0 = model
        .step(
            build_prompt(Version(0)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("first call succeeds");
    assert!(out0.usage.is_none(), "usage = {:?}", out0.usage);

    let out1 = model
        .step(
            build_prompt(Version(1)),
            &schema,
            &mut NullSink,
            CancellationToken::new(),
        )
        .await
        .expect("second call succeeds");
    assert_eq!(
        out1.usage,
        Some(Usage {
            prompt_tokens: 123,
            completion_tokens: 45,
        })
    );
}

#[tokio::test]
async fn cancellation_during_delay_returns_cancelled_promptly() {
    let model = MockModel::new(vec![
        MockTurn::new("late").with_delay(Duration::from_secs(5)),
    ]);
    let schema = schema();
    let cancel = CancellationToken::new();

    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = model
        .step(build_prompt(Version(0)), &schema, &mut NullSink, cancel)
        .await;
    let elapsed = start.elapsed();

    assert!(matches!(result, Err(ModelError::Cancelled)));
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
}
