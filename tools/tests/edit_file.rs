mod support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::EditFile;

fn edit_tool() -> EditFile {
    EditFile {
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn single_match_is_replaced_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "a.txt", "old": "line two", "new": "line TWO"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    let first_line = text.lines().next().unwrap();
    assert_eq!(
        first_line,
        "edited a.txt: replaced 1 occurrence (lines 2-2)"
    );
    assert!(text.contains("    2 | line TWO"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line one\nline TWO\nline three\n");
}

#[tokio::test]
async fn context_window_includes_new_text_and_neighbors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("b.txt");
    let content = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
    std::fs::write(&file, content).unwrap();

    let out = edit_tool()
        .call(json!({"path": "b.txt", "old": "l5", "new": "FIVE"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    // Replaced line 5; window should be lines 2-8 (3 before, 3 after, capped
    // at file end).
    assert!(text.contains("    2 | l2"), "output was:\n{text}");
    assert!(text.contains("    5 | FIVE"), "output was:\n{text}");
    assert!(text.contains("    8 | l8"), "output was:\n{text}");
    assert!(!text.contains("    1 | l1"), "output was:\n{text}");
}

#[tokio::test]
async fn zero_matches_is_execution_failed_and_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("c.txt");
    std::fs::write(&file, "hello world\n").unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "c.txt", "old": "not present", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert!(msg.contains("not found"), "got: {msg}"),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "hello world\n");
}

#[tokio::test]
async fn multiple_matches_is_execution_failed_and_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("d.txt");
    std::fs::write(&file, "dup\ndup\n").unwrap();

    let err = edit_tool()
        .call(json!({"path": "d.txt", "old": "dup", "new": "x"}), &ctx)
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert!(msg.contains("occurs 2 times"), "got: {msg}"),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "dup\ndup\n");
}

#[tokio::test]
async fn empty_old_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("e.txt");
    std::fs::write(&file, "content\n").unwrap();

    let err = edit_tool()
        .call(json!({"path": "e.txt", "old": "", "new": "x"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
}

#[tokio::test]
async fn missing_file_is_execution_failed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = edit_tool()
        .call(json!({"path": "nope.txt", "old": "a", "new": "b"}), &ctx)
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert!(msg.contains("no such file"), "got: {msg}"),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn sandbox_escape_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = edit_tool()
        .call(json!({"path": "../x", "old": "a", "new": "b"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Sandbox(_)), "got: {err:?}");
}

#[test]
fn description_and_schema_warn_that_new_replaces_old_entirely() {
    let tool = edit_tool();
    assert!(
        tool.description().contains("REPLACES"),
        "description was: {}",
        tool.description()
    );

    let schema = tool.args_schema();
    let schema_str = schema.as_value().to_string();
    assert!(
        schema_str.contains("replaces `old` entirely"),
        "schema was: {schema_str}"
    );
}

#[tokio::test]
async fn whitespace_only_difference_is_matched_with_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("g.txt");
    // File uses 4-space indent.
    let content = "fn f() {\n    if true {\n        do_thing();\n    }\n}\n";
    std::fs::write(&file, content).unwrap();

    // `old` uses 2-space indent and a trailing space on one line, so it
    // won't match exactly but does match ignoring whitespace.
    let old = "  if true {\n    do_thing(); \n  }";
    let new = "    if true {\n        do_other_thing();\n    }";

    let out = edit_tool()
        .call(json!({"path": "g.txt", "old": old, "new": new}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.contains("note: matched `old` ignoring whitespace differences"),
        "output was:\n{text}"
    );
    assert!(text.contains("edited g.txt"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    let expected = "fn f() {\n    if true {\n        do_other_thing();\n    }\n}\n";
    assert_eq!(on_disk, expected);
}

#[tokio::test]
async fn whitespace_tolerant_match_must_be_unique() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("h.txt");
    // Two windows are identical modulo whitespace (extra internal spaces),
    // but neither is an exact substring match for `old` (whose single
    // space collapses from each line's doubled internal whitespace, so
    // the exact-match path finds zero matches).
    let content = "  same  line\nsame  line\n    same  line\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "h.txt", "old": "same line", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("ignoring whitespace"), "got: {msg}")
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn not_found_reports_closest_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("i.txt");
    let content = "one\nlet result = compute_value(x, y);\nthree\n";
    std::fs::write(&file, content).unwrap();

    // Typo'd anchor: "compute_values" vs "compute_value".
    let err = edit_tool()
        .call(
            json!({"path": "i.txt", "old": "let result = compute_values(x, y);", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest line is"), "got: {msg}");
            assert!(msg.contains("compute_value"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn multiline_replacement_changes_line_count_and_reports_range() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "before\nold one\nold two\nafter\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "f.txt",
                "old": "old one\nold two",
                "new": "new a\nnew b\nnew c"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    let first_line = text.lines().next().unwrap();
    assert_eq!(
        first_line,
        "edited f.txt: replaced 1 occurrence (lines 2-4)"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "before\nnew a\nnew b\nnew c\nafter\n");

    // Following line ("after") is intact and correctly renumbered.
    assert!(text.contains("    5 | after"), "output was:\n{text}");
}
