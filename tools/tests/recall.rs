mod support;

use std::sync::Arc;

use serde_json::json;
use statie_core::{LogEntry, LogKind, MemoryLog, SandboxRoot, SessionLog, Tool, ToolCtx, ToolError};
use statie_tools::Recall;
use tokio_util::sync::CancellationToken;

fn tool() -> Recall {
    Recall {
        max_output_bytes: 200_000,
    }
}

fn entry(step: u64, kind: LogKind, payload: serde_json::Value) -> LogEntry {
    LogEntry {
        step,
        kind,
        ts: "2026-09-01T00:00:00Z".to_string(),
        payload,
    }
}

fn seeded_ctx(dir: &std::path::Path) -> ToolCtx {
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"output": "cargo test failed parser"}),
    ))
    .unwrap();
    log.append(entry(2, LogKind::Reasoning, json!({"text": "parser parser parser"})))
        .unwrap();
    log.append(entry(3, LogKind::User, json!({"text": "please fix the parser"})))
        .unwrap();

    ToolCtx {
        root: SandboxRoot::new(dir).unwrap(),
        log,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn finds_tool_output_and_excludes_reasoning() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = seeded_ctx(dir.path());

    let out = tool().call(json!({"query": "parser"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.contains("cargo test failed parser"), "got: {text}");
    assert!(!text.contains("parser parser parser"), "got: {text}");
}

#[tokio::test]
async fn reasoning_kind_is_forbidden() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = seeded_ctx(dir.path());

    let err = tool()
        .call(json!({"query": "parser", "kind": "reasoning"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
}

#[tokio::test]
async fn kind_filter_user_returns_only_user_entry() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = seeded_ctx(dir.path());

    let out = tool()
        .call(json!({"query": "parser", "kind": "user"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("please fix the parser"), "got: {text}");
    assert!(!text.contains("cargo test failed parser"), "got: {text}");
    assert!(text.contains("[step 3] user"), "got: {text}");
}

#[tokio::test]
async fn nonsense_query_returns_no_matching_entries() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = seeded_ctx(dir.path());

    let out = tool()
        .call(json!({"query": "xyzzy_no_such_term"}), &ctx)
        .await
        .unwrap();

    assert_eq!(out.to_string(), "no matching entries");
}
