//! `list_files` tool.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, join_string_array_args};

pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".statie",
    "__pycache__",
    ".venv",
    "venv",
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesArgs {
    /// Directory to list, relative to the sandbox root. Defaults to ".".
    #[serde(alias = "file", alias = "file_path")]
    pub path: Option<String>,
    /// Maximum number of entries to return. Defaults to 200.
    pub max: Option<usize>,
}

/// Recursively lists files and directories under a sandbox path, skipping
/// common noise directories (`.git`, `target`, `node_modules`, `.statie`,
/// `__pycache__`, `.venv`, `venv`).
pub struct ListFiles {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for ListFiles {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "Recursively list files and directories under a sandbox path."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ListFilesArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["path"]);
        let args: ListFilesArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let rel = args.path.unwrap_or_else(|| ".".to_string());
        let max = args.max.unwrap_or(200);

        let sandbox_path = ctx.root.join(&rel)?;
        let base = sandbox_path.as_path().to_path_buf();

        if !base.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "no such directory: {rel}"
            )));
        }
        if !base.is_dir() {
            return Err(ToolError::ExecutionFailed(format!(
                "not a directory: {rel}"
            )));
        }

        let mut entries: Vec<(PathBuf, bool)> = Vec::new();
        walk(&base, &base, &mut entries)?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let total = entries.len();
        let shown = entries.into_iter().take(max);

        let mut lines: Vec<String> = Vec::new();
        let mut count = 0usize;
        for (path, is_dir) in shown {
            let rel_str = path.to_string_lossy().replace('\\', "/");
            if is_dir {
                lines.push(format!("{rel_str}/"));
            } else {
                lines.push(rel_str);
            }
            count += 1;
        }

        if count < total {
            lines.push(format!("… {} more", total - count));
        }

        let out = if lines.is_empty() {
            "(empty directory)".to_string()
        } else {
            lines.join("\n")
        };
        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Recursively walk `dir`, collecting `(path relative to base, is_dir)` for
/// every entry, skipping noise directories.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, bool)>) -> Result<(), ToolError> {
    let read = std::fs::read_dir(dir).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    for entry in read {
        let entry = entry.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
                .to_path_buf();
            out.push((rel, true));
            walk(base, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
                .to_path_buf();
            out.push((rel, false));
        }
    }
    Ok(())
}
