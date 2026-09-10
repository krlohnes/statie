mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use statie_tools::{apply_to_sandbox, candidate, diff, remove, run_in, snapshot};

/// Returns `None` (and prints a note) if `git` is not on PATH, so these
/// tests skip cleanly in an environment without it.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

fn read(dir: &std::path::Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

// ---------------------------------------------------------------------
// (a) snapshot captures tracked/modified/untracked, excludes ignored, and
// leaves the real repo state (HEAD, index, working tree) untouched.
// ---------------------------------------------------------------------

#[test]
fn snapshot_captures_working_tree_without_disturbing_real_state() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    write(root, "committed.txt", "original\n");
    write(root, ".gitignore", "ignored.txt\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);
    let head_before = git_in(root, &["rev-parse", "HEAD"]);

    // Modify the tracked file, add an untracked file, add an ignored file.
    write(root, "committed.txt", "modified\n");
    write(root, "untracked.txt", "new stuff\n");
    write(root, "ignored.txt", "should not appear\n");

    let status_before = git_in(root, &["status", "--porcelain"]);

    let snap = snapshot(root).unwrap();
    assert!(!snap.initialized, "repo already existed");

    // The snapshot tree has the modified content and the untracked file,
    // but not the ignored one.
    let ls = git_in(root, &["ls-tree", "-r", "--name-only", snap.sha.as_str()]);
    let names: Vec<&str> = ls.lines().collect();
    assert!(names.contains(&"committed.txt"), "names: {names:?}");
    assert!(names.contains(&"untracked.txt"), "names: {names:?}");
    assert!(!names.contains(&"ignored.txt"), "names: {names:?}");

    let committed_blob = git_in(root, &["show", &format!("{}:committed.txt", snap.sha)]);
    assert_eq!(committed_blob, "modified");

    // Real repo state is untouched.
    let status_after = git_in(root, &["status", "--porcelain"]);
    assert_eq!(status_before, status_after);

    let head_after = git_in(root, &["rev-parse", "HEAD"]);
    assert_eq!(head_before, head_after);

    // The snapshot sha is reachable from no branch.
    let branches = git_in(root, &["branch", "--contains", &snap.sha]);
    assert!(branches.is_empty(), "branches: {branches:?}");
}

// ---------------------------------------------------------------------
// (b) snapshot of a non-repo directory initializes one and produces a
// root commit containing the files.
// ---------------------------------------------------------------------

#[test]
fn snapshot_initializes_repo_when_missing() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "a.txt", "hello\n");

    let snap = snapshot(root).unwrap();
    assert!(snap.initialized);

    let ls = git_in(root, &["ls-tree", "-r", "--name-only", snap.sha.as_str()]);
    assert!(ls.lines().any(|l| l == "a.txt"), "ls: {ls:?}");

    // Root commit: no parents.
    let parents = git_in(root, &["log", "--pretty=%P", "-1", &snap.sha]);
    assert!(parents.is_empty(), "parents: {parents:?}");
}

// ---------------------------------------------------------------------
// (c) candidate + run_in: edit a file in the worktree, run a command,
// check exit codes/output, and confirm the real sandbox file is
// untouched.
// ---------------------------------------------------------------------

#[tokio::test]
async fn candidate_edit_and_run_in_leave_sandbox_untouched() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "file.txt", "before\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let snap = snapshot(root).unwrap();
    let cand = candidate(&snap, |wt: &std::path::Path| {
        std::fs::write(wt.join("file.txt"), "after\n").map_err(|e| e.to_string())
    })
    .unwrap();

    assert_eq!(read(&cand.worktree, "file.txt"), "after\n");
    assert_eq!(
        read(root, "file.txt"),
        "before\n",
        "sandbox must be untouched"
    );

    let cat_result = run_in(&cand, "cat file.txt", Duration::from_secs(5), 200_000)
        .await
        .unwrap();
    assert_eq!(cat_result.exit, Some(0));
    assert!(cat_result.stdout.contains("after"), "{cat_result:?}");
    assert!(!cat_result.timed_out);

    let fail_result = run_in(&cand, "exit 3", Duration::from_secs(5), 200_000)
        .await
        .unwrap();
    assert_eq!(fail_result.exit, Some(3));
    assert!(!fail_result.timed_out);

    assert_eq!(
        read(root, "file.txt"),
        "before\n",
        "sandbox still untouched"
    );

    remove(cand).unwrap();
}

// ---------------------------------------------------------------------
// (d) diff + apply_to_sandbox
// ---------------------------------------------------------------------

#[test]
fn diff_and_apply_to_sandbox_updates_working_tree_only() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "file.txt", "before\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let index_before = git_in(root, &["status", "--porcelain"]);

    let snap = snapshot(root).unwrap();
    let cand = candidate(&snap, |wt: &std::path::Path| {
        std::fs::write(wt.join("file.txt"), "after\n").map_err(|e| e.to_string())
    })
    .unwrap();

    let patch = diff(&cand).unwrap();
    assert!(patch.contains("file.txt"), "patch: {patch}");

    apply_to_sandbox(&cand).unwrap();

    assert_eq!(read(root, "file.txt"), "after\n");

    // Index still untouched: git status should show file.txt as modified
    // in the working tree, not staged. `git_in` trims the overall output,
    // which strips the leading space off a lone porcelain line (` M
    // file.txt` -> `M file.txt`), so check the raw (untrimmed) output
    // directly for the unstaged marker rather than relying on `git_in`'s
    // trimmed copy.
    let status_after = git_in(root, &["status", "--porcelain"]);
    assert_ne!(index_before, status_after);
    let raw_status = String::from_utf8(
        Command::new("git")
            .current_dir(root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        raw_status
            .lines()
            .all(|l| l.starts_with(" M") || l.starts_with("??")),
        "status: {raw_status:?} (expected unstaged modification only)"
    );

    remove(cand).unwrap();
}

// ---------------------------------------------------------------------
// (e) timeout: leaves no lingering process.
// ---------------------------------------------------------------------

#[cfg(unix)]
fn pgrep_marker_count(marker: &str) -> usize {
    let out = Command::new("pgrep").arg("-f").arg(marker).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        Err(_) => 0,
    }
}

#[cfg(unix)]
fn pkill_marker(marker: &str) {
    let _ = Command::new("pkill").arg("-f").arg(marker).status();
}

#[cfg(unix)]
#[tokio::test]
async fn run_in_timeout_kills_process_and_reports_promptly() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "file.txt", "x\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let snap = snapshot(root).unwrap();
    let cand = candidate(&snap, |_wt: &std::path::Path| Ok(())).unwrap();

    let marker = format!("statie-worktree-test-marker-{}", std::process::id());
    pkill_marker(&marker);

    let cmd = format!("exec -a {marker} sleep 30");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        run_in(&cand, &cmd, Duration::from_secs(1), 200_000),
    )
    .await
    .expect("did not finish within 2s")
    .unwrap();

    assert!(result.timed_out, "{result:?}");
    assert_eq!(result.exit, None);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let remaining = pgrep_marker_count(&marker);
    pkill_marker(&marker);
    assert_eq!(remaining, 0, "expected no lingering {marker} processes");

    remove(cand).unwrap();
}

// ---------------------------------------------------------------------
// (f) remove cleans up the worktree directory and `git worktree list`.
// ---------------------------------------------------------------------

#[test]
fn remove_cleans_up_worktree() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "file.txt", "x\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let snap = snapshot(root).unwrap();
    let cand = candidate(&snap, |_wt: &std::path::Path| Ok(())).unwrap();
    let worktree_path = cand.worktree.clone();
    assert!(worktree_path.exists());

    remove(cand).unwrap();

    assert!(!worktree_path.exists());
    let list = git_in(root, &["worktree", "list"]);
    assert!(
        !list.contains(worktree_path.to_string_lossy().as_ref()),
        "list: {list}"
    );
}

// ---------------------------------------------------------------------
// (g) two candidates from one snapshot get distinct directories.
// ---------------------------------------------------------------------

#[test]
fn two_candidates_from_one_snapshot_get_distinct_directories() {
    require_git!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "file.txt", "x\n");
    git_in(root, &["add", "-A"]);
    git_in(root, &["commit", "-m", "initial"]);

    let snap = snapshot(root).unwrap();
    let cand_a = candidate(&snap, |_wt: &std::path::Path| Ok(())).unwrap();
    let cand_b = candidate(&snap, |_wt: &std::path::Path| Ok(())).unwrap();

    assert_ne!(cand_a.worktree, cand_b.worktree);
    let paths: Vec<PathBuf> = vec![cand_a.worktree.clone(), cand_b.worktree.clone()];
    assert!(paths.iter().all(|p| p.exists()));

    remove(cand_a).unwrap();
    remove(cand_b).unwrap();
}
