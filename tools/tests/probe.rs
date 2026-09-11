mod support;

use std::process::Command;

use serde_json::json;
use statie_core::{Tool, ToolError};
use statie_tools::Probe;

/// Returns `None` (and prints a note) if `git` is not on PATH, so these
/// tests skip cleanly in an environment without it -- same pattern as
/// `tools/tests/worktree.rs`.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_git_and_cargo {
    () => {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return;
        }
        if !cargo_available() {
            eprintln!("skipping: `cargo` not found on PATH");
            return;
        }
    };
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return;
        }
    };
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &std::path::Path) {
    git_in(dir, &["init"]);
    git_in(dir, &["config", "user.email", "test@example.com"]);
    git_in(dir, &["config", "user.name", "Test"]);
}

fn write(dir: &std::path::Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn tool() -> Probe {
    Probe {
        default_timeout_secs: 120,
        max_output_bytes: 200_000,
    }
}

/// A tiny Rust fixture crate: `probe_fix` with `pub fn tax(cents: u64) ->
/// u64 { cents * 8 / 100 }`, git-initialized with one commit, and
/// `CARGO_TARGET_DIR` pointed at a scratch dir under the same tempdir so
/// the fixture's cargo run never touches this workspace's own `target/`.
struct RustFixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl RustFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        init_repo(&root);
        write(
            &root,
            "Cargo.toml",
            "[package]\nname = \"probe_fix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [workspace]\n",
        );
        write(
            &root,
            "src/lib.rs",
            "pub fn tax(cents: u64) -> u64 { cents * 8 / 100 }\n",
        );
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-m", "initial"]);
        // `worktree::run_in` defaults `CARGO_TARGET_DIR` to
        // `<root>/.statie/target` (under this fixture's own tempdir), so
        // the fixture's cargo run never touches this workspace's `target/`.
        RustFixture { _dir: dir, root }
    }
}

// ---------------------------------------------------------------------
// Rust: a probe that prints a computed value returns it, exit 0, and
// leaves no trace in the sandbox or a lingering worktree.
// ---------------------------------------------------------------------

#[tokio::test]
async fn rust_probe_prints_value_and_leaves_sandbox_clean() {
    require_git_and_cargo!();
    let fixture = RustFixture::new();
    let ctx = support::ctx_for(&fixture.root);

    let status_before = git_in(&fixture.root, &["status", "--porcelain"]);

    let out = tool()
        .call(
            json!({"code": "println!(\"{}\", tax(4600));", "lang": "rust"}),
            &ctx,
        )
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("368"), "got: {text}");
    assert!(text.contains("exit 0"), "got: {text}");

    assert!(
        !fixture
            .root
            .join("tests")
            .join("__statie_probe.rs")
            .exists(),
        "probe test file must not be left in the sandbox"
    );
    assert!(
        !fixture.root.join(".statie").join("worktrees").exists()
            || std::fs::read_dir(fixture.root.join(".statie").join("worktrees"))
                .unwrap()
                .next()
                .is_none(),
        "no worktree directory should remain"
    );

    let status_after = git_in(&fixture.root, &["status", "--porcelain"]);
    assert_eq!(
        status_before, status_after,
        "sandbox git status must be unchanged"
    );
}

// ---------------------------------------------------------------------
// Rust: a probe with a compile error returns non-zero exit and the
// `error[` text verbatim, and still cleans up fully.
// ---------------------------------------------------------------------

#[tokio::test]
async fn rust_probe_compile_error_is_reported_verbatim_and_cleans_up() {
    require_git_and_cargo!();
    let fixture = RustFixture::new();
    let ctx = support::ctx_for(&fixture.root);

    let status_before = git_in(&fixture.root, &["status", "--porcelain"]);

    let out = tool()
        .call(
            json!({"code": "let x: u64 = \"not a number\";", "lang": "rust"}),
            &ctx,
        )
        .await
        .unwrap();
    let text = out.to_string();

    assert!(!text.contains("exit 0"), "got: {text}");
    assert!(text.contains("error["), "got: {text}");

    assert!(
        !fixture
            .root
            .join("tests")
            .join("__statie_probe.rs")
            .exists(),
        "probe test file must not be left in the sandbox"
    );
    assert!(
        !fixture.root.join(".statie").join("worktrees").exists()
            || std::fs::read_dir(fixture.root.join(".statie").join("worktrees"))
                .unwrap()
                .next()
                .is_none(),
        "no worktree directory should remain"
    );

    let status_after = git_in(&fixture.root, &["status", "--porcelain"]);
    assert_eq!(
        status_before, status_after,
        "sandbox git status must be unchanged"
    );
}

// ---------------------------------------------------------------------
// Python: a probe importing a sandbox module and printing a computed
// value returns it, and leaves no trace.
// ---------------------------------------------------------------------

#[tokio::test]
async fn python_probe_prints_value_and_leaves_sandbox_clean() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "pricing.py", "def total(c): return c * 108 // 100\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let ctx = support::ctx_for(root);

    let out = tool()
        .call(
            json!({
                "code": "from pricing import total\nprint(total(9000))",
                "lang": "python"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("9720"), "got: {text}");
    assert!(text.contains("exit 0"), "got: {text}");

    assert!(
        !root.join("__statie_probe.py").exists(),
        "probe script must not be left in the sandbox"
    );
}

// ---------------------------------------------------------------------
// Language default: rust when Cargo.toml exists, python when only .py
// files exist, error when the sandbox has neither.
// ---------------------------------------------------------------------

#[tokio::test]
async fn lang_defaults_to_python_when_only_py_files_present() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "pricing.py", "def total(c): return c * 108 // 100\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let ctx = support::ctx_for(root);

    let out = tool()
        .call(
            json!({"code": "from pricing import total\nprint(total(100))"}),
            &ctx,
        )
        .await
        .unwrap();
    let text = out.to_string();
    assert!(text.contains("probe (python)"), "got: {text}");
    assert!(text.contains("108"), "got: {text}");
}

#[tokio::test]
async fn lang_defaults_to_rust_when_cargo_toml_present() {
    require_git_and_cargo!();
    let fixture = RustFixture::new();
    let ctx = support::ctx_for(&fixture.root);

    let out = tool()
        .call(json!({"code": "println!(\"{}\", tax(100));"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();
    assert!(text.contains("probe (rust)"), "got: {text}");
}

#[tokio::test]
async fn lang_defaults_error_on_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let err = tool()
        .call(json!({"code": "print(1)"}), &ctx)
        .await
        .expect_err("must error when language cannot be determined");
    assert!(
        matches!(err, ToolError::ExecutionFailed(ref m) if m.contains("cannot tell the language")),
        "got: {err:?}"
    );
}

// ---------------------------------------------------------------------
// A snippet longer than 4000 characters is refused before anything runs.
// ---------------------------------------------------------------------

#[tokio::test]
async fn oversized_snippet_is_refused_before_running() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.py", "x = 1\n");
    let ctx = support::ctx_for(dir.path());

    let long_code = "x = 1\n".repeat(1000); // well over 4000 chars
    assert!(long_code.chars().count() > 5000 || long_code.chars().count() > 4000);

    let err = tool()
        .call(json!({"code": long_code, "lang": "python"}), &ctx)
        .await
        .expect_err("must refuse an over-length snippet");
    assert!(
        matches!(err, ToolError::InvalidArgs(ref m) if m.contains("too long")),
        "got: {err:?}"
    );
}
