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
fn description_and_schema_document_anchor_insertion() {
    let tool = edit_tool();
    assert!(
        tool.description().contains("after/before"),
        "description was: {}",
        tool.description()
    );
    assert!(
        tool.description().contains("Prefer the anchor form"),
        "description was: {}",
        tool.description()
    );

    let schema = tool.args_schema();
    let schema_str = schema.as_value().to_string();
    assert!(
        schema_str.contains("OR `after`/`before`"),
        "schema was: {schema_str}"
    );
    assert!(
        schema_str.contains("Easiest way to add code: anchor on one line"),
        "schema was: {schema_str}"
    );
    assert!(
        schema_str.contains("if `old` is given, `after`/`before` are ignored"),
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
            assert!(msg.contains("closest match is near line"), "got: {msg}");
            assert!(msg.contains("compute_value"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn not_found_shows_numbered_window_around_closest_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("j.txt");
    // 60-line file; line 40 is the unique close match for our typo'd `old`.
    let mut content = String::new();
    for i in 1..=60 {
        if i == 40 {
            content.push_str("let result = compute_value(x, y);\n");
        } else {
            content.push_str(&format!("filler line {i}\n"));
        }
    }
    std::fs::write(&file, &content).unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "j.txt", "old": "let result = compute_values(x, y);", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest match is near line 40"), "got: {msg}");
            assert!(
                msg.contains("   40 | let result = compute_value(x, y);"),
                "got: {msg}"
            );
            assert!(msg.contains("   37 | filler line 37"), "got: {msg}");
            assert!(
                msg.contains(
                    "copy the lines you want to replace from this window into `old`, verbatim."
                ),
                "got: {msg}"
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn not_found_window_never_exceeds_forty_lines() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("k.txt");
    // Long file (200 lines) with a large `old` (30 lines) whose closest
    // match anchors near line 40: window_end would otherwise be
    // 40 + max(12, 30+4) = 40 + 34 = 74, spanning 74-37+1 = 38 lines,
    // so bump old further to force a genuinely oversized window and
    // confirm the 40-line cap kicks in regardless.
    let mut content = String::new();
    for i in 1..=200 {
        if i == 40 {
            content.push_str("let result = compute_value(x, y);\n");
        } else {
            content.push_str(&format!("filler line {i}\n"));
        }
    }
    std::fs::write(&file, &content).unwrap();

    // A large multi-line `old` whose first non-empty line is the typo'd
    // anchor; the rest is filler that won't match anything, but only the
    // first line drives the closest-line heuristic and the line count
    // drives the (uncapped) requested window size.
    let mut old = String::from("let result = compute_values(x, y);\n");
    for i in 0..30 {
        old.push_str(&format!("nonexistent filler {i}\n"));
    }

    let err = edit_tool()
        .call(json!({"path": "k.txt", "old": old, "new": "x"}), &ctx)
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest match is near line 40"), "got: {msg}");
            let window_lines = msg
                .lines()
                .filter(|l| {
                    let trimmed = l.trim_start();
                    trimmed
                        .split(" | ")
                        .next()
                        .map(|n| n.trim().parse::<usize>().is_ok())
                        .unwrap_or(false)
                        && l.contains(" | ")
                })
                .count();
            assert!(
                window_lines <= 40,
                "window had {window_lines} lines, got: {msg}"
            );
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

#[tokio::test]
async fn after_inserts_following_the_unique_anchor_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("anchor_after.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "anchor_after.txt",
                "after": "two",
                "new": "inserted a\ninserted b"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 2 line(s) after line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "one\ntwo\ninserted a\ninserted b\nthree\n");
}

#[tokio::test]
async fn before_inserts_preceding_the_unique_anchor_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("anchor_before.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "anchor_before.txt",
                "before": "two",
                "new": "inserted a\ninserted b"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 2 line(s) before line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "one\ninserted a\ninserted b\ntwo\nthree\n");
}

#[tokio::test]
async fn anchor_whitespace_normalized_match_works_and_is_noted() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("anchor_ws.txt");
    // File line has 4-space indent; anchor arg has 2-space indent and an
    // extra internal space, so it won't match exactly but does match
    // ignoring whitespace.
    std::fs::write(&file, "fn f() {\n    do_thing();\n}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "anchor_ws.txt",
                "after": "  do_thing();",
                "new": "    do_other();"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: matched anchor ignoring whitespace differences"),
        "output was:\n{text}"
    );
    assert!(
        text.contains("inserted 1 line(s) after line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn f() {\n    do_thing();\n    do_other();\n}\n");
}

#[tokio::test]
async fn ambiguous_anchor_names_the_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("anchor_dup.txt");
    std::fs::write(&file, "dup\nother\ndup\ndup\n").unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "anchor_dup.txt", "after": "dup", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("anchor line occurs 3 times"), "got: {msg}");
            assert!(msg.contains("lines 1, 3, 4"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "dup\nother\ndup\ndup\n");
}

#[tokio::test]
async fn missing_anchor_reports_closest_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("anchor_missing.txt");
    let content = "one\nlet result = compute_value(x, y);\nthree\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "anchor_missing.txt",
                "after": "let result = compute_values(x, y);",
                "new": "x"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest match is near line"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn old_and_after_with_different_text_old_wins_and_notes_ignored_after() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("both.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    // `after` is a substring of `old` (its trailing line), matching the
    // live-run scenario: `old` matches the file exactly and should win.
    let out = edit_tool()
        .call(
            json!({
                "path": "both.txt",
                "old": "line one\nline two",
                "after": "line two",
                "new": "line ONE\nline TWO"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: ignored `after`: `old` already locates the text"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line ONE\nline TWO\nline three\n");
}

#[tokio::test]
async fn old_and_before_with_unrelated_text_old_wins_and_notes_ignored_before() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("both_before.txt");
    std::fs::write(&file, "content\nother\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "both_before.txt", "old": "content", "before": "other", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: ignored `before`: `old` already locates the text"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "x\nother\n");
}

#[tokio::test]
async fn old_missing_with_matching_after_still_errors_and_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("old_missing.txt");
    let content = "line one\nline two\nline three\n";
    std::fs::write(&file, content).unwrap();

    // `old` does not occur in the file, but `after` would match on its own;
    // `old`'s presence must still take priority and produce the miss error
    // rather than falling back to inserting via `after`.
    let err = edit_tool()
        .call(
            json!({
                "path": "old_missing.txt",
                "old": "line NOPE",
                "after": "line two",
                "new": "inserted"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("old text not found"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be unchanged on error");
}

#[tokio::test]
async fn old_and_after_with_identical_text_and_empty_new_deletes_block() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("dup.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "dup.txt", "old": "line two", "after": "line two", "new": ""}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line, "edited dup.txt: deleted 1 line(s) (lines 2-2)");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line one\n\nline three\n");
}

#[tokio::test]
async fn old_and_before_with_identical_text_ignoring_surrounding_whitespace_deletes_block() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("dup_trim.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "dup_trim.txt", "old": "line two", "before": "  line two  ", "new": ""}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    let first_line = text.lines().next().unwrap();
    assert_eq!(
        first_line,
        "edited dup_trim.txt: deleted 1 line(s) (lines 2-2)"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line one\n\nline three\n");
}

#[tokio::test]
async fn old_with_empty_new_deletes_and_reports_deleted_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("delete.txt");
    std::fs::write(&file, "keep\ndrop one\ndrop two\nkeep too\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "delete.txt", "old": "drop one\ndrop two", "new": ""}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    let first_line = text.lines().next().unwrap();
    assert_eq!(
        first_line,
        "edited delete.txt: deleted 2 line(s) (lines 2-3)"
    );
    assert!(!text.contains("replaced"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "keep\n\nkeep too\n");
}

#[tokio::test]
async fn none_of_old_after_before_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("none.txt");
    std::fs::write(&file, "content\n").unwrap();

    let err = edit_tool()
        .call(json!({"path": "none.txt", "new": "x"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::InvalidArgs(_)), "got: {err:?}");
}

#[tokio::test]
async fn multiline_after_with_unique_match_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("multiline_anchor.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "multiline_anchor.txt", "after": "one\ntwo", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 1 line(s) after line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "one\ntwo\nx\nthree\n");
}

#[tokio::test]
async fn multiline_after_block_inserts_after_its_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("multiline_after.txt");
    std::fs::write(
        &file,
        "fn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }\n}\n",
    )
    .unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "multiline_after.txt",
                "after": "fn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }",
                "new": "\n    fn other() {}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 2 line(s) after line 3"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "fn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }\n\n    fn other() {}\n}\n"
    );
}

#[tokio::test]
async fn multiline_before_inserts_before_its_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("multiline_before.txt");
    std::fs::write(
        &file,
        "fn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }\n}\n",
    )
    .unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "multiline_before.txt",
                "before": "fn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }",
                "new": "// doc comment"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 1 line(s) before line 1"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "// doc comment\nfn allowed_delegates(&self) -> Vec<String> {\n        vec![]\n    }\n}\n"
    );
}

#[tokio::test]
async fn multiline_anchor_whitespace_difference_matches_with_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("multiline_ws.txt");
    // File uses 4-space indent throughout.
    std::fs::write(
        &file,
        "fn f() {\n    if true {\n        do_thing();\n    }\n}\n",
    )
    .unwrap();

    // Anchor block uses 2-space indent and an extra internal space, so it
    // won't match exactly but does match ignoring per-line whitespace.
    let out = edit_tool()
        .call(
            json!({
                "path": "multiline_ws.txt",
                "after": "  if true {\n    do_thing(); \n  }",
                "new": "    // trailing comment"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: matched anchor ignoring whitespace differences"),
        "output was:\n{text}"
    );
    assert!(
        text.contains("inserted 1 line(s) after line 4"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "fn f() {\n    if true {\n        do_thing();\n    }\n    // trailing comment\n}\n"
    );
}

#[tokio::test]
async fn multiline_anchor_occurring_twice_is_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("multiline_dup.txt");
    std::fs::write(&file, "a\nb\nc\nother\na\nb\nc\nmore\n").unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "multiline_dup.txt", "after": "a\nb\nc", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("anchor line occurs 2 times"), "got: {msg}");
            assert!(msg.contains("lines 1, 5"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "a\nb\nc\nother\na\nb\nc\nmore\n");
}

#[tokio::test]
async fn closest_line_hint_prefers_trait_definition_over_struct() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("skill.rs");

    let mut content = String::new();
    for i in 1..=60 {
        if i == 3 {
            content.push_str("pub struct SkillSpec {\n");
        } else if i == 30 {
            content.push_str("pub trait Skill: Send + Sync + 'static {\n");
        } else {
            content.push_str(&format!("// filler line {i}\n"));
        }
    }
    std::fs::write(&file, &content).unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "skill.rs", "after": "pub trait Skill {", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest match is near line 30"), "got: {msg}");
            assert!(!msg.contains("closest match is near line 3;"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn closest_line_hint_picks_matching_fn_among_several() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("trait_defs.rs");

    let content = "\
pub trait Skill {
    fn name(&self) -> &str;
    fn allows_ask_user(&self) -> bool;
    fn allowed_delegates(&self) -> Vec<String>;
    fn description(&self) -> &str;
}
";
    std::fs::write(&file, content).unwrap();

    // Typo'd anchor (missing the trailing `;`), should still be scored
    // closest to the `allows_ask_user` line rather than any other `fn`
    // line.
    let err = edit_tool()
        .call(
            json!({
                "path": "trait_defs.rs",
                "after": "fn allows_ask_user(&self) -> bool {",
                "new": "x"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("closest match is near line 3"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content);
}

#[tokio::test]
async fn new_without_trailing_newline_still_yields_well_formed_file() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("no_trailing_nl.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "no_trailing_nl.txt", "after": "one", "new": "inserted"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 1 line(s) after line 1"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    // "inserted" is not glued onto "two": the file has each on its own
    // line.
    assert_eq!(on_disk, "one\ninserted\ntwo\nthree\n");
}

#[tokio::test]
async fn after_new_repeats_anchor_verbatim_is_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("repeat_after.rs");
    std::fs::write(&file, "fn a() {}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "repeat_after.rs",
                "after": "fn a() {}",
                "new": "fn a() {}\nfn b() {}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains(
            "note: `new` began with the `after` anchor; inserted only the lines after it"
        ),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn a() {}\nfn b() {}\n");
    assert_eq!(
        on_disk.matches("fn a() {}").count(),
        1,
        "anchor must not be duplicated; on disk:\n{on_disk}"
    );
}

#[tokio::test]
async fn after_new_repeats_anchor_with_indentation_difference_is_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("repeat_after_ws.rs");
    std::fs::write(&file, "    fn a() {}\n").unwrap();

    // `new`'s repeated anchor line has different indentation than both the
    // file's anchor line and the `after` argument itself.
    let out = edit_tool()
        .call(
            json!({
                "path": "repeat_after_ws.rs",
                "after": "    fn a() {}",
                "new": "  fn a() {}\nfn b() {}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains(
            "note: `new` began with the `after` anchor; inserted only the lines after it"
        ),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "    fn a() {}\nfn b() {}\n");
    assert_eq!(
        on_disk.matches("fn a() {}").count(),
        1,
        "anchor must not be duplicated; on disk:\n{on_disk}"
    );
}

#[tokio::test]
async fn before_new_repeats_anchor_verbatim_is_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("repeat_before.rs");
    std::fs::write(&file, "fn a() {}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "repeat_before.rs",
                "before": "fn a() {}",
                "new": "fn z() {}\nfn a() {}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains(
            "note: `new` ended with the `before` anchor; inserted only the lines before it"
        ),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn z() {}\nfn a() {}\n");
    assert_eq!(
        on_disk.matches("fn a() {}").count(),
        1,
        "anchor must not be duplicated; on disk:\n{on_disk}"
    );
}

#[tokio::test]
async fn after_new_equal_to_anchor_is_execution_failed_and_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("repeat_only.rs");
    let content = "fn a() {}\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "repeat_only.rs",
                "after": "fn a() {}",
                "new": "fn a() {}"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert_eq!(
            msg,
            "`new` is just the anchor itself; nothing to insert. To replace the anchored block, use old/new."
        ),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be unchanged on error");
}

#[tokio::test]
async fn after_new_not_starting_with_anchor_is_inserted_whole_without_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("no_repeat.rs");
    std::fs::write(&file, "fn a() {}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "no_repeat.rs",
                "after": "fn a() {}",
                "new": "fn b() {}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        !text.contains("began with the `after` anchor"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn a() {}\nfn b() {}\n");
}

#[tokio::test]
async fn edit_leaving_extra_closing_brace_notes_imbalance() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("unbalanced.rs");
    std::fs::write(&file, "fn a() {}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "unbalanced.rs",
                "old": "fn a() {}",
                "new": "fn a() {}}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains(
            "note: this edit changed the file's brace balance (-1 `}`); check the edited region before running tests."
        ),
        "output was:\n{text}"
    );
}

#[tokio::test]
async fn balanced_edit_has_no_brace_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("balanced.rs");
    std::fs::write(&file, "fn a() {}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "balanced.rs",
                "old": "fn a() {}",
                "new": "fn a() { 1 }"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(!text.contains("brace balance"), "output was:\n{text}");
}

#[tokio::test]
async fn unbalanced_edit_on_non_code_extension_has_no_brace_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("notes.md");
    std::fs::write(&file, "hello\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "notes.md",
                "old": "hello",
                "new": "hello {{{"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(!text.contains("brace balance"), "output was:\n{text}");
}

#[tokio::test]
async fn after_insert_of_verbatim_existing_block_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("dup_block.rs");
    let content = "match x {\n    A => 1,\n    B => 2,\n    C => 3,\n}\n";
    std::fs::write(&file, content).unwrap();

    // "B => 2,\n    C => 3," already occurs verbatim at lines 3-4; trying to
    // insert it again after "A => 1," must be refused, and the file must be
    // byte-identical afterward.
    let err = edit_tool()
        .call(
            json!({
                "path": "dup_block.rs",
                "after": "A => 1,",
                "new": "    B => 2,\n    C => 3,"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert_eq!(
            msg,
            "the text to insert is already in dup_block.rs at lines 3-4, so this edit is already applied; nothing was changed. If the test still fails, the block that is there is wrong, not missing: replace it with old/new, copying `old` from these lines exactly.\n    3 |     B => 2,\n    4 |     C => 3,\n"
        ),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_insert_of_existing_block_with_different_indentation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("dup_block_ws.rs");
    let content = "match x {\n    A => 1,\n    B => 2,\n    C => 3,\n}\n";
    std::fs::write(&file, content).unwrap();

    // Same block as the file already has at lines 3-4, but re-sent with
    // different indentation: still refused (whitespace-tolerant match).
    let err = edit_tool()
        .call(
            json!({
                "path": "dup_block_ws.rs",
                "after": "A => 1,",
                "new": "  B => 2,\n  C => 3,"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("already in dup_block_ws.rs at lines 3-4"),
                "got: {msg}"
            );
            assert!(msg.contains("already applied"), "got: {msg}");
            assert!(
                msg.contains(
                    "the block that is there is wrong, not missing: replace it with old/new"
                ),
                "got: {msg}"
            );
            // The quoted lines must be the file's actual (unindented-as-sent)
            // content, not the differently-indented text the caller sent.
            assert!(msg.contains("    3 |     B => 2,"), "got: {msg}");
            assert!(msg.contains("    4 |     C => 3,"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_insert_of_block_differing_in_one_line_is_inserted() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("almost_dup.rs");
    let content = "match x {\n    A => 1,\n    B => 2,\n    C => 3,\n}\n";
    std::fs::write(&file, content).unwrap();

    // Differs from the existing "B => 2,\n    C => 3," block in one line
    // ("D" instead of "C"), so it is genuinely new text and must be
    // inserted, not refused.
    let out = edit_tool()
        .call(
            json!({
                "path": "almost_dup.rs",
                "after": "A => 1,",
                "new": "    B => 2,\n    D => 3,"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 2 line(s) after line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "match x {\n    A => 1,\n    B => 2,\n    D => 3,\n    B => 2,\n    C => 3,\n}\n"
    );
}

#[tokio::test]
async fn single_line_closing_brace_insert_is_exempt_and_inserted() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("brace.rs");
    // "}" already occurs (twice: closing the fn and the outer scope), but a
    // single-line insertion is exempt from the duplicate check.
    let content = "fn a() {\n    if true {\n    }\n}\n";
    std::fs::write(&file, content).unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "brace.rs",
                "after": "if true {",
                "new": "}"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 1 line(s) after line 2"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn a() {\n    if true {\n}\n    }\n}\n");
}

#[tokio::test]
async fn after_new_stripped_remainder_duplicates_existing_multiline_block_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("strip_then_dup2.rs");
    let content = "fn a() {\n    step_one();\n    step_two();\n    step_three();\n}\n";
    std::fs::write(&file, content).unwrap();

    // `new` = anchor ("fn a() {\n    step_one();") followed by a two-line
    // block ("    step_two();\n    step_three();") that already exists
    // verbatim right after the anchor in the file. After
    // `strip_repeated_anchor` removes the repeated anchor prefix, the
    // remaining two-line block must be recognized as already present and
    // refused rather than duplicated.
    let err = edit_tool()
        .call(
            json!({
                "path": "strip_then_dup2.rs",
                "after": "fn a() {\n    step_one();",
                "new": "fn a() {\n    step_one();\n    step_two();\n    step_three();"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("already in strip_then_dup2.rs at lines 3-4"),
                "got: {msg}"
            );
            assert!(msg.contains("already applied"), "got: {msg}");
            assert!(
                msg.contains(
                    "the block that is there is wrong, not missing: replace it with old/new"
                ),
                "got: {msg}"
            );
            assert!(msg.contains("    3 |     step_two();"), "got: {msg}");
            assert!(msg.contains("    4 |     step_three();"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_insert_of_long_existing_block_caps_quoted_lines_at_40() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("long_dup_block.rs");

    // A 50-line block (lines 2-51) that the caller re-sends verbatim; the
    // quoted duplicate-range in the refusal must be capped at 40 printed
    // lines with a trailing "more lines" note, not print all 50.
    let block_lines: Vec<String> = (1..=50).map(|i| format!("    item_{i}();")).collect();
    let block = block_lines.join("\n");
    let content = format!("fn a() {{\n{block}\n}}\n");
    std::fs::write(&file, &content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "long_dup_block.rs",
                "after": "fn a() {",
                "new": block
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("already in long_dup_block.rs at lines 2-51"),
                "got: {msg}"
            );
            assert!(msg.contains("already applied"), "got: {msg}");
            assert!(
                msg.contains(
                    "the block that is there is wrong, not missing: replace it with old/new"
                ),
                "got: {msg}"
            );
            // First quoted line (line 2) is present...
            assert!(msg.contains("    2 |     item_1();"), "got: {msg}");
            // ...the 40th quoted line (line 41) is the last one printed...
            assert!(msg.contains("   41 |     item_40();"), "got: {msg}");
            // ...and the 41st (line 42) is not, because the window is capped
            // at 40 lines with a trailer instead.
            assert!(!msg.contains("item_41();"), "got: {msg}");
            assert!(msg.contains("... (10 more lines)"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_anchor_written_on_one_line_matches_four_line_struct_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("ui_event.rs");
    // Mirrors the live-run failure: the file has the match arm split across
    // four lines, but the anchor is written on one line.
    let content = "fn f(e: UiEvent) {\n    match e {\n        UiEvent::Maintainer {\n            notes_tokens,\n            next_set,\n        } => {\n            do_thing();\n        }\n    }\n}\n";
    std::fs::write(&file, content).unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "ui_event.rs",
                "after": "UiEvent::Maintainer { notes_tokens, next_set, } => {",
                "new": "// handled"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: matched anchor ignoring line breaks and whitespace (lines 3-6)"),
        "output was:\n{text}"
    );
    // Inserted after the fourth line of the anchor (line 6, the `} => {`
    // line).
    assert!(
        text.contains("inserted 1 line(s) after line 6"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "fn f(e: UiEvent) {\n    match e {\n        UiEvent::Maintainer {\n            notes_tokens,\n            next_set,\n        } => {\n// handled\n            do_thing();\n        }\n    }\n}\n"
    );
}

#[tokio::test]
async fn old_written_on_one_line_replaces_whole_multiline_block() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("ui_event_old.rs");
    let content = "fn f(e: UiEvent) {\n    match e {\n        UiEvent::Maintainer {\n            notes_tokens,\n            next_set,\n        } => {\n            do_thing();\n        }\n    }\n}\n";
    std::fs::write(&file, content).unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "ui_event_old.rs",
                "old": "UiEvent::Maintainer { notes_tokens, next_set, } => {",
                "new": "        UiEvent::Maintainer { notes_tokens, next_set: _ } => {"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("note: matched `old` ignoring line breaks and whitespace (lines 3-6)"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "fn f(e: UiEvent) {\n    match e {\n        UiEvent::Maintainer { notes_tokens, next_set: _ } => {\n            do_thing();\n        }\n    }\n}\n"
    );
}

#[tokio::test]
async fn collapsed_match_inside_longer_identifier_is_still_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("ident_boundary.txt");
    // Collapses (whitespace stripped) to "...foobar..."; a needle "foo"
    // would match as a raw substring, but must be rejected because it lands
    // inside the longer identifier `foobar` rather than at a token
    // boundary.
    let content = "let foobar = 1;\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "ident_boundary.txt", "old": "f\no\no", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => assert!(msg.contains("not found"), "got: {msg}"),
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be unchanged on error");
}

#[tokio::test]
async fn collapsed_match_occurring_twice_is_ambiguous_and_names_both_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("collapsed_dup.txt");
    // Two separate occurrences of a block that, once split across lines
    // differently than the `old` argument, only match in the
    // collapsed-whitespace tier (neither exact nor per-line-normalized,
    // since the line-break positions differ from `old`'s).
    let content = "fn one() {\n    foo(\n        bar,\n    );\n}\nfn two() {\n    foo(\n        bar,\n    );\n}\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({"path": "collapsed_dup.txt", "old": "foo(bar,\n    );", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("occurs 2 times")
                    && msg.contains("ignoring line breaks and whitespace"),
                "got: {msg}"
            );
            assert!(msg.contains("lines 2-4, 7-9"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be unchanged on ambiguity");
}

#[tokio::test]
async fn exact_match_takes_precedence_over_collapsed_whitespace_tier_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("exact_precedence.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "exact_precedence.txt", "old": "line two", "new": "line TWO"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        !text.contains("ignoring line breaks"),
        "output was:\n{text}"
    );
    assert!(!text.contains("ignoring whitespace"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "line one\nline TWO\nline three\n");
}

#[tokio::test]
async fn exact_anchor_match_takes_precedence_over_collapsed_whitespace_tier_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("exact_anchor_precedence.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let out = edit_tool()
        .call(
            json!({"path": "exact_anchor_precedence.txt", "after": "two", "new": "inserted"}),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        !text.contains("ignoring line breaks"),
        "output was:\n{text}"
    );
    assert!(!text.contains("ignoring whitespace"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "one\ntwo\ninserted\nthree\n");
}

#[tokio::test]
async fn after_insert_duplicating_python_constant_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("pricing.py");
    let content = "from formatter import format_amount\n\nTAX_RATE = 0.08\n\n\ndef total(amount):\n    return amount * (1 + TAX_RATE)\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "pricing.py",
                "after": "from formatter import format_amount",
                "new": "TAX_RATE = 0.08"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                "line 3 of pricing.py already has `TAX_RATE = 0.08`; your insert would duplicate it. Use old/new to replace the existing code instead."
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_insert_duplicating_rust_use_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("lib_dup_use.rs");
    let content = "use std::fmt;\nuse std::collections::HashMap;\n\nfn f() {}\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "lib_dup_use.rs",
                "after": "use std::fmt;",
                "new": "use std::collections::HashMap;"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                "line 2 of lib_dup_use.rs already has `use std::collections::HashMap;`; your insert would duplicate it. Use old/new to replace the existing code instead."
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn after_insert_sharing_only_a_non_definition_line_applies() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("pricing_control.py");
    // Control: the inserted block shares a non-definition line
    // ("    return total") with an existing function elsewhere in the
    // file, but is otherwise new code. This must apply normally rather
    // than being refused, since a bare `return` line legitimately recurs
    // and is not "looks like a definition".
    let content =
        "def existing(amount):\n    total = amount\n    return total\n\n\ndef other():\n    pass\n";
    std::fs::write(&file, content).unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "pricing_control.py",
                "after": "def other():",
                "new": "    total = 0\n    return total"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains("inserted 2 line(s) after line 6"),
        "output was:\n{text}"
    );

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk,
        "def existing(amount):\n    total = amount\n    return total\n\n\ndef other():\n    total = 0\n    return total\n    pass\n"
    );
}

#[tokio::test]
async fn before_insert_duplicating_definition_is_also_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("before_dup.rs");
    let content = "const LIMIT: u32 = 10;\nfn f() {}\n";
    std::fs::write(&file, content).unwrap();

    let err = edit_tool()
        .call(
            json!({
                "path": "before_dup.rs",
                "before": "fn f() {}",
                "new": "const LIMIT: u32 = 10;"
            }),
            &ctx,
        )
        .await
        .unwrap_err();

    match &err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                "line 1 of before_dup.rs already has `const LIMIT: u32 = 10;`; your insert would duplicate it. Use old/new to replace the existing code instead."
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, content, "file must be byte-identical on refusal");
}

#[tokio::test]
async fn old_new_edit_that_unbalances_braces_reports_delta_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("brace_note.rs");
    std::fs::write(&file, "fn a() {\n}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "brace_note.rs",
                "old": "fn a() {\n}",
                "new": "fn a() {\n"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(
        text.contains(
            "note: this edit changed the file's brace balance (+1 `{`); check the edited region before running tests."
        ),
        "output was:\n{text}"
    );
}

#[tokio::test]
async fn balance_preserving_edit_has_no_note_line() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("brace_balanced2.rs");
    std::fs::write(&file, "fn a() {\n    1\n}\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "brace_balanced2.rs",
                "old": "1",
                "new": "2"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(!text.contains("note:"), "output was:\n{text}");
}

#[tokio::test]
async fn edit_inside_string_literal_with_lone_brace_has_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("brace_in_string.rs");
    std::fs::write(&file, "fn a() {\n    let s = \"x\";\n}\n").unwrap();

    // Replace the string contents with one containing a lone `}` — since
    // it's inside a string literal, the bracket scanner must not count it,
    // so no note should appear.
    let out = edit_tool()
        .call(
            json!({
                "path": "brace_in_string.rs",
                "old": "\"x\"",
                "new": "\"}\""
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(!text.contains("brace balance"), "output was:\n{text}");

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "fn a() {\n    let s = \"}\";\n}\n");
}

#[tokio::test]
async fn edit_on_already_unbalanced_file_not_touching_braces_has_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("already_unbalanced.rs");
    // Already missing a closing brace before the edit.
    std::fs::write(&file, "fn a() {\n    let x = 1;\n").unwrap();

    let out = edit_tool()
        .call(
            json!({
                "path": "already_unbalanced.rs",
                "old": "let x = 1;",
                "new": "let x = 2;"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let text = out.to_string();
    assert!(!text.contains("brace balance"), "output was:\n{text}");
}
