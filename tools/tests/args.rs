//! Tests for the argument-normalization rule that joins a JSON array of
//! strings into a single `"\n"`-joined string for string-typed tool
//! arguments (`tools/src/common.rs::join_string_array_args`). This exists
//! because schema-constrained models occasionally wrap a single string
//! anchor/argument in a one-element array (e.g. `"after": ["line"]`), which
//! previously failed deserialization outright.

mod support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::{EditFile, Shell, WriteFile};

fn edit_tool() -> EditFile {
    EditFile {
        max_output_bytes: 200_000,
    }
}

fn write_tool() -> WriteFile {
    WriteFile {
        max_output_bytes: 200_000,
    }
}

fn shell_tool() -> Shell {
    Shell {
        default_timeout_secs: 120,
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn edit_file_after_as_one_element_array_behaves_as_string() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    edit_tool()
        .call(
            json!({
                "path": "a.txt",
                "after": ["line one"],
                "new": "INSERTED"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let on_disk = std::fs::read_to_string(&file).unwrap();

    // Compare against the exact-string form on a fresh copy of the original
    // file, to assert the array form behaves *exactly* like the string form.
    let dir2 = tempfile::tempdir().unwrap();
    let ctx2 = support::ctx_for(dir2.path());
    let file2 = dir2.path().join("a.txt");
    std::fs::write(&file2, "line one\nline two\nline three\n").unwrap();
    edit_tool()
        .call(
            json!({
                "path": "a.txt",
                "after": "line one",
                "new": "INSERTED"
            }),
            &ctx2,
        )
        .await
        .unwrap();
    let expected = std::fs::read_to_string(&file2).unwrap();

    assert_eq!(on_disk, expected);
    assert_eq!(on_disk, "line one\nINSERTED\nline two\nline three\n");
}

#[tokio::test]
async fn edit_file_after_as_multi_element_array_joins_with_newline() {
    // A two-element anchor array ["a", "b"] should behave exactly as the
    // anchor string "a\nb" (a two-line anchor block).
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("b.txt");
    std::fs::write(&file, "a\nb\nc\n").unwrap();

    edit_tool()
        .call(
            json!({
                "path": "b.txt",
                "after": ["a", "b"],
                "new": "INSERTED"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let on_disk = std::fs::read_to_string(&file).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let ctx2 = support::ctx_for(dir2.path());
    let file2 = dir2.path().join("b.txt");
    std::fs::write(&file2, "a\nb\nc\n").unwrap();
    edit_tool()
        .call(
            json!({
                "path": "b.txt",
                "after": "a\nb",
                "new": "INSERTED"
            }),
            &ctx2,
        )
        .await
        .unwrap();
    let expected = std::fs::read_to_string(&file2).unwrap();

    assert_eq!(on_disk, expected);
    assert_eq!(on_disk, "a\nb\nINSERTED\nc\n");
}

#[tokio::test]
async fn edit_file_after_array_with_non_string_element_still_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("c.txt");
    std::fs::write(&file, "line one\n").unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "c.txt",
                "after": [1],
                "new": "x"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::InvalidArgs(msg) => {
            assert!(
                msg.contains("invalid type") && msg.contains("string"),
                "expected serde's invalid-type-expected-string message, got: {msg}"
            );
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }

    // File must be untouched.
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line one\n");
}

#[tokio::test]
async fn write_file_content_as_array_joins_with_newline() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    write_tool()
        .call(json!({"path": "out.txt", "content": ["x", "y"]}), &ctx)
        .await
        .unwrap();

    let on_disk = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(on_disk, "x\ny");
}

#[tokio::test]
async fn shell_cmd_as_one_element_array_runs_command() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = shell_tool()
        .call(json!({"cmd": ["echo hi"]}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("exit: 0"), "got: {text}");
    assert!(text.contains("hi"), "got: {text}");
}
