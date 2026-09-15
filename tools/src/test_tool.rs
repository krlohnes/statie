//! `test` tool: runs (or detects) the project's test command.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, join_string_array_args, run_shell};

/// Exact text of the error returned when `cmd` is omitted and no test
/// command could be auto-detected from the sandbox root's project files.
/// Quoted (verbatim) in [`TestTool::description`], per D13, so the two
/// cannot drift.
pub const NO_TEST_COMMAND_DETECTED_MSG: &str = "no test command detected; pass cmd";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TestArgs {
    /// The test command to run. If omitted, it is auto-detected from
    /// project files in the sandbox root.
    #[serde(alias = "command")]
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
        static DESCRIPTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESCRIPTION.get_or_init(|| {
            format!(
                "Run the project's test suite. cmd (the shell command to run) is optional: if \
                 omitted, it is auto-detected from project files in the sandbox root (cargo \
                 test, npm test, pytest, go test ./..., or make test); if none of those files \
                 are found, the call fails with \"{NO_TEST_COMMAND_DETECTED_MSG}\". Output \
                 starts with the command and a pass/fail verdict line, followed by a failure \
                 summary (on failure) and the raw output, with passing-test noise stripped."
            )
        })
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<TestArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["cmd"]);
        let args: TestArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let cmd = match args.cmd {
            Some(c) => c,
            None => detect_test_command(ctx.root.path()).ok_or_else(|| {
                ToolError::ExecutionFailed(NO_TEST_COMMAND_DETECTED_MSG.to_string())
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
/// failure summary on non-zero exit, then the raw `run_shell` output (with
/// noisy passing-test lines stripped out).
fn format_result(cmd: &str, run_shell_output: &str) -> String {
    let code = parse_exit_code(run_shell_output);

    match code {
        Some(0) => {
            let note = filtered_out_note(run_shell_output)
                .map(|line| format!("{line}\n"))
                .unwrap_or_default();
            format!("command: {cmd}\nresult: pass\n{note}{run_shell_output}")
        }
        Some(code) => {
            let summary = failure_summary(run_shell_output, 20);
            let summary_block = if summary.is_empty() {
                "--- failure summary ---\n(no failure lines recognized; see stderr below)\n"
                    .to_string()
            } else {
                format!("--- failure summary ---\n{}\n", summary.join("\n"))
            };
            let trimmed_output = strip_passing_test_lines(run_shell_output);
            format!("command: {cmd}\nresult: fail (exit {code})\n{summary_block}{trimmed_output}")
        }
        None => {
            // Couldn't parse an exit code; fall back to the raw output
            // without a verdict line rather than guessing.
            format!("command: {cmd}\n{run_shell_output}")
        }
    }
}

/// Scans `run_shell_output` for the last cargo-style summary line ending in
/// `N filtered out` (or the shorter `N filtered`, e.g. from a test harness
/// that doesn't say "out"), and if `N > 0`, returns the note line to append
/// after `result: pass`. Only intended to be called on a passing run: a
/// failing filtered run already has work to do and doesn't need the nudge.
fn filtered_out_note(run_shell_output: &str) -> Option<String> {
    let mut found: Option<u64> = None;

    for line in run_shell_output.lines() {
        if let Some(n) = parse_filtered_count(line) {
            found = Some(n);
        }
    }

    let n = found?;
    if n == 0 {
        return None;
    }

    Some(format!(
        "note: {n} test(s) were filtered out by this command; run it without the filter \
         before approving."
    ))
}

/// Parses a `N filtered out` or `N filtered` count anywhere in a line (e.g.
/// cargo's `test result: ok. 1 passed; ...; 17 filtered out; finished in
/// 0.00s`, where the phrase is followed by more text), returning `None` if
/// the line doesn't contain that phrase preceded by a number.
fn parse_filtered_count(line: &str) -> Option<u64> {
    let idx = line
        .find("filtered out")
        .or_else(|| line.find("filtered"))?;
    let before = line[..idx].trim_end();
    let digits = before.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Returns true for lines that are pure noise once a run has failed: a
/// single passing test result, or a runner's "running N tests" banner.
fn is_droppable_noise_line(line: &str) -> bool {
    if line.starts_with("running ") {
        let rest = line.trim_start_matches("running ");
        if let Some(rest) = rest
            .strip_suffix(" tests")
            .or_else(|| rest.strip_suffix(" test"))
            && rest.chars().all(|c| c.is_ascii_digit())
            && !rest.is_empty()
        {
            return true;
        }
    }

    if let Some(rest) = line.strip_prefix("test ")
        && let Some(status) = rest.strip_suffix(" ... ok")
        && !status.is_empty()
    {
        return true;
    }

    false
}

/// Drops passing-test noise (`test ... ok` lines and `running N tests`
/// banners) from the raw `run_shell` output, replacing them with a single
/// count line so the model knows how much was omitted. Non-matching lines
/// (failures, compiler output, warnings) are kept verbatim and in order.
fn strip_passing_test_lines(run_shell_output: &str) -> String {
    let mut dropped = 0usize;
    let mut kept: Vec<&str> = Vec::new();

    for line in run_shell_output.lines() {
        if is_droppable_noise_line(line) {
            dropped += 1;
        } else {
            kept.push(line);
        }
    }

    let mut result = kept.join("\n");
    // `lines()` strips the trailing newline; restore it if the input had one.
    if run_shell_output.ends_with('\n') {
        result.push('\n');
    }

    if dropped > 0 {
        result.push_str(&format!("({dropped} passing test lines omitted)\n"));
    }

    result
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
///
/// In addition to single-line signals, two kinds of *context* are pulled
/// in so the model can see why something failed, not just that it did:
/// - After a `panicked at` line, the following lines (the assertion
///   message, `left:`/`right:` values, custom panic text) are included up
///   to the next blank line, a `----`/`test ` line, or a cap of 12 lines.
/// - After a rustc `error[...]`/`error:` line, the next 6 lines are
///   included so the location (`--> file:line:col`) and code snippet come
///   through.
pub fn failure_summary(output: &str, max_lines: usize) -> Vec<String> {
    let body = match output.find('\n') {
        Some(idx) => &output[idx + 1..],
        None => "",
    };

    let lines: Vec<&str> = body.lines().collect();
    let mut seen = std::collections::HashSet::new();
    let mut matched = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if is_failure_line(line) {
            push_unique(&mut matched, &mut seen, line);

            if line.contains("panicked at") {
                let mut j = i + 1;
                let mut taken = 0;
                while j < lines.len() && taken < 12 {
                    let next = lines[j];
                    let next_trimmed = next.trim_start();
                    if next.trim().is_empty()
                        || next_trimmed.starts_with("----")
                        || next_trimmed.starts_with("test ")
                    {
                        break;
                    }
                    push_unique(&mut matched, &mut seen, next);
                    taken += 1;
                    j += 1;
                }
            } else if is_rustc_error_line(line) {
                let end = (i + 1 + 6).min(lines.len());
                for next in &lines[i + 1..end] {
                    push_unique(&mut matched, &mut seen, next);
                }
            }
        }

        i += 1;
    }

    if matched.len() <= max_lines {
        return matched;
    }

    let remaining = matched.len() - max_lines;
    let mut truncated: Vec<String> = matched.into_iter().take(max_lines).collect();
    truncated.push(format!("…({remaining} more matching lines)"));
    truncated
}

/// Pushes `line` (trimmed to 200 chars) onto `matched` if not already seen.
fn push_unique(
    matched: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
    line: &str,
) {
    let trimmed: String = line.chars().take(200).collect();
    if seen.insert(trimmed.clone()) {
        matched.push(trimmed);
    }
}

/// True for rustc's `error[E....]: ...` and plain `error: ...` diagnostic
/// header lines (but not e.g. a stray "error:" inside test output prose —
/// this is intentionally the same substring check `is_failure_line` uses,
/// scoped to just the two rustc-shaped prefixes).
fn is_rustc_error_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error[") || trimmed.starts_with("error:")
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

    // Rust panic detail lines that can appear on their own (e.g. already
    // captured as panic-context, but also matched directly if scanned
    // outside that window): assertion text and left/right diff lines.
    if trimmed.starts_with("assertion")
        || trimmed.starts_with("left:")
        || trimmed.starts_with("right:")
    {
        return true;
    }

    // rustc's "run with RUST_BACKTRACE" hint, useful to know the backtrace
    // was suppressed.
    if trimmed.starts_with("note: run with") {
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
        // `FAILED` is a failure signal but (unlike `error:`/`panicked at`)
        // does not pull in trailing context lines, so this stays a clean
        // single-line trimming test.
        let long_line = format!("test x ... FAILED {}", "x".repeat(300));
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

    #[test]
    fn filtered_out_note_parses_cargo_summary_line() {
        let output = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 17 filtered \
                       out; finished in 0.00s\n";
        assert_eq!(
            filtered_out_note(output),
            Some(
                "note: 17 test(s) were filtered out by this command; run it without the \
                 filter before approving."
                    .to_string()
            )
        );
    }

    #[test]
    fn filtered_out_note_none_when_zero_filtered() {
        let output = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered \
                       out; finished in 0.00s\n";
        assert_eq!(filtered_out_note(output), None);
    }

    #[test]
    fn filtered_out_note_none_without_the_phrase() {
        let output = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; finished in \
                       0.00s\n";
        assert_eq!(filtered_out_note(output), None);
    }

    #[test]
    fn filtered_out_note_accepts_shorter_filtered_form() {
        let output = "12 tests, 5 filtered\n";
        assert_eq!(
            filtered_out_note(output),
            Some(
                "note: 5 test(s) were filtered out by this command; run it without the \
                 filter before approving."
                    .to_string()
            )
        );
    }
}
