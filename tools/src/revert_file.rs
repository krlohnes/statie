//! `revert_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{
    absent_marker_path, backup_content_path, cap_output, join_string_array_args, write_atomic,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevertFileArgs {
    /// Path to the file, relative to the sandbox root.
    #[serde(alias = "file", alias = "file_path")]
    pub path: String,
}

/// Restores a file to the content it had before this session's first
/// `write_file`/`edit_file` of it, using the pre-edit backup those tools
/// record under `.statie/orig/` (see `common::backup_before_edit`).
pub struct RevertFile {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for RevertFile {
    fn name(&self) -> &str {
        "revert_file"
    }

    fn description(&self) -> &str {
        "Restore a file to the content it had before this session's first edit_file/write_file of it. Use it when a file is corrupted past what edit_file can fix (duplicated blocks, ambiguous anchors, unbalanced braces) and start the change over from the original."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<RevertFileArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["path"]);
        let args: RevertFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let sandbox_path = ctx.root.join(&args.path)?;
        let path = sandbox_path.as_path();

        let backup_path = backup_content_path(&ctx.root, &args.path);
        let marker_path = absent_marker_path(&ctx.root, &args.path);

        if marker_path.exists() {
            if path.exists() {
                std::fs::remove_file(path)
                    .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            }
            let out = format!(
                "removed {}; it did not exist before this session",
                args.path
            );
            return Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)));
        }

        if !backup_path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "no edits recorded for {} in this session; nothing to revert",
                args.path
            )));
        }

        let bytes =
            std::fs::read(&backup_path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        }

        write_atomic(path, &bytes)?;

        let n = bytecount_lines(&bytes);
        let out = format!(
            "reverted {} to its content from before this session's first edit ({n} lines)",
            args.path
        );
        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Counts lines the way `str::lines()` would (one per `\n`, plus one more
/// for a final non-empty line with no trailing newline), but over raw bytes
/// so it works even if `bytes` isn't valid UTF-8. Mirrors
/// `write_file::bytecount_lines`.
fn bytecount_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}
