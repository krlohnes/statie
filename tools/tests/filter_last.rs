mod support;

use std::sync::Arc;

use serde_json::json;
use statie_core::{
    LogEntry, LogKind, MemoryLog, SandboxRoot, SessionLog, Tool, ToolCtx, ToolError,
};
use statie_tools::FilterLastToolCall;
use tokio_util::sync::CancellationToken;

fn tool() -> FilterLastToolCall {
    FilterLastToolCall {
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

fn ctx_with_log(dir: &std::path::Path, log: Arc<MemoryLog>) -> ToolCtx {
    ToolCtx {
        root: SandboxRoot::new(dir).unwrap(),
        log,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn matches_lines_with_correct_line_numbers_and_header() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    let output = "line one\nerror: bad thing\nline three\nerror: worse thing\nline five";
    log.append(entry(
        7,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": output, "truncated": true}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "error:"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.contains("2 matching line(s) of 5 in the last `shell` output (step 7)"),
        "got: {text}"
    );
    assert!(text.contains("2: error: bad thing"), "got: {text}");
    assert!(text.contains("4: error: worse thing"), "got: {text}");
    assert!(!text.contains("line one"), "got: {text}");
}

#[tokio::test]
async fn skips_a_more_recent_filter_last_output_and_targets_the_one_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": "needle here\nnothing", "truncated": false}),
    ))
    .unwrap();
    log.append(entry(
        2,
        LogKind::ToolOutput,
        json!({
            "name": "filter_last_tool_call",
            "output": "1 matching line(s) of 2 in the last `shell` output (step 1)\n1: needle here",
            "truncated": false
        }),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.contains("in the last `shell` output (step 1)"),
        "got: {text}"
    );
    assert!(text.contains("1: needle here"), "got: {text}");
}

#[tokio::test]
async fn context_includes_neighbours_and_separator_between_distant_groups() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    let output = "a\nb\nMATCH one\nd\ne\nf\ng\nh\nMATCH two\nj";
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": output, "truncated": false}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "MATCH", "context": 1}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    // First group: lines 2,3,4 (b, MATCH one, d)
    assert!(text.contains("2- b"), "got: {text}");
    assert!(text.contains("3: MATCH one"), "got: {text}");
    assert!(text.contains("4- d"), "got: {text}");
    // Second group: lines 8,9,10 (h, MATCH two, j)
    assert!(text.contains("8- h"), "got: {text}");
    assert!(text.contains("9: MATCH two"), "got: {text}");
    assert!(text.contains("10- j"), "got: {text}");
    // Separator between the two non-adjacent groups.
    assert!(text.contains("\n--\n"), "got: {text}");
}

#[tokio::test]
async fn case_insensitive_matches() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": "ERROR: boom\nfine", "truncated": false}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "error:", "case_insensitive": true}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1: ERROR: boom"), "got: {text}");
}

#[tokio::test]
async fn max_results_truncation_trailer() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    let output = (1..=5)
        .map(|i| format!("hit {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": output, "truncated": false}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "hit", "max_results": 2}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("2 matching line(s) of 5"), "got: {text}");
    assert!(
        text.contains("…(stopped at 2 matches; narrow the pattern)"),
        "got: {text}"
    );
}

#[tokio::test]
async fn no_prior_tool_output_is_execution_failed() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    let ctx = ctx_with_log(dir.path(), log);

    let err = tool()
        .call(json!({"pattern": "anything"}), &ctx)
        .await
        .unwrap_err();

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("no previous tool output"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_regex_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": "line", "truncated": false}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let err = tool()
        .call(json!({"pattern": "("}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
}

#[tokio::test]
async fn no_matches_returns_exact_message() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    log.append(entry(
        1,
        LogKind::ToolOutput,
        json!({"name": "shell", "output": "a\nb\nc", "truncated": false}),
    ))
    .unwrap();
    let ctx = ctx_with_log(dir.path(), log);

    let out = tool()
        .call(json!({"pattern": "zzz_no_such_thing"}), &ctx)
        .await
        .unwrap();

    assert_eq!(
        out.to_string(),
        "no lines match /zzz_no_such_thing/ in the last `shell` output (3 lines)"
    );
}
