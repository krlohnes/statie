use super::support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::ReadFile;

fn tool() -> ReadFile {
    ReadFile::new(200_000)
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
    // Reached EOF: no trailing continuation line.
    assert!(!text.contains("more lines"));
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
    assert!(text.contains("... 2 more lines; call read_file with offset=4 to continue"));
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

#[tokio::test]
async fn default_window_is_100_lines_with_continuation() {
    let dir = tempfile::tempdir().unwrap();
    // 150 short lines so the byte cap never kicks in; only `limit` bounds
    // the window.
    let content = (1..=150)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"path": "f.txt"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(text.starts_with("f.txt (150 lines, showing 1-100)\n"));
    assert!(text.contains("    1 | line1"));
    assert!(text.contains("  100 | line100"));
    assert!(!text.contains("| line101"));
    assert!(text.contains("... 50 more lines; call read_file with offset=101 to continue"));
}

#[tokio::test]
async fn byte_cap_cuts_before_limit_at_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    // 100 lines of 200 bytes each (well within the 100-line `limit`, but
    // the rendered window vastly exceeds the byte cap), so the byte cap —
    // not `limit` — must be what ends the window.
    let long_line = "x".repeat(200);
    let content = std::iter::repeat_n(long_line, 100)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"path": "f.txt"}), &ctx).await.unwrap();
    let text = out.to_string();

    // Header says 100 lines total, and `to` must match the last numbered
    // line actually rendered in the body.
    assert!(text.starts_with("f.txt (100 lines, showing 1-"));

    let header_line = text.lines().next().unwrap();
    let to: usize = header_line
        .rsplit('-')
        .next()
        .unwrap()
        .trim_end_matches(')')
        .parse()
        .unwrap();
    assert!(
        to < 100,
        "expected the byte cap to cut before line 100, got to={to}"
    );

    let last_numbered = format!("{to:>5} | ");
    assert!(text.contains(&last_numbered));
    let next_numbered = format!("{:>5} | ", to + 1);
    assert!(!text.contains(&next_numbered));

    let remaining = 100 - to;
    assert!(text.contains(&format!(
        "... window cut at 6000 bytes ({remaining} more lines); call read_file with offset={}",
        to + 1
    )));
}

#[tokio::test]
async fn window_reaching_eof_has_no_continuation_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"path": "f.txt", "limit": 10}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.starts_with("f.txt (3 lines, showing 1-3)\n"));
    assert!(!text.contains("more lines"));
}

#[tokio::test]
async fn offset_past_eof_behaves_as_before() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"path": "f.txt", "offset": 100}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    // offset is clamped to the last line, showing just that line, with no
    // trailing continuation line since there's nothing left to page to.
    assert!(text.starts_with("f.txt (3 lines, showing 3-3)\n"));
    assert!(text.contains("    3 | three"));
    assert!(!text.contains("more lines"));
}

// ---------------------------------------------------------------------
// Fix 3: `file` and `file_path` are accepted as aliases for `path`.
// ---------------------------------------------------------------------

#[tokio::test]
async fn file_alias_is_accepted_for_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"file": "f.txt"}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(
        text.starts_with("f.txt (3 lines, showing 1-3)\n"),
        "got: {text}"
    );
}

#[tokio::test]
async fn file_path_alias_is_accepted_for_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"file_path": "f.txt"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.starts_with("f.txt (3 lines, showing 1-3)\n"),
        "got: {text}"
    );
}
