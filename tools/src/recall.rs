//! `recall` tool: searches the session log. The A1 prompt-gate invariant is
//! enforced here at the boundary — `Reasoning` entries can never re-enter a
//! prompt via this tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{LogKind, Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::cap_output;

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
const PAYLOAD_TRUNCATE_CHARS: usize = 600;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// Search query (substring/keyword match) against the session log.
    pub query: String,
    /// Restrict results to one log entry kind: user, patch, action,
    /// tool_output, or system. `reasoning` is not recallable.
    pub kind: Option<String>,
    /// Maximum number of results. Defaults to 5, capped at 20.
    pub limit: Option<usize>,
}

/// Searches the session log for entries matching a query, optionally
/// filtered by kind. `reasoning` entries are never returned: reasoning must
/// never re-enter a prompt (A1).
pub struct Recall {
    pub max_output_bytes: usize,
}

fn parse_kind(kind: &str) -> Result<LogKind, ToolError> {
    match kind {
        "user" => Ok(LogKind::User),
        "patch" => Ok(LogKind::Patch),
        "action" => Ok(LogKind::Action),
        "tool_output" => Ok(LogKind::ToolOutput),
        "system" => Ok(LogKind::System),
        "reasoning" => Err(ToolError::InvalidArgs(
            "reasoning entries are not recallable".to_string(),
        )),
        other => Err(ToolError::InvalidArgs(format!("unknown kind: {other}"))),
    }
}

#[async_trait]
impl Tool for Recall {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search the session log for prior entries (user messages, patches, actions, tool output, system notes)."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<RecallArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: RecallArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

        let kind = match &args.kind {
            Some(k) => Some(parse_kind(k)?),
            None => None,
        };

        let mut hits = ctx.log.search(&args.query, kind, limit);
        // Belt-and-suspenders: even when no kind filter was given, drop any
        // Reasoning entries a backend's search might otherwise surface.
        hits.retain(|e| e.kind != LogKind::Reasoning);

        if hits.is_empty() {
            return Ok(ToolOutputFull::new("no matching entries".to_string()));
        }

        let blocks: Vec<String> = hits
            .iter()
            .map(|e| {
                let kind_str = serde_json::to_value(e.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown".to_string());
                let payload_str = serde_json::to_string(&e.payload).unwrap_or_default();
                let payload = truncate_chars(&payload_str, PAYLOAD_TRUNCATE_CHARS);
                format!("[step {}] {kind_str} {}\n{payload}", e.step, e.ts)
            })
            .collect();

        let out = blocks.join("\n\n");
        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}…")
}
