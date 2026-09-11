use super::support;

use serde_json::json;
use statie_core::Tool;
use statie_tools::{TestTool, detect_test_command};

fn tool() -> TestTool {
    TestTool {
        default_timeout_secs: 120,
        max_output_bytes: 200_000,
    }
}

#[test]
fn detects_cargo_test() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    assert_eq!(
        detect_test_command(dir.path()),
        Some("cargo test".to_string())
    );
}

#[test]
fn detects_nothing_for_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(detect_test_command(dir.path()), None);
}

#[tokio::test]
async fn explicit_cmd_runs_and_prefixes_output() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"cmd": "echo ok"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.starts_with("command: echo ok\n"), "got: {text}");
    assert!(text.contains("ok"), "got: {text}");
}

#[tokio::test]
async fn no_cmd_and_no_detection_is_execution_failed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool().call(json!({}), &ctx).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no test command detected"), "got: {msg}");
}

#[tokio::test]
async fn passing_command_reports_pass_verdict_and_no_failure_summary() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"cmd": "echo ok"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: pass"), "got: {text}");
    assert!(!text.contains("failure summary"), "got: {text}");
}

#[tokio::test]
async fn failing_command_reports_fail_verdict_and_failure_summary() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let cmd =
        "sh -c \"echo 'test foo ... FAILED'; echo \\\"thread 'main' panicked at\\\" 1>&2; exit 1\"";
    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");

    let summary_idx = text
        .find("--- failure summary ---")
        .expect("missing failure summary section");
    let exit_idx = text.find("exit: 1").expect("missing exit line");
    assert!(
        summary_idx < exit_idx,
        "summary should come before exit line: {text}"
    );

    let summary_section = &text[summary_idx..exit_idx];
    assert!(
        summary_section.contains("test foo ... FAILED"),
        "got: {text}"
    );
    assert!(
        summary_section.contains("thread 'main' panicked at"),
        "got: {text}"
    );
}

#[tokio::test]
async fn failing_command_with_no_recognizable_lines_gets_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let cmd = "sh -c 'echo nothing interesting; exit 1'";
    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");
    assert!(
        text.contains("(no failure lines recognized; see stderr below)"),
        "got: {text}"
    );
}

// ---------------------------------------------------------------------
// Fix 3: `command` is accepted as an alias for `cmd`.
// ---------------------------------------------------------------------

#[tokio::test]
async fn command_alias_is_accepted_for_cmd() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"command": "echo ok"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.starts_with("command: echo ok\n"), "got: {text}");
    assert!(text.contains("ok"), "got: {text}");
}

// ---------------------------------------------------------------------
// Richer failure summaries: panic context, rustc error context, and
// stripping passing-test noise from the raw output section.
// ---------------------------------------------------------------------

/// Writes a script file with the given canned output to stdout, then
/// `exit 1`, and returns a shell command string that runs it.
fn script_cmd(dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    format!("sh {}", path.display())
}

#[tokio::test]
async fn panic_context_lines_are_included_in_summary() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let script = r#"#!/bin/sh
cat <<'EOF'
running 1 test
test parse_validate::rejects_bad_msg ... FAILED

failures:

---- parse_validate::rejects_bad_msg stdout ----

thread 'parse_validate::rejects_bad_msg' panicked at core/tests/parse_validate.rs:810:5:
assertion failed: msg.contains("x")
  left: "a"
 right: "b"
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    parse_validate::rejects_bad_msg

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
EOF
exit 1
"#;
    let cmd = script_cmd(dir.path(), "panic.sh", script);

    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");

    let summary_idx = text.find("--- failure summary ---").unwrap();
    let exit_idx = text.find("exit: 1").unwrap();
    let summary_section = &text[summary_idx..exit_idx];

    assert!(
        summary_section.contains("panicked at core/tests/parse_validate.rs:810:5:"),
        "got: {text}"
    );
    assert!(
        summary_section.contains(r#"assertion failed: msg.contains("x")"#),
        "got: {text}"
    );
    assert!(summary_section.contains(r#"left: "a""#), "got: {text}");
    assert!(summary_section.contains(r#"right: "b""#), "got: {text}");
}

#[tokio::test]
async fn rustc_error_context_lines_are_included_in_summary() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let script = r#"#!/bin/sh
cat <<'EOF' 1>&2
error[E0599]: no method named `foo` found for struct `Skill` in the current scope
  --> core/src/skill.rs:12:5
   |
12 |     self.foo()
   |          ^^^ method not found in `Skill`

error: aborting due to 1 previous error
EOF
exit 1
"#;
    let cmd = script_cmd(dir.path(), "compile_err.sh", script);

    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");

    let summary_idx = text.find("--- failure summary ---").unwrap();
    let exit_idx = text.find("exit: 1").unwrap();
    let summary_section = &text[summary_idx..exit_idx];

    assert!(
        summary_section.contains("error[E0599]: no method named `foo` found"),
        "got: {text}"
    );
    assert!(
        summary_section.contains("--> core/src/skill.rs:12:5"),
        "got: {text}"
    );
}

#[tokio::test]
async fn raw_output_omits_passing_test_lines_and_reports_count() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let mut script = String::from("#!/bin/sh\ncat <<'EOF'\nrunning 150 tests\n");
    for i in 0..149 {
        script.push_str(&format!("test case_{i} ... ok\n"));
    }
    script.push_str("test y ... FAILED\nEOF\nexit 1\n");

    let cmd = script_cmd(dir.path(), "many_passing.sh", &script);

    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");

    // The raw section (after the exit line) should have dropped the 150
    // "... ok" style lines (149 test lines + the "running 150 tests"
    // banner) and reported the count, while keeping the FAILED line.
    let exit_idx = text.find("exit: 1").expect("missing exit line");
    let raw_section = &text[exit_idx..];

    assert!(!raw_section.contains("case_0 ... ok"), "got: {text}");
    assert!(!raw_section.contains("running 150 tests"), "got: {text}");
    assert!(raw_section.contains("test y ... FAILED"), "got: {text}");
    assert!(
        raw_section.contains("(150 passing test lines omitted)"),
        "got: {text}"
    );
}

// ---------------------------------------------------------------------
// A passing filtered run gets a note pointing at the unfiltered run.
// ---------------------------------------------------------------------

#[tokio::test]
async fn passing_run_with_filtered_tests_gets_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let cmd = "sh -c \"printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 17 \
               filtered out; finished in 0.00s\\n'\"";
    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: pass"), "got: {text}");
    assert!(
        text.contains(
            "note: 17 test(s) were filtered out by this command; run it without the filter \
             before approving."
        ),
        "got: {text}"
    );
}

#[tokio::test]
async fn passing_run_with_zero_filtered_gets_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let cmd = "sh -c \"printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 \
               filtered out; finished in 0.00s\\n'\"";
    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: pass"), "got: {text}");
    assert!(
        !text.contains("filtered out by this command"),
        "got: {text}"
    );
}

#[tokio::test]
async fn failing_run_with_filtered_tests_gets_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let cmd = "sh -c \"printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; \
               17 filtered out; finished in 0.00s\\n'; exit 1\"";
    let out = tool().call(json!({"cmd": cmd}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: fail (exit 1)"), "got: {text}");
    assert!(
        !text.contains("filtered out by this command"),
        "got: {text}"
    );
}

#[tokio::test]
async fn passing_run_with_no_filtered_line_gets_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"cmd": "echo ok"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("result: pass"), "got: {text}");
    assert!(
        !text.contains("filtered out by this command"),
        "got: {text}"
    );
}
