mod support;

use std::time::Duration;

use serde_json::json;
use statie_core::{Tool, ToolCtx, ToolError};
use statie_tools::{Shell, ToolConfig, default_tools};

fn tool() -> Shell {
    Shell {
        default_timeout_secs: 120,
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn captures_exit_code_stdout_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"cmd": "echo hi; echo err 1>&2; exit 3"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("exit: 3"), "got: {text}");
    assert!(text.contains("hi"), "got: {text}");
    assert!(text.contains("err"), "got: {text}");
}

#[tokio::test]
async fn runs_in_sandbox_root() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"cmd": "pwd"}), &ctx).await.unwrap();
    let text = out.to_string();

    let canonical = ctx.root.path().to_string_lossy().to_string();
    assert!(text.contains(&canonical), "got: {text}, expected root: {canonical}");
}

#[tokio::test]
async fn cancel_kills_and_errors_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let cancel = ctx.cancel.clone();

    let ctx_for_task = ToolCtx {
        root: statie_core::SandboxRoot::new(dir.path()).unwrap(),
        log: ctx.log.clone(),
        cancel: ctx.cancel.clone(),
    };

    let handle = tokio::spawn(async move { tool().call(json!({"cmd": "sleep 10"}), &ctx_for_task).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("did not finish within 1s")
        .expect("task panicked");

    let err = result.unwrap_err();
    assert!(matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("cancelled")), "got: {err:?}");
}

#[tokio::test]
async fn timeout_errors_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool().call(json!({"cmd": "sleep 10", "timeout_secs": 1}), &ctx),
    )
    .await
    .expect("did not finish within 3s");

    let err = result.unwrap_err();
    assert!(matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("timed out")), "got: {err:?}");
}

#[test]
fn allow_shell_false_excludes_shell_and_test() {
    let cfg = ToolConfig {
        allow_shell: false,
        ..ToolConfig::default()
    };
    let tools = default_tools(&cfg);
    assert!(tools.get("shell").is_none());
    assert!(tools.get("test").is_none());
}
