use super::support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::{EditFile, RevertFile, WriteFile};

fn revert_tool() -> RevertFile {
    RevertFile {
        max_output_bytes: 200_000,
    }
}

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

#[tokio::test]
async fn edit_then_revert_restores_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    let original = "line one\nline two\nline three\n";
    std::fs::write(&file, original).unwrap();

    edit_tool()
        .call(
            json!({"path": "a.txt", "old": "line two", "new": "line TWO"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "line one\nline TWO\nline three\n"
    );

    let out = revert_tool()
        .call(json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.to_string(),
        "reverted a.txt to its content from before this session's first edit (3 lines)"
    );

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(on_disk, original.as_bytes());
}

#[tokio::test]
async fn two_edits_then_revert_restores_the_original_not_the_intermediate() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    let original = "alpha\nbeta\ngamma\n";
    std::fs::write(&file, original).unwrap();

    edit_tool()
        .call(json!({"path": "a.txt", "old": "beta", "new": "BETA"}), &ctx)
        .await
        .unwrap();
    edit_tool()
        .call(
            json!({"path": "a.txt", "old": "gamma", "new": "GAMMA"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "alpha\nBETA\nGAMMA\n"
    );

    revert_tool()
        .call(json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap();

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(
        on_disk,
        original.as_bytes(),
        "must restore the ORIGINAL, not the intermediate edit"
    );
}

#[tokio::test]
async fn write_file_creating_new_file_then_revert_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    write_tool()
        .call(json!({"path": "new.txt", "content": "brand new"}), &ctx)
        .await
        .unwrap();
    assert!(dir.path().join("new.txt").exists());

    let out = revert_tool()
        .call(json!({"path": "new.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.to_string(),
        "removed new.txt; it did not exist before this session"
    );

    assert!(!dir.path().join("new.txt").exists());
}

#[tokio::test]
async fn revert_with_no_prior_edit_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("untouched.txt");
    std::fs::write(&file, "hello\n").unwrap();

    let err = revert_tool()
        .call(json!({"path": "untouched.txt"}), &ctx)
        .await
        .unwrap_err();

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                "no edits recorded for untouched.txt in this session; nothing to revert"
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }

    // File must be untouched.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n");
}

#[tokio::test]
async fn a_refused_edit_creates_no_backup_so_a_following_revert_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "hello world\n").unwrap();

    // Anchor not found -> refused, no backup should be made.
    let err = edit_tool()
        .call(
            json!({"path": "a.txt", "old": "not present anywhere", "new": "x"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::ExecutionFailed(_)));

    let err = revert_tool()
        .call(json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap_err();
    match err {
        ToolError::ExecutionFailed(msg) => {
            assert_eq!(
                msg,
                "no edits recorded for a.txt in this session; nothing to revert"
            );
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn backup_survives_a_revert_so_reverting_twice_works() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let file = dir.path().join("a.txt");
    let original = "one\ntwo\n";
    std::fs::write(&file, original).unwrap();

    edit_tool()
        .call(json!({"path": "a.txt", "old": "two", "new": "TWO"}), &ctx)
        .await
        .unwrap();

    revert_tool()
        .call(json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes());

    // Edit again, then revert again -- the backup must still be there.
    edit_tool()
        .call(json!({"path": "a.txt", "old": "one", "new": "ONE"}), &ctx)
        .await
        .unwrap();

    let out = revert_tool()
        .call(json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.to_string(),
        "reverted a.txt to its content from before this session's first edit (2 lines)"
    );
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes());
}

#[tokio::test]
async fn parent_dir_escape_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = revert_tool()
        .call(json!({"path": "../x"}), &ctx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Sandbox(_)), "got: {err:?}");
}
