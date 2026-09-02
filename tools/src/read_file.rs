//! `read_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::cap_output;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file, relative to the sandbox root.
    pub path: String,
    /// 1-based line number to start reading from. Defaults to 1.
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Defaults to 200.
    pub limit: Option<usize>,
}

/// Reads a text file (or a window of it) from the sandbox, prefixing each
/// line with its line number.
pub struct ReadFile {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the sandbox, optionally windowed by offset/limit lines."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ReadFileArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(200);

        let sandbox_path = ctx.root.join(&args.path)?;
        let path = sandbox_path.as_path();

        if !path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "no such file: {}",
                args.path
            )));
        }
        if !path.is_file() {
            return Err(ToolError::ExecutionFailed(format!(
                "not a regular file: {}",
                args.path
            )));
        }

        let bytes = std::fs::read(path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| ToolError::ExecutionFailed(format!("not valid utf-8: {}", args.path)))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let from = offset.min(total.max(1));
        let start_idx = from.saturating_sub(1);
        let end_idx = (start_idx + limit).min(total);
        let to = end_idx;

        let mut out = format!("{} ({total} lines, showing {from}-{to})\n", args.path);
        for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
            let n = start_idx + i + 1;
            out.push_str(&format!("{n:>5} | {line}\n"));
        }

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}
