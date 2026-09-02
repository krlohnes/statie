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
            None => detect_test_command(ctx.root.path()).ok_or_else(|| {
                ToolError::ExecutionFailed("no test command detected; pass cmd".to_string())
            })?,
        };

        let out = run_shell(
            &cmd,
            ctx.root.path(),
            self.default_timeout_secs,
            &ctx.cancel,
        )
        .await?;
        let full = format_result(&cmd, &out);

        Ok(ToolOutputFull::new(cap_output(full, self.max_output_bytes)))
    }
}

/// Builds the tool's final output: a structured verdict line, an optional
/// failure summary on non-zero exit, then the raw `run_shell` output.
fn format_result(cmd: &str, run_shell_output: &str) -> String {
    let code = parse_exit_code(run_shell_output);

    match code {
        Some(0) => {
            format!("command: {cmd}\nresult: pass\n{run_shell_output}")
        }
        Some(code) => {
            let summary = failure_summary(run_shell_output, 20);
            let summary_block = if summary.is_empty() {
                "--- failure summary ---\n(no failure lines recognized; see stderr below)\n"
                    .to_string()
            } else {
                format!("--- failure summary ---\n{}\n", summary.join("\n"))
            };
            format!("command: {cmd}\nresult: fail (exit {code})\n{summary_block}{run_shell_output}")
        }
        None => {
            // Couldn't parse an exit code; fall back to the raw output
            // without a verdict line rather than guessing.
            format!("command: {cmd}\n{run_shell_output}")
        }
    }
}

/// Parses the leading `exit: N` line out of `run_shell`'s output.
fn parse_exit_code(run_shell_output: &str) -> Option<i32> {
    let first_line = run_shell_output.lines().next()?;
    let rest = first_line.strip_prefix("exit: ")?;
    rest.trim().parse::<i32>().ok()
}

/// Scans the stdout+stderr portion of `run_shell`'s output (everything
/// after the leading `exit:` line) for lines that look like failure
/// signals, returning up to `max_lines` of them in order. Exact-duplicate
/// lines are collapsed, and each line is trimmed to 200 chars. If more
/// matching lines exist than `max_lines`, a final `…({n} more matching
/// lines)` line is appended.
pub fn failure_summary(output: &str, max_lines: usize) -> Vec<String> {
    let body = match output.find('\n') {
        Some(idx) => &output[idx + 1..],
        None => "",
    };

    let mut seen = std::collections::HashSet::new();
    let mut matched = Vec::new();

    for line in body.lines() {
        if is_failure_line(line) {
            let trimmed: String = line.chars().take(200).collect();
            if seen.insert(trimmed.clone()) {
                matched.push(trimmed);
            }
        }
    }

    if matched.len() <= max_lines {
        return matched;
    }

    let remaining = matched.len() - max_lines;
    let mut truncated: Vec<String> = matched.into_iter().take(max_lines).collect();
    truncated.push(format!("…({remaining} more matching lines)"));
    truncated
}

/// Heuristics for recognizing a line as a failure signal across common
/// test runners (Rust, Python/pytest, Go, generic assertion/error output).
fn is_failure_line(line: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "FAILED",
        "failed",
        "panicked at",
        "error[",
        "error:",
        "Error",
        "AssertionError",
        "assert",
        "FAIL",
        "✗",
    ];

    if NEEDLES.iter().any(|n| line.contains(n)) {
        return true;
    }

    let trimmed = line.trim_start();

    // Rust: `---- name stdout ----`
    if trimmed.starts_with("---- ") && trimmed.ends_with(" ----") {
        return true;
    }

    // Python/pytest: `E   assert ...` style assertion lines.
    if trimmed.starts_with("E   ") {
        return true;
    }

    // Go: `--- FAIL: TestName`
    if trimmed.starts_with("--- FAIL:") {
        return true;
    }

    false
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_summary_dedupes_exact_repeats() {
        let output =
            "exit: 1\n--- stdout ---\ntest foo ... FAILED\ntest foo ... FAILED\n--- stderr ---\n";
        let summary = failure_summary(output, 20);
        assert_eq!(summary, vec!["test foo ... FAILED".to_string()]);
    }

    #[test]
    fn failure_summary_trims_long_lines_to_200_chars() {
        let long_line = format!("error: {}", "x".repeat(300));
        let output = format!("exit: 1\n--- stdout ---\n{long_line}\n--- stderr ---\n");
        let summary = failure_summary(&output, 20);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].chars().count(), 200);
        assert_eq!(summary[0], long_line.chars().take(200).collect::<String>());
    }

    #[test]
    fn failure_summary_truncates_with_remaining_count_trailer() {
        let mut body = String::from("exit: 1\n--- stdout ---\n");
        for i in 0..25 {
            body.push_str(&format!("test case_{i} ... FAILED\n"));
        }
        body.push_str("--- stderr ---\n");

        let summary = failure_summary(&body, 20);
        assert_eq!(summary.len(), 21);
        assert_eq!(summary[20], "…(5 more matching lines)");
        assert_eq!(summary[0], "test case_0 ... FAILED");
        assert_eq!(summary[19], "test case_19 ... FAILED");
    }

    #[test]
    fn failure_summary_empty_when_nothing_matches() {
        let output = "exit: 1\n--- stdout ---\nall good here\n--- stderr ---\nnothing bad\n";
        let summary = failure_summary(output, 20);
        assert!(summary.is_empty());
    }

    #[test]
    fn parse_exit_code_reads_leading_line() {
        assert_eq!(parse_exit_code("exit: 0\n--- stdout ---\n"), Some(0));
        assert_eq!(parse_exit_code("exit: 137\n--- stdout ---\n"), Some(137));
        assert_eq!(parse_exit_code("not an exit line"), None);
    }
}
