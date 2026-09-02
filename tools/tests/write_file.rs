mod support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::{ReadFile, WriteFile};

fn write_tool() -> WriteFile {
    WriteFile {
        max_output_bytes: 200_000,
    }
}

fn read_tool() -> ReadFile {
    ReadFile {
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn creates_nested_file_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = write_tool()
        .call(json!({"path": "a/b/c.txt", "content": "hello world"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    assert!(text.starts_with("wrote 11 bytes to a/b/c.txt"));

    assert!(dir.path().join("a/b/c.txt").exists());

    let read_out = read_tool()
        .call(json!({"path": "a/b/c.txt"}), &ctx)
        .await
        .unwrap();
    assert!(read_out.to_string().contains("hello world"));
}

#[tokio::test]
async fn no_tmp_files_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    write_tool()
        .call(json!({"path": "x/y.txt", "content": "abc"}), &ctx)
        .await
        .unwrap();

    let entries: Vec<_> = std::fs::read_dir(dir.path().join("x"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();

    assert_eq!(entries, vec!["y.txt".to_string()]);
    assert!(!entries.iter().any(|n| n.contains(".tmp-")));
}

#[tokio::test]
async fn parent_dir_escape_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = write_tool()
        .call(json!({"path": "../escape.txt", "content": "x"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Sandbox(_)), "got: {err:?}");
}
