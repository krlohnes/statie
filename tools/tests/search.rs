mod support;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::Search;

fn tool() -> Search {
    Search {
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn matches_across_nested_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("a/one.rs"), "fn foo() {}\nlet x = 1;\n").unwrap();
    std::fs::write(
        dir.path().join("a/b/two.rs"),
        "// comment\nfn foo_bar() {}\n",
    )
    .unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"pattern": "foo"}), &ctx).await.unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "2 match(es) for /foo/");
    assert!(lines.contains(&"a/one.rs:1: fn foo() {}"));
    assert!(lines.contains(&"a/b/two.rs:2: fn foo_bar() {}"));
    assert_eq!(lines.len(), 3);
}

#[tokio::test]
async fn glob_filters_by_file_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.rs"), "needle here\n").unwrap();
    std::fs::write(dir.path().join("skip.txt"), "needle here too\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle", "glob": "*.rs"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1 match(es)"));
    assert!(text.contains("keep.rs:1:"));
    assert!(!text.contains("skip.txt"));
}

#[tokio::test]
async fn skips_git_and_target_dirs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/config"), "needle\n").unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/out"), "needle\n").unwrap();
    std::fs::write(dir.path().join("real.txt"), "needle\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1 match(es)"));
    assert!(text.contains("real.txt:1:"));
    assert!(!text.contains(".git"));
    assert!(!text.contains("target/out"));
}

#[tokio::test]
async fn skips_venv_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".venv/lib")).unwrap();
    std::fs::write(dir.path().join(".venv/lib/site.py"), "needle\n").unwrap();
    std::fs::write(dir.path().join("real.txt"), "needle\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1 match(es)"));
    assert!(text.contains("real.txt:1:"));
    assert!(!text.contains(".venv"), "output was:\n{text}");
}

#[tokio::test]
async fn case_insensitive_flag_controls_matching() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "Hello World\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out_sensitive = tool()
        .call(json!({"pattern": "hello world"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out_sensitive.to_string(), "no matches for /hello world/");

    let out_insensitive = tool()
        .call(
            json!({"pattern": "hello world", "case_insensitive": true}),
            &ctx,
        )
        .await
        .unwrap();
    let text = out_insensitive.to_string();
    assert!(text.contains("1 match(es)"));
    assert!(text.contains("f.txt:1: Hello World"));
}

#[tokio::test]
async fn max_results_caps_and_reports_trailer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "needle 1\nneedle 2\nneedle 3\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle", "max_results": 1}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "1 match(es) for /needle/");
    assert_eq!(lines[1], "f.txt:1: needle 1");
    assert_eq!(lines.len(), 3);
    assert!(lines[2].starts_with("…(stopped at 1 results"));
}

#[tokio::test]
async fn invalid_regex_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool()
        .call(json!({"pattern": "("}), &ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn no_matches_reports_exact_message() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "nothing to see\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"pattern": "zzz"}), &ctx).await.unwrap();
    assert_eq!(out.to_string(), "no matches for /zzz/");
}

#[tokio::test]
async fn path_pointing_at_single_file_searches_only_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "needle in a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "needle in b\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle", "path": "a.txt"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1 match(es)"));
    assert!(text.contains("a.txt:1: needle in a"));
    assert!(!text.contains("b.txt"));
}

#[tokio::test]
async fn parent_dir_path_is_sandbox_error() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool()
        .call(json!({"pattern": "needle", "path": "../"}), &ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Sandbox(_)));
}

#[tokio::test]
async fn non_utf8_file_is_skipped_without_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend_from_slice(b"needle\n");
    std::fs::write(dir.path().join("bin.dat"), bytes).unwrap();
    std::fs::write(dir.path().join("ok.txt"), "needle here\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("1 match(es)"));
    assert!(text.contains("ok.txt:1:"));
    assert!(!text.contains("bin.dat"));
}

#[tokio::test]
async fn subdirectory_search_reports_paths_relative_to_sandbox_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
    std::fs::write(dir.path().join("src/inner/lib.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(dir.path().join("other.rs"), "fn needle() {}\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle", "path": "src"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    // The reported path must be usable directly as a `read_file` path.
    assert!(
        text.contains("src/inner/lib.rs:1: fn needle() {}"),
        "got: {text}"
    );
    assert!(!text.contains("other.rs"), "got: {text}");
}
