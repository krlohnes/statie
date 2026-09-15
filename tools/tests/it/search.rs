use super::support;

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
    assert!(lines.contains(&"a/one.rs:2- let x = 1;"));
    assert!(lines.contains(&"a/b/two.rs:1- // comment"));
    assert!(lines.contains(&"a/b/two.rs:2: fn foo_bar() {}"));
    // Last line of a/b/two.rs, so no line past EOF.
    assert!(!text.contains("a/b/two.rs:3-"));
    // Different files, so a `--` separator appears between their windows.
    assert!(lines.contains(&"--"));
    assert!(text.contains("to edit here: edit_file"));
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
    assert_eq!(lines[2], "f.txt:2- needle 2");
    assert_eq!(lines[3], "f.txt:3- needle 3");
    assert!(lines[4].starts_with("…(stopped at 1 results"));
    assert!(lines[5].starts_with("to edit here: edit_file"));
    assert_eq!(lines.len(), 6);
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

#[tokio::test]
async fn match_shows_one_line_of_following_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "l1\nl2\nneedle here\nl4\nl5\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("f.txt:3: needle here"), "got: {text}");
    assert!(text.contains("f.txt:4- l4"), "got: {text}");
}

#[tokio::test]
async fn match_on_last_line_has_no_continuation_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\nl4\nneedle here").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("f.txt:5: needle here"), "got: {text}");
    assert!(!text.contains("f.txt:6-"), "got: {text}");
}

#[tokio::test]
async fn trailer_names_clamped_offset_for_read_file() {
    let dir = tempfile::tempdir().unwrap();
    // Match on line 3: offset should clamp to 1 (3 - 5 < 1).
    std::fs::write(dir.path().join("f.txt"), "l1\nl2\nneedle\nl4\nl5\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.contains("to edit here: edit_file {\"path\": \"f.txt\", \"before\": \"needle\", \"new\": \"...\"} — or read_file {\"path\": \"f.txt\", \"offset\": 1, \"limit\": 40}"),
        "got: {text}"
    );

    // Match on line 25: offset should be 25 - 5 = 20.
    let mut content_25 = String::new();
    for i in 1..=24 {
        content_25.push_str(&format!("line{i}\n"));
    }
    content_25.push_str("needle\n");
    std::fs::write(dir.path().join("g.txt"), content_25).unwrap();

    let out = tool()
        .call(json!({"pattern": "needle", "path": "g.txt"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.contains("to edit here: edit_file {\"path\": \"g.txt\", \"before\": \"needle\", \"new\": \"...\"} — or read_file {\"path\": \"g.txt\", \"offset\": 20, \"limit\": 40}"),
        "got: {text}"
    );
}

#[tokio::test]
async fn max_results_one_returns_exactly_one_match_plus_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("f.txt"),
        "needle 1\nfollow 1\nneedle 2\nfollow 2\n",
    )
    .unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle", "max_results": 1}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    // max_results stopped counting at 1 match, so only one line uses the
    // `:` match marker even though "needle 2" (an unmatched line, since
    // scanning stopped) still appears as trailing context text.
    assert_eq!(lines[0], "1 match(es) for /needle/");
    assert_eq!(lines[1], "f.txt:1: needle 1");
    assert_eq!(lines[2], "f.txt:2- follow 1");
    assert_eq!(lines[3], "f.txt:3- needle 2");
    assert_eq!(lines[4], "f.txt:4- follow 2");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("f.txt:") && l.contains(": needle"))
            .count(),
        1,
        "got: {text}"
    );
}

#[tokio::test]
async fn single_match_renders_two_before_six_after_context() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for i in 1..=20 {
        if i == 10 {
            content.push_str("needle here\n");
        } else {
            content.push_str(&format!("line{i}\n"));
        }
    }
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "1 match(es) for /needle/");
    assert_eq!(lines[1], "f.txt:8- line8");
    assert_eq!(lines[2], "f.txt:9- line9");
    assert_eq!(lines[3], "f.txt:10: needle here");
    assert_eq!(lines[4], "f.txt:11- line11");
    assert_eq!(lines[5], "f.txt:12- line12");
    assert_eq!(lines[6], "f.txt:13- line13");
    assert_eq!(lines[7], "f.txt:14- line14");
    assert_eq!(lines[8], "f.txt:15- line15");
    assert_eq!(lines[9], "f.txt:16- line16");
    assert!(!text.contains("f.txt:7-"), "got: {text}");
    assert!(!text.contains("f.txt:17-"), "got: {text}");
    assert!(text.contains("\"before\": \""), "got: {text}");
}

#[tokio::test]
async fn match_on_first_line_has_no_negative_context_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "needle here\nl2\nl3\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "1 match(es) for /needle/");
    assert_eq!(lines[1], "f.txt:1: needle here");
    assert!(!text.contains(":0-"), "got: {text}");
    assert!(!text.contains(":-1"), "got: {text}");
}

#[tokio::test]
async fn match_on_final_line_has_no_lines_past_eof() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "l1\nl2\nneedle here").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "1 match(es) for /needle/");
    assert_eq!(lines[1], "f.txt:1- l1");
    assert_eq!(lines[2], "f.txt:2- l2");
    assert_eq!(lines[3], "f.txt:3: needle here");
    assert!(!text.contains("f.txt:4"), "got: {text}");
}

#[tokio::test]
async fn nearby_matches_merge_into_one_window_without_separator() {
    let dir = tempfile::tempdir().unwrap();
    // Matches on lines 5 and 8 (3 lines apart): windows [3,11] and [6,14]
    // overlap and must merge into a single block with no `--` inside it.
    let mut content = String::new();
    for i in 1..=15 {
        if i == 5 || i == 8 {
            content.push_str("needle\n");
        } else {
            content.push_str(&format!("line{i}\n"));
        }
    }
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("2 match(es) for /needle/"), "got: {text}");
    assert!(text.contains("f.txt:5: needle"), "got: {text}");
    assert!(text.contains("f.txt:8: needle"), "got: {text}");
    assert!(!text.contains("--"), "got: {text}");
}

#[tokio::test]
async fn matches_in_different_files_get_separator() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "needle in a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "needle in b\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert!(text.contains("2 match(es) for /needle/"), "got: {text}");
    assert!(lines.contains(&"--"), "got: {text}");
}

#[tokio::test]
async fn more_than_five_matches_use_compact_rendering() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for i in 1..=6 {
        content.push_str(&format!("needle {i}\nfollow {i}\n"));
    }
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"pattern": "needle"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[0], "6 match(es) for /needle/");
    // Old compact rendering: match line + one `-` continuation line, no
    // 2-line-before context (no line for the "follow" line preceding a
    // match, since compact mode only shows the *following* line).
    assert_eq!(lines[1], "f.txt:1: needle 1");
    assert_eq!(lines[2], "f.txt:2- follow 1");
    assert_eq!(lines[3], "f.txt:3: needle 2");
    assert!(!text.contains("--"), "got: {text}");
    assert!(text.contains("to edit here: edit_file"), "got: {text}");
}

#[tokio::test]
async fn missing_pattern_with_file_path_falls_back_to_read_file_window() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("headless.rs"),
        "line one\nline two\nline three\n",
    )
    .unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"path": "headless.rs"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.starts_with(
            "note: search needs a `pattern`; showed the start of headless.rs instead. To find a spot, send pattern + path."
        ),
        "got: {text}"
    );
    assert!(
        text.contains("headless.rs (3 lines, showing 1-3)"),
        "got: {text}"
    );
    assert!(text.contains("    1 | line one"), "got: {text}");
    assert!(text.contains("    2 | line two"), "got: {text}");
    assert!(text.contains("    3 | line three"), "got: {text}");
}

#[tokio::test]
async fn empty_pattern_with_file_path_falls_back_to_read_file_window() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"path": "f.txt", "pattern": ""}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(
        text.starts_with("note: search needs a `pattern`; showed the start of f.txt instead."),
        "got: {text}"
    );
    assert!(text.contains("1 | hello"), "got: {text}");
}

#[tokio::test]
async fn missing_pattern_with_dir_path_reports_what_is_needed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool().call(json!({"path": "sub"}), &ctx).await.unwrap_err();

    match &err {
        ToolError::InvalidArgs(msg) => assert_eq!(
            msg,
            "search needs `pattern` (a regex) and optionally `path`; to see a file, use read_file"
        ),
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn missing_pattern_and_path_reports_what_is_needed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool().call(json!({}), &ctx).await.unwrap_err();

    match &err {
        ToolError::InvalidArgs(msg) => assert_eq!(
            msg,
            "search needs `pattern` (a regex) and optionally `path`; to see a file, use read_file"
        ),
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Unit 40: the description states the tool's forms and output shape up
// front, at most 130 words.
// ---------------------------------------------------------------------

#[test]
fn description_names_every_argument_and_says_regex_and_output_form() {
    let desc = tool().description().to_string();

    for arg in ["pattern", "path"] {
        assert!(desc.contains(arg), "description missing `{arg}`: {desc}");
    }
    assert!(
        desc.contains("regular expression"),
        "description must say pattern is a regular expression: {desc}"
    );
    assert!(
        desc.contains("path:line"),
        "description must describe the output form: {desc}"
    );

    assert!(
        desc.split_whitespace().count() <= 130,
        "description is {} words, over the 130-word cap: {desc}",
        desc.split_whitespace().count()
    );
}
