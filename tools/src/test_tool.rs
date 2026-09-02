//! `test` tool: runs (or detects) the project's test command.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, run_shell};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TestArgs {
    /// The test command to run. If omitted, it is auto-detected from
    /// project files in the sandbox root.
    pub cmd: Option<String>,
}

/// Runs the project's test suite: either the explicitly given `cmd`, or one
/// auto-detected from the sandbox root's project files.
pub struct TestTool {
    pub default_timeout_secs: u64,
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "test"
    }

    fn description(&self) -> &str {
        "Run the project's test suite (auto-detected, or an explicit command)."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<TestArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: TestArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let cmd = match args.cmd {
            Some(c) => c,
            None => detect_test_command(ctx.root.path())
                .ok_or_else(|| ToolError::ExecutionFailed("no test command detected; pass cmd".to_string()))?,
        };

        let out = run_shell(&cmd, ctx.root.path(), self.default_timeout_secs, &ctx.cancel).await?;
        let full = format!("command: {cmd}\n{out}");

        Ok(ToolOutputFull::new(cap_output(full, self.max_output_bytes)))
    }
}

/// Detects a project's test command by looking for well-known project
/// files in `root`. Returns `None` if nothing recognizable is found.
pub fn detect_test_command(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        return Some("cargo test".to_string());
    }
    if root.join("package.json").is_file() {
        return Some("npm test".to_string());
    }
    if root.join("pyproject.toml").is_file()
        || root.join("pytest.ini").is_file()
        || root.join("setup.py").is_file()
    {
        return Some("pytest".to_string());
    }
    if root.join("go.mod").is_file() {
        return Some("go test ./...".to_string());
    }
    if let Ok(makefile) = std::fs::read_to_string(root.join("Makefile"))
        && makefile.lines().any(|l| l.starts_with("test:"))
    {
        return Some("make test".to_string());
    }
    None
}
