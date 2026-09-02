use std::fs;

use serde::Serialize;

use statie_augment::{RepoBm25, RepoBm25Options};
use statie_core::{Augmenter, HeuristicTokenizer, SandboxRoot, TokenBudget, Tokenizer};

#[derive(Serialize)]
struct S {
    task: String,
}

fn write_file(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn parser_rs_content() -> String {
    // ~60 lines, mentions `fn parse_tokens` several times, so it spans 2
    // chunks at chunk_lines = 40.
    let mut s = String::new();
    for i in 0..60 {
        if i % 10 == 0 {
            s.push_str("fn parse_tokens() {\n");
        } else {
            s.push_str(&format!("    // line {i} of the parser\n"));
        }
    }
    s
}

#[tokio::test]
async fn repo_bm25_finds_relevant_chunk_excludes_target_and_returns_nonempty_text() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/parser.rs", &parser_rs_content());
    write_file(
        dir.path(),
        "src/main.rs",
        "fn main() {\n    println!(\"hi\");\n}\n",
    );
    write_file(
        dir.path(),
        "target/debug/junk.rs",
        &"parse_tokens ".repeat(50),
    );
    write_file(dir.path(), "README.md", "# demo project\n");

    let root = SandboxRoot::new(dir.path()).unwrap();
    let opts = RepoBm25Options::default();
    let index = RepoBm25::build(&root, opts).unwrap();

    let state = S {
        task: "fix parse_tokens in the parser".to_string(),
    };
    let tok = HeuristicTokenizer;
    let budget = TokenBudget(2000);

    let snippets = index.augment(&state, None, budget, &tok).await;

    assert!(!snippets.is_empty(), "expected at least one snippet");
    assert!(
        snippets[0].source.starts_with("src/parser.rs:"),
        "expected first snippet to come from src/parser.rs, got {}",
        snippets[0].source
    );
    assert!(
        snippets.iter().all(|s| !s.source.starts_with("target/")),
        "no snippet should come from the excluded target/ dir: {:?}",
        snippets.iter().map(|s| &s.source).collect::<Vec<_>>()
    );
    assert!(
        snippets.iter().all(|s| !s.text.is_empty()),
        "every snippet's text should be non-empty"
    );
}

#[tokio::test]
async fn budget_limits_snippet_count_and_token_total() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/parser.rs", &parser_rs_content());
    write_file(dir.path(), "src/main.rs", "fn main() {}\n");

    let root = SandboxRoot::new(dir.path()).unwrap();
    let index = RepoBm25::build(&root, RepoBm25Options::default()).unwrap();

    let state = S {
        task: "fix parse_tokens in the parser".to_string(),
    };
    let tok = HeuristicTokenizer;

    let small = index.augment(&state, None, TokenBudget(30), &tok).await;
    assert!(small.len() <= 1);
    let total: usize = small
        .iter()
        .map(|s| tok.count(&format!("[{}]\n{}", s.source, s.text)))
        .sum();
    assert!(total <= 30, "total tokens {total} should be <= 30");

    let zero = index.augment(&state, None, TokenBudget(0), &tok).await;
    assert!(zero.is_empty());
}

#[tokio::test]
async fn chunking_splits_a_100_line_file_at_chunk_boundaries() {
    let dir = tempfile::tempdir().unwrap();

    // 100 lines; unique markers in each expected chunk region so we can
    // search for them and check which chunk they land in.
    let mut content = String::new();
    for i in 1..=100 {
        match i {
            5 => content.push_str("uniquemarkerone\n"),
            45 => content.push_str("uniquemarkertwo\n"),
            85 => content.push_str("uniquemarkerthree\n"),
            _ => content.push_str(&format!("filler line {i}\n")),
        }
    }
    write_file(dir.path(), "file.rs", &content);

    let root = SandboxRoot::new(dir.path()).unwrap();
    let opts = RepoBm25Options {
        chunk_lines: 40,
        max_snippets: 10,
        ..RepoBm25Options::default()
    };
    let index = RepoBm25::build(&root, opts).unwrap();

    let tok = HeuristicTokenizer;

    let s1 = S {
        task: "uniquemarkerone".to_string(),
    };
    let hits1 = index.augment(&s1, None, TokenBudget(10_000), &tok).await;
    assert!(
        hits1.iter().any(|s| s.source == "file.rs:1-40"),
        "{:?}",
        hits1
    );

    let s2 = S {
        task: "uniquemarkertwo".to_string(),
    };
    let hits2 = index.augment(&s2, None, TokenBudget(10_000), &tok).await;
    assert!(
        hits2.iter().any(|s| s.source == "file.rs:41-80"),
        "{:?}",
        hits2
    );

    let s3 = S {
        task: "uniquemarkerthree".to_string(),
    };
    let hits3 = index.augment(&s3, None, TokenBudget(10_000), &tok).await;
    assert!(
        hits3.iter().any(|s| s.source == "file.rs:81-100"),
        "{:?}",
        hits3
    );
}

fn snippets_text(snippets: &[statie_core::Snippet]) -> String {
    snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn augment_reflects_file_edits_made_after_build() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "calc.py",
        "def subtract(a, b):\n    return a - b\n",
    );

    let root = SandboxRoot::new(dir.path()).unwrap();
    let index = RepoBm25::build(&root, RepoBm25Options::default()).unwrap();

    let tok = HeuristicTokenizer;
    let budget = TokenBudget(2000);

    let state = S {
        task: "subtract".to_string(),
    };
    let snippets = index.augment(&state, None, budget, &tok).await;
    let text = snippets_text(&snippets);
    assert!(
        text.contains("subtract"),
        "expected snippet to contain 'subtract', got: {text}"
    );

    // Overwrite the file with different content (different length, so the
    // fingerprint changes even if the filesystem's mtime resolution is
    // coarse). Also bump mtime explicitly to be robust either way.
    let new_content = "def multiply(a, b):\n    return a * b\n\n# padding to change length\n";
    write_file(dir.path(), "calc.py", new_content);
    let f = std::fs::File::open(dir.path().join("calc.py")).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
        .unwrap();

    let state2 = S {
        task: "multiply".to_string(),
    };
    let snippets2 = index.augment(&state2, None, TokenBudget(2000), &tok).await;
    let text2 = snippets_text(&snippets2);
    assert!(
        text2.contains("multiply"),
        "expected snippet to contain 'multiply', got: {text2}"
    );
    assert!(
        !text2.contains("subtract"),
        "expected snippet to no longer contain 'subtract', got: {text2}"
    );
}

#[tokio::test]
async fn augment_picks_up_new_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "README.md", "# demo\n");

    let root = SandboxRoot::new(dir.path()).unwrap();
    let index = RepoBm25::build(&root, RepoBm25Options::default()).unwrap();

    let tok = HeuristicTokenizer;
    let budget = TokenBudget(2000);
    let state = S {
        task: "distinctivetokenxyz".to_string(),
    };

    let before = index.augment(&state, None, budget, &tok).await;
    assert!(
        !snippets_text(&before).contains("distinctivetokenxyz"),
        "should not find token before file exists"
    );

    write_file(dir.path(), "new_file.py", "distinctivetokenxyz = 42\n");

    let after_create = index.augment(&state, None, TokenBudget(2000), &tok).await;
    assert!(
        snippets_text(&after_create).contains("distinctivetokenxyz"),
        "expected token after creating new_file.py, got: {:?}",
        after_create
    );

    std::fs::remove_file(dir.path().join("new_file.py")).unwrap();

    let after_delete = index.augment(&state, None, TokenBudget(2000), &tok).await;
    assert!(
        !snippets_text(&after_delete).contains("distinctivetokenxyz"),
        "expected token to be gone after deleting new_file.py, got: {:?}",
        after_delete
    );
}

#[tokio::test]
async fn refresh_if_changed_is_false_when_nothing_changed() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/main.rs", "fn main() {}\n");

    let root = SandboxRoot::new(dir.path()).unwrap();
    let index = RepoBm25::build(&root, RepoBm25Options::default()).unwrap();

    let first = index.refresh_if_changed().unwrap();
    let second = index.refresh_if_changed().unwrap();
    assert!(
        !second,
        "expected no change on second call, first result was {first}"
    );
}

#[tokio::test]
async fn default_exclude_dirs_skip_pycache() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/main.rs", "fn main() {}\n");
    write_file(
        dir.path(),
        "__pycache__/module.pyc",
        "distinctivepycachetoken\n",
    );

    let root = SandboxRoot::new(dir.path()).unwrap();
    let index = RepoBm25::build(&root, RepoBm25Options::default()).unwrap();

    let tok = HeuristicTokenizer;
    let budget = TokenBudget(2000);
    let state = S {
        task: "distinctivepycachetoken".to_string(),
    };

    let snippets = index.augment(&state, None, budget, &tok).await;
    assert!(
        !snippets_text(&snippets).contains("distinctivepycachetoken"),
        "expected __pycache__ contents to be excluded, got: {:?}",
        snippets
    );
}
