use super::support;

use serde_json::json;
use statie_core::Tool;
use statie_tools::ListFiles;

fn tool() -> ListFiles {
    ListFiles {
        max_output_bytes: 200_000,
    }
}

fn make_tree(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join("target/junk"), "junk").unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
    std::fs::write(dir.join("README.md"), "# hi").unwrap();
}

#[tokio::test]
async fn lists_exact_tree_skipping_noise_dirs() {
    let dir = tempfile::tempdir().unwrap();
    make_tree(dir.path());
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({}), &ctx).await.unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines, vec!["README.md", "src/", "src/main.rs"]);
}

#[tokio::test]
async fn skips_pycache_dir() {
    let dir = tempfile::tempdir().unwrap();
    make_tree(dir.path());
    std::fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
    std::fs::write(dir.path().join("__pycache__/mod.cpython-311.pyc"), "junk").unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({}), &ctx).await.unwrap();
    let text = out.to_string();

    assert!(!text.contains("__pycache__"), "output was:\n{text}");
}

#[tokio::test]
async fn truncates_with_more_line() {
    let dir = tempfile::tempdir().unwrap();
    make_tree(dir.path());
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"max": 1}), &ctx).await.unwrap();
    let text = out.to_string();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "README.md");
    assert!(lines[1].starts_with("… ") && lines[1].ends_with(" more"));
}
