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
                "big.txt exists with 41 lines; write_file replaces the whole file and you would be writing it from memory. Use edit_file (old/new, or after/before + new) for changes to an existing file. If the file is beyond repair, `revert_file` restores its content from before this session's first edit."
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(on_disk, original.as_bytes(), "file must be byte-identical");
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
