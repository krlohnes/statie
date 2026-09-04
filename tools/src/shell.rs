//! `shell` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, join_string_array_args, run_shell};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The shell command to run (executed via `sh -c` / `cmd /C`).
    #[serde(alias = "command")]
    pub cmd: String,
    /// Timeout in seconds. Defaults to the tool's configured shell timeout.
    pub timeout_secs: Option<u64>,
}

/// Runs a shell command inside the sandbox root, with cancellation and a
/// timeout. A non-zero exit code is reported in the output, not as an
/// error.
pub struct Shell {
    pub default_timeout_secs: u64,
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command inside the sandbox root. Non-zero exit codes are reported, not errors."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<ShellArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["cmd"]);
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let timeout_secs = args.timeout_secs.unwrap_or(self.default_timeout_secs);
        let out = run_shell(&args.cmd, ctx.root.path(), timeout_secs, &ctx.cancel).await?;

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}
