//! `try_fixes` tool: proposes k candidate fixes for the current failing
//! test, verifies each in an isolated worktree, and applies the passing
//! one (`statie_core::Fixer`, BACKLOG.md item 4). Like `think`
//! (`tools/src/think.rs`), the tool itself never runs -- the session
//! intercepts `Action::Tool { name: "try_fixes", .. }` before dispatch and
//! calls the configured `Fixer` (plus the worktree verification primitive
//! in `tools::worktree`) directly, so `call` here always returns an error
//! naming that. Session-side wiring is a later unit; this crate only
//! registers the tool so it renders in the tool list with its schema.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TryFixesArgs {
    /// How many candidate fixes to sample. Defaults to the configured
    /// `fixer.k` when omitted.
    pub k: Option<u32>,
}

/// Proposes k candidate fixes for the current failing test, verifies each
/// in an isolated worktree, and applies the passing one. Run by the
/// session, not the tool registry: see this module's doc.
pub struct TryFixes;

#[async_trait]
impl Tool for TryFixes {
    fn name(&self) -> &str {
        "try_fixes"
    }

    fn description(&self) -> &str {
        "Propose k candidate fixes for the current failing test, verify each in an isolated worktree, apply the passing one. Use when the same test failure has repeated."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<TryFixesArgs>()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        Err(ToolError::ExecutionFailed(
            "`try_fixes` is run by the session, not the tool registry".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_schema_has_optional_k() {
        let schema = Schema::of::<TryFixesArgs>();
        let value = schema.as_value();
        let props = value
            .get("properties")
            .and_then(Value::as_object)
            .expect("schema should have properties");
        assert!(props.contains_key("k"));

        // `k` is optional: it must not be in `required` (if `required` is
        // present at all).
        if let Some(required) = value.get("required").and_then(Value::as_array) {
            assert!(
                !required.contains(&Value::String("k".to_string())),
                "k must be optional"
            );
        }
    }

    #[tokio::test]
    async fn call_always_fails() {
        let tool = TryFixes;
        let ctx = ToolCtx {
            root: statie_core::SandboxRoot::new(std::env::temp_dir()).unwrap(),
            log: std::sync::Arc::new(statie_core::MemoryLog::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = tool
            .call(serde_json::json!({"k": 3}), &ctx)
            .await
            .expect_err("call must always fail");
        match err {
            ToolError::ExecutionFailed(msg) => {
                assert_eq!(
                    msg,
                    "`try_fixes` is run by the session, not the tool registry"
                );
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_always_fails_with_no_args() {
        let tool = TryFixes;
        let ctx = ToolCtx {
            root: statie_core::SandboxRoot::new(std::env::temp_dir()).unwrap(),
            log: std::sync::Arc::new(statie_core::MemoryLog::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = tool
            .call(serde_json::json!({}), &ctx)
            .await
            .expect_err("call must always fail");
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }
}
