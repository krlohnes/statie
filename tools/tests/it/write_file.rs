use super::support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::{EXISTS_WITH_LINES_SUFFIX, ReadFile, WriteFile};

fn write_tool() -> WriteFile {
    WriteFile {
        max_output_bytes: 200_000,
    }
}

fn read_tool() -> ReadFile {
    ReadFile::new(200_000)
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

fn lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[tokio::test]
async fn rewriting_large_existing_file_is_rejected_and_leaves_it_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("big.txt");
    let original = lines(41);
    std::fs::write(&file, &original).unwrap();

    let err = write_tool()
        .call(json!({"path": "big.txt", "content": "replaced"}), &ctx)
        .await
        .unwrap_err();

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                format!("big.txt exists with 41 lines{EXISTS_WITH_LINES_SUFFIX}")
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(on_disk, original.as_bytes(), "file must be byte-identical");
}

// =======================================================================
// BACKLOG.md item 15: `confirmed_full_read` skips the line-cap refusal.
// =======================================================================

#[tokio::test]
async fn a_62_line_file_is_refused_with_the_extended_message_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("big.txt");
    let original = lines(62);
    std::fs::write(&file, &original).unwrap();

    let err = write_tool()
        .call(json!({"path": "big.txt", "content": "replaced"}), &ctx)
        .await
        .unwrap_err();

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                format!("big.txt exists with 62 lines{EXISTS_WITH_LINES_SUFFIX}")
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(on_disk, original.as_bytes(), "file must be byte-identical");
}

#[tokio::test]
async fn a_62_line_file_is_rewritten_when_confirmed_full_read_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("big.txt");
    std::fs::write(&file, lines(62)).unwrap();

    write_tool()
        .call(
            json!({"path": "big.txt", "content": "replaced", "confirmed_full_read": true}),
            &ctx,
        )
        .await
        .unwrap();

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "replaced");
}

#[test]
fn confirmed_full_read_is_not_in_the_rendered_args_schema() {
    let schema = write_tool().args_schema();
    let schema_str = schema.as_value().to_string();
    assert!(
        !schema_str.contains("confirmed_full_read"),
        "schema must not document confirmed_full_read (harness-only, BACKLOG.md item 15): {schema_str}"
    );
}

#[tokio::test]
async fn rewriting_existing_file_at_the_threshold_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("ok.txt");
    std::fs::write(&file, lines(40)).unwrap();

    write_tool()
        .call(json!({"path": "ok.txt", "content": "replaced"}), &ctx)
        .await
        .unwrap();

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(on_disk, "replaced");
}

#[tokio::test]
async fn writing_a_new_path_succeeds_regardless_of_content_size() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let content = lines(200);
    write_tool()
        .call(json!({"path": "new.txt", "content": content.clone()}), &ctx)
        .await
        .unwrap();

    let on_disk = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
    assert_eq!(on_disk, content);
}

// ---------------------------------------------------------------------
// Unit 40: the description states the tool's forms and failure texts up
// front, at most 130 words, built from the same const the tool's
// large-existing-file refusal uses (D13).
// ---------------------------------------------------------------------

#[test]
fn description_names_every_argument_and_quotes_the_const_failure_text() {
    let desc = write_tool().description().to_string();

    for arg in ["path", "content"] {
        assert!(desc.contains(arg), "description missing `{arg}`: {desc}");
    }
    assert!(
        desc.contains("replaces the whole file"),
        "description must say it replaces the whole file: {desc}"
    );
    assert!(
        desc.contains(EXISTS_WITH_LINES_SUFFIX),
        "description missing EXISTS_WITH_LINES_SUFFIX: {desc}"
    );

    assert!(
        desc.split_whitespace().count() <= 130,
        "description is {} words, over the 130-word cap: {desc}",
        desc.split_whitespace().count()
    );
}

#[tokio::test]
async fn large_file_refusal_matches_the_const_the_description_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("big.txt");
    std::fs::write(&file, lines(50)).unwrap();

    let err = write_tool()
        .call(json!({"path": "big.txt", "content": "replaced"}), &ctx)
        .await
        .unwrap_err();

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                format!("big.txt exists with 50 lines{EXISTS_WITH_LINES_SUFFIX}")
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }
}
