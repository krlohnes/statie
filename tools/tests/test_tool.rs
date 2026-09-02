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
    assert_eq!(detect_test_command(dir.path()), Some("cargo test".to_string()));
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

    let out = tool()
        .call(json!({"cmd": "echo ok"}), &ctx)
        .await
        .unwrap();
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
