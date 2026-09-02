mod support;

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
