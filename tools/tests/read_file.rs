mod support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::ReadFile;

fn tool() -> ReadFile {
    ReadFile {
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn reads_a_whole_small_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"path": "f.txt"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.starts_with("f.txt (5 lines, showing 1-5)\n"));
    assert!(text.contains("    1 | one"));
    assert!(text.contains("    5 | five"));
}

#[tokio::test]
async fn offset_and_limit_window() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"path": "f.txt", "offset": 2, "limit": 2}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.starts_with("f.txt (5 lines, showing 2-3)\n"));
    assert!(text.contains("    2 | two"));
    assert!(text.contains("    3 | three"));
    assert!(!text.contains("| four"));
}

#[tokio::test]
async fn outside_root_is_sandbox_error() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool()
        .call(json!({"path": "../outside.txt"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Sandbox(_)), "got: {err:?}");
}

#[tokio::test]
async fn missing_file_is_execution_failed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool()
        .call(json!({"path": "nope.txt"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::ExecutionFailed(_)), "got: {err:?}");
}

#[tokio::test]
async fn invalid_args_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool().call(json!({}), &ctx).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
}
