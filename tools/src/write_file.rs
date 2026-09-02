//! `write_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, write_atomic};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Path to the file, relative to the sandbox root. Parent directories
    /// are created if missing.
    pub path: String,
    /// The full contents to write.
    pub content: String,
}

/// Writes a text file into the sandbox, creating parent directories and
/// writing atomically (temp file + rename).
pub struct WriteFile {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a text file into the sandbox, creating parent directories as needed."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<WriteFileArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let sandbox_path = ctx.root.join(&args.path)?;
        let path = sandbox_path.as_path();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        }

        write_atomic(path, args.content.as_bytes())?;

        let bytes = args.content.len();
        let out = format!("wrote {bytes} bytes to {}", args.path);
        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}
