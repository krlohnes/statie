//! `think` tool: asks the reasoning subagent (`statie_core::Thinker`) a
//! question about the current situation. The tool itself never runs -- the
//! session intercepts `Action::Tool { name: "think", .. }` before dispatch
//! (`core/src/session.rs`) and calls the configured `Thinker` directly, so
//! `call` here always returns an error naming that.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThinkArgs {
    /// The question to ask the reasoning subagent.
    #[schemars(length(min = 1))]
    pub question: String,
}

/// Asks a reasoning subagent a question about the current situation. Run by
/// the session, not the tool registry: see this module's doc.
pub struct Think;

#[async_trait]
impl Tool for Think {
    fn name(&self) -> &str {
        "think"
    }

    fn description(&self) -> &str {
        "Ask a reasoning subagent a question about the current situation. It sees everything you see plus your question, thinks at length with no tools, and returns its conclusion, which you must then act on. Use it when your fix did not change the failure, or when you cannot tell which of several causes is real. Not for questions a read_file or test would answer."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ThinkArgs>()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        Err(ToolError::ExecutionFailed(
            "`think` is run by the session, not the tool registry".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_schema_requires_question() {
        let schema = Schema::of::<ThinkArgs>();
        let value = schema.as_value();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("schema should have required");
        assert_eq!(required, &vec![Value::String("question".to_string())]);

        let props = value
            .get("properties")
            .and_then(Value::as_object)
            .expect("schema should have properties");
        assert!(props.contains_key("question"));
    }

    #[tokio::test]
    async fn call_always_fails() {
        let tool = Think;
        let ctx = ToolCtx {
            root: statie_core::SandboxRoot::new(std::env::temp_dir()).unwrap(),
            log: std::sync::Arc::new(statie_core::MemoryLog::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = tool
            .call(serde_json::json!({"question": "why?"}), &ctx)
            .await
            .expect_err("call must always fail");
        match err {
            ToolError::ExecutionFailed(msg) => {
                assert_eq!(msg, "`think` is run by the session, not the tool registry");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
