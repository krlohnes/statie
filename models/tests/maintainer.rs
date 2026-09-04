//! Integration tests for `ModelMaintainer` (statie-spec.md "9. Context
//! maintenance"): drives a `Model` backend via `MockModel`, asserting the
//! happy path, `next: null`, fenced JSON, parse failure, the over-budget
//! retry, over-budget-twice, and a timeout against a custom slow `Model`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use statie_core::{
    Action, BoundedPrompt, Capabilities, Commitment, HeuristicTokenizer, MaintainError,
    MaintainInput, Maintainer, Model, ModelError, RawOutput, Schema, StepSink, TokenBudget,
};
use statie_models::{MockModel, MockTurn, ModelMaintainer};
use tokio_util::sync::CancellationToken;

fn tool_action() -> Action {
    Action::Tool {
        name: "edit_file".to_string(),
        args: serde_json::json!({"path": "a.rs"}),
    }
}

#[allow(clippy::too_many_arguments)]
fn base_input<'a>(
    reasoning: Option<&'a str>,
    patch: &'a serde_json::Value,
    action: &'a Action,
    observation: &'a str,
    agent: &'a serde_json::Value,
    commitment: Option<&'a Commitment>,
    notes: &'a str,
    budget: usize,
) -> MaintainInput<'a> {
    base_input_with_tools(
        reasoning,
        patch,
        action,
        observation,
        agent,
        commitment,
        notes,
        budget,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
fn base_input_with_tools<'a>(
    reasoning: Option<&'a str>,
    patch: &'a serde_json::Value,
    action: &'a Action,
    observation: &'a str,
    agent: &'a serde_json::Value,
    commitment: Option<&'a Commitment>,
    notes: &'a str,
    budget: usize,
    active_tools: &'a [String],
) -> MaintainInput<'a> {
    MaintainInput {
        reasoning,
        patch,
        action,
        observation,
        agent,
        commitment,
        notes,
        budget: TokenBudget(budget),
        active_tools,
    }
}

#[tokio::test]
async fn happy_path_parses_notes_and_next_with_one_model_call() {
    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        r#"{"notes": "kept the anchor text", "next": {"tool": "edit_file", "path": "a.rs", "what": "delete lines 10-12"}}"#,
    )]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model.clone(), tok, 4000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({"state": "editing"});
    let active_tools = vec!["read_file".to_string(), "edit_file".to_string()];
    let input = base_input_with_tools(
        Some("my reasoning about the edit"),
        &patch,
        &action,
        "the tool output",
        &agent,
        None,
        "previous notes",
        400,
        &active_tools,
    );

    let out = maintainer.maintain(input).await.expect("should succeed");
    assert_eq!(out.notes, "kept the anchor text");
    let next = out.next.expect("expected a next commitment");
    assert_eq!(next.tool, "edit_file");
    assert_eq!(next.path.as_deref(), Some("a.rs"));
    assert_eq!(next.what, "delete lines 10-12");

    assert_eq!(model.calls(), 1);
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].user.contains("previous notes:"));
    assert!(prompts[0].user.contains("my reasoning about the edit"));
    assert!(
        prompts[0]
            .user
            .contains("active tools: read_file, edit_file")
    );
}

#[tokio::test]
async fn next_null_parses_as_none() {
    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        r#"{"notes": "nothing decided yet", "next": null}"#,
    )]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model, tok, 4000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 400);

    let out = maintainer.maintain(input).await.expect("should succeed");
    assert_eq!(out.notes, "nothing decided yet");
    assert!(out.next.is_none());
}

#[tokio::test]
async fn fenced_json_is_accepted() {
    let model = Arc::new(MockModel::new(vec![MockTurn::new(
        "```json\n{\"notes\": \"fenced\", \"next\": null}\n```",
    )]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model, tok, 4000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 400);

    let out = maintainer.maintain(input).await.expect("should succeed");
    assert_eq!(out.notes, "fenced");
    assert!(out.next.is_none());
}

#[tokio::test]
async fn garbage_output_returns_parse_error() {
    let model = Arc::new(MockModel::new(vec![MockTurn::new("not json at all")]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model, tok, 4000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 400);

    let err = maintainer.maintain(input).await.expect_err("should fail");
    match err {
        MaintainError::Parse(msg) => assert!(msg.contains("not json at all")),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[tokio::test]
async fn over_budget_then_fits_retries_once_with_retry_note() {
    // HeuristicTokenizer counts ceil(chars/4); budget of 5 tokens means
    // anything with `notes` text over ~20 chars is over budget.
    let long_notes = "x".repeat(200);
    let short_notes = "short";
    let model = Arc::new(MockModel::new(vec![
        MockTurn::new(format!(r#"{{"notes": "{long_notes}", "next": null}}"#)),
        MockTurn::new(format!(r#"{{"notes": "{short_notes}", "next": null}}"#)),
    ]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model.clone(), tok, 100_000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 5);

    let out = maintainer.maintain(input).await.expect("should succeed");
    assert_eq!(out.notes, short_notes);

    assert_eq!(model.calls(), 2);
    let prompts = model.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(!prompts[0].user.contains("the cap is"));
    assert!(prompts[1].user.contains("the cap is 5"));
    assert!(prompts[1].user.contains("Cut from the end"));
}

#[tokio::test]
async fn over_budget_twice_returns_over_budget_error() {
    let long_notes = "x".repeat(200);
    let model = Arc::new(MockModel::new(vec![
        MockTurn::new(format!(r#"{{"notes": "{long_notes}", "next": null}}"#)),
        MockTurn::new(format!(r#"{{"notes": "{long_notes}", "next": null}}"#)),
    ]));
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model.clone(), tok, 100_000, Duration::from_secs(5));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 5);

    let err = maintainer.maintain(input).await.expect_err("should fail");
    match err {
        MaintainError::OverBudget { tokens, budget } => {
            assert!(tokens > budget);
            assert_eq!(budget, 5);
        }
        other => panic!("expected OverBudget, got {other:?}"),
    }
    assert_eq!(model.calls(), 2);
}

/// A `Model` whose `step` sleeps longer than the timeout it's given, to
/// exercise `ModelMaintainer`'s `tokio::time::timeout` wrapping.
struct SlowModel {
    delay: Duration,
}

#[async_trait]
impl Model for SlowModel {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_thinking: true,
            schema_constrained: true,
            prefix_cache: false,
        }
    }

    async fn step(
        &self,
        prompt: BoundedPrompt,
        _schema: &Schema,
        _sink: &mut dyn StepSink,
        _cancel: CancellationToken,
    ) -> Result<RawOutput, ModelError> {
        tokio::time::sleep(self.delay).await;
        Ok(RawOutput::new(
            prompt.version(),
            None,
            r#"{"notes": "too slow", "next": null}"#.to_string(),
        ))
    }
}

#[tokio::test]
async fn slow_backend_times_out() {
    let model = Arc::new(SlowModel {
        delay: Duration::from_millis(300),
    });
    let tok = Arc::new(HeuristicTokenizer);
    let maintainer = ModelMaintainer::new(model, tok, 4000, Duration::from_millis(50));

    let patch = serde_json::json!({});
    let action = tool_action();
    let agent = serde_json::json!({});
    let input = base_input(None, &patch, &action, "obs", &agent, None, "notes", 400);

    let err = maintainer.maintain(input).await.expect_err("should fail");
    assert!(matches!(err, MaintainError::Timeout), "got {err:?}");
}
