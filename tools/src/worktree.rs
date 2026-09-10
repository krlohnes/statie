//! Snapshot-and-verify primitive (BACKLOG.md item 3): try a candidate edit
//! in an isolated copy of the sandbox and run a command there, without
//! disturbing the sandbox's real working tree, index, or HEAD.
//!
//! Git worktrees are the copy: they honour `.gitignore` (no `target/`,
//! `node_modules`), share the object store with the sandbox's repo, and
//! make candidates commits — cheap to create, diff, and throw away.
//!
//! This is a library module, not a `Tool` the model can call directly; it
//! is meant to be driven by a higher-level tool/loop (BACKLOG.md item 4,
//! `try_fixes`).
//!
//! ## Flow
//!
//! 1. [`snapshot`] captures the sandbox's *current* working tree (tracked,
//!    modified, and untracked files, minus ignored ones) as a commit that
//!    sits on no branch, via a temporary index — the real index and HEAD
//!    are never touched.
//! 2. [`candidate`] creates a detached `git worktree` at that snapshot
//!    under `<root>/.statie/worktrees/<short-sha>-<n>/` and applies one
//!    edit inside it.
//! 3. [`run_in`] runs a command inside the candidate worktree, sandboxed
//!    with a timeout and process-group kill, same shape as `shell.rs`.
//! 4. [`diff`] captures what the candidate changed relative to its
//!    snapshot (again as a commit that sits on no branch), and
//!    [`apply_to_sandbox`] applies that diff to the real sandbox working
//!    tree with `git apply` — index and HEAD still untouched.
//! 5. [`remove`] tears the worktree down.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::common::cap_output;

/// A commit of the sandbox's working tree that sits on no branch.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub sha: String,
    pub root: PathBuf,
    /// Whether `snapshot` had to `git init` the root (it was not already a
    /// git repository) before committing.
    pub initialized: bool,
}

/// A detached worktree checked out at a snapshot, with one edit applied.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub sha: String,
    pub worktree: PathBuf,
    /// The snapshot this candidate was created from.
    pub snapshot_sha: String,
    /// The sandbox root the snapshot (and candidate) belong to.
    pub root: PathBuf,
}

/// The result of running a command inside a candidate worktree.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git {cmd:?} failed: {stderr}")]
    Git { cmd: String, stderr: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("edit failed: {0}")]
    Edit(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

impl From<std::io::Error> for WorktreeError {
    fn from(e: std::io::Error) -> Self {
        WorktreeError::Io(e.to_string())
    }
}

/// Runs `git <args>` inside `cwd` (no shell involved), with `envs` applied
/// on top of the inherited environment. Captures stdout as a `String`
/// (lossily) and returns `WorktreeError::Git` on non-zero exit, with the
/// full command line and stderr for debugging.
fn git(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, WorktreeError> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    for (k, v) in envs {
        command.env(k, v);
    }
    let output = command
        .output()
        .map_err(|e| WorktreeError::Io(format!("failed to spawn git {args:?}: {e}")))?;

    if !output.status.success() {
        return Err(WorktreeError::Git {
            cmd: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn current_head(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"], &[])
        .ok()
        .map(|s| s.trim().to_string())
}

/// Path to a scratch git index file, unique per call, under
/// `<root>/.statie/tmp/`. Never the real `.git/index`.
fn tmp_index_path(root: &Path) -> Result<PathBuf, WorktreeError> {
    let dir = root.join(".statie").join("tmp");
    std::fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(dir.join(format!("index-{pid}-{nanos}")))
}

/// Ensures `<root>/.statie/` is git-ignored so snapshot/worktree scratch
/// state under it never shows up as untracked content in `snapshot`
/// itself. Appends `.statie/` to `.git/info/exclude` (never touches the
/// tracked `.gitignore`) if it is not already covered.
fn ensure_statie_dir_excluded(root: &Path) {
    let git_dir = match git(root, &["rev-parse", "--git-dir"], &[]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };
    let git_dir = if Path::new(&git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        root.join(git_dir)
    };
    let exclude_path = git_dir.join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".statie/") {
        return;
    }
    if let Some(parent) = exclude_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(".statie/\n");
    let _ = std::fs::write(&exclude_path, new_content);
}

/// Snapshots the sandbox's CURRENT working tree (tracked, modified, and
/// untracked files, minus ignored ones) as a commit that appears on no
/// branch, without touching the real index or HEAD.
///
/// If `root` is not a git repository, runs `git init` there first (noted
/// in the returned `Snapshot::initialized`) and commits the tree as the
/// root commit (no parent).
pub fn snapshot(root: &Path) -> Result<Snapshot, WorktreeError> {
    let root = root
        .canonicalize()
        .map_err(|e| WorktreeError::Io(format!("failed to canonicalize {root:?}: {e}")))?;

    let initialized = !is_git_repo(&root);
    if initialized {
        git(&root, &["init"], &[])?;
        // A default identity so `commit-tree` doesn't fail on a machine
        // with no global user.name/user.email configured.
        git(&root, &["config", "user.email", "statie@localhost"], &[])?;
        git(&root, &["config", "user.name", "statie"], &[])?;
    }

    ensure_statie_dir_excluded(&root);

    let index_path = tmp_index_path(&root)?;
    // Ensure the scratch index starts empty rather than inheriting
    // whatever the real index currently holds via some ambient state.
    let _ = std::fs::remove_file(&index_path);

    let index_env = index_path.to_string_lossy().into_owned();
    let envs: &[(&str, &str)] = &[("GIT_INDEX_FILE", index_env.as_str())];

    let result = (|| -> Result<Snapshot, WorktreeError> {
        git(&root, &["add", "-A"], envs)?;
        let tree = git(&root, &["write-tree"], envs)?.trim().to_string();

        let head = current_head(&root);
        let commit_args: Vec<&str> = match &head {
            Some(h) => vec![
                "commit-tree",
                tree.as_str(),
                "-p",
                h.as_str(),
                "-m",
                "statie snapshot",
            ],
            None => vec!["commit-tree", tree.as_str(), "-m", "statie snapshot"],
        };
        let sha = git(&root, &commit_args, &[])?.trim().to_string();

        Ok(Snapshot {
            sha,
            root: root.clone(),
            initialized,
        })
    })();

    let _ = std::fs::remove_file(&index_path);
    result
}

/// Creates a detached worktree at `snapshot.sha` under
/// `<root>/.statie/worktrees/<short-sha>-<n>/` and applies `edit` inside
/// it. `edit` receives the worktree's root path so callers can reuse the
/// real edit_file logic against a plain filesystem path.
pub fn candidate<F>(snapshot: &Snapshot, edit: F) -> Result<Candidate, WorktreeError>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    let worktrees_dir = snapshot.root.join(".statie").join("worktrees");
    std::fs::create_dir_all(&worktrees_dir)?;

    let short_sha = &snapshot.sha[..snapshot.sha.len().min(12)];
    let mut n = 0u32;
    let worktree_path = loop {
        let candidate_path = worktrees_dir.join(format!("{short_sha}-{n}"));
        if !candidate_path.exists() {
            break candidate_path;
        }
        n += 1;
    };

    git(
        &snapshot.root,
        &[
            "worktree",
            "add",
            "--detach",
            worktree_path
                .to_str()
                .ok_or_else(|| WorktreeError::Io("worktree path is not valid UTF-8".to_string()))?,
            &snapshot.sha,
        ],
        &[],
    )?;

    edit(&worktree_path).map_err(WorktreeError::Edit)?;

    Ok(Candidate {
        sha: snapshot.sha.clone(),
        worktree: worktree_path,
        snapshot_sha: snapshot.sha.clone(),
        root: snapshot.root.clone(),
    })
}

/// Runs `sh -c cmd` inside the candidate's worktree with the given
/// timeout. `CARGO_TARGET_DIR` is set to `<root>/.statie/target` unless
/// `envs` already sets it. Output is capped at `max_output_bytes`
/// head/tail (via `cap_output`, so effectively head-only with a trailer
/// noting the cap — matching the rest of the tools crate). Process-group
/// kill on timeout, same pattern as `common::run_shell`.
pub async fn run_in(
    candidate: &Candidate,
    cmd: &str,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<RunResult, WorktreeError> {
    run_in_with_env(candidate, cmd, timeout, max_output_bytes, &[]).await
}

/// Like [`run_in`], but with explicit extra environment variables. If
/// `envs` sets `CARGO_TARGET_DIR` itself, that value wins; otherwise it
/// defaults to `<root>/.statie/target` so parallel candidate builds share
/// build artifacts instead of each doing a from-scratch `cargo build`.
pub async fn run_in_with_env(
    candidate: &Candidate,
    cmd: &str,
    timeout: Duration,
    max_output_bytes: usize,
    envs: &[(&str, &str)],
) -> Result<RunResult, WorktreeError> {
    let mut command = shell_command(cmd);
    command
        .current_dir(&candidate.worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let target_dir = candidate.root.join(".statie").join("target");
    if !envs.iter().any(|(k, _)| *k == "CARGO_TARGET_DIR") {
        command.env("CARGO_TARGET_DIR", &target_dir);
    }
    for (k, v) in envs {
        command.env(k, v);
    }

    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| WorktreeError::Io(format!("failed to spawn shell: {e}")))?;

    let pgid = child_pgid(&child);

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| WorktreeError::Io("failed to capture stdout".to_string()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| WorktreeError::Io("failed to capture stderr".to_string()))?;

    let stdout_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let stdout_task = tokio::spawn(read_into(stdout_pipe, stdout_buf.clone()));
    let stderr_task = tokio::spawn(read_into(stderr_pipe, stderr_buf.clone()));
    let collect_output = async {
        let _ = tokio::join!(stdout_task, stderr_task);
    };
    tokio::pin!(collect_output);

    // `run_in` has no external cancellation token in its signature, but we
    // still race against the timeout the same way `run_shell` races
    // against cancel/timeout, so a stuck command can't hang the caller.
    let never_cancel = CancellationToken::new();

    tokio::select! {
        biased;
        _ = never_cancel.cancelled() => unreachable!("never cancelled"),
        _ = tokio::time::sleep(timeout) => {
            kill_process_group(&mut child, pgid).await;
            let out_bytes = stdout_buf.lock().await;
            let err_bytes = stderr_buf.lock().await;
            Ok(RunResult {
                exit: None,
                stdout: cap_output(String::from_utf8_lossy(&out_bytes).into_owned(), max_output_bytes),
                stderr: cap_output(String::from_utf8_lossy(&err_bytes).into_owned(), max_output_bytes),
                timed_out: true,
            })
        }
        status_result = child.wait() => {
            let status = status_result
                .map_err(|e| WorktreeError::Io(format!("failed to wait for shell: {e}")))?;
            let code = status.code();

            let lingering = tokio::time::timeout(Duration::from_millis(500), &mut collect_output)
                .await
                .is_err();
            if lingering {
                kill_process_group(&mut child, pgid).await;
            }

            let out_bytes = stdout_buf.lock().await;
            let err_bytes = stderr_buf.lock().await;
            Ok(RunResult {
                exit: code,
                stdout: cap_output(String::from_utf8_lossy(&out_bytes).into_owned(), max_output_bytes),
                stderr: cap_output(String::from_utf8_lossy(&err_bytes).into_owned(), max_output_bytes),
                timed_out: false,
            })
        }
    }
}

async fn read_into(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    buf: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
) {
    let mut chunk = [0u8; 8192];
    loop {
        let n = match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.lock().await.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(unix)]
fn child_pgid(child: &tokio::process::Child) -> Option<libc::pid_t> {
    child.id().map(|id| id as libc::pid_t)
}

#[cfg(windows)]
fn child_pgid(_child: &tokio::process::Child) -> Option<()> {
    None
}

#[cfg(unix)]
async fn kill_process_group(child: &mut tokio::process::Child, pgid: Option<libc::pid_t>) {
    if let Some(pgid) = pgid {
        // SAFETY: killpg with a valid pgid and SIGKILL has no
        // memory-safety implications; a failure (e.g. ESRCH because the
        // group is already gone) is fine to ignore.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(windows)]
async fn kill_process_group(child: &mut tokio::process::Child, _pgid: Option<()>) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(unix)]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

#[cfg(windows)]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/C").arg(cmd);
    c
}

/// The candidate's diff against its snapshot: commits the worktree's
/// current state (tracked, modified, untracked-minus-ignored) as a child
/// commit of the snapshot, again on no branch, then diffs snapshot..that
/// commit. This captures edits made after `candidate()` (e.g. by
/// `run_in`'s command, or by direct filesystem writes into the worktree),
/// not just the `edit` closure's own change.
pub fn diff(candidate: &Candidate) -> Result<String, WorktreeError> {
    let commit_sha = commit_worktree_state(candidate)?;
    git(
        &candidate.root,
        &["diff", &candidate.snapshot_sha, &commit_sha],
        &[],
    )
}

/// Commits the candidate worktree's current on-disk state as a child of
/// its snapshot, via a temporary index scoped to the worktree, without
/// touching the worktree's own (real, for that worktree) index or HEAD.
fn commit_worktree_state(candidate: &Candidate) -> Result<String, WorktreeError> {
    let index_path = tmp_index_path(&candidate.root)?;
    let _ = std::fs::remove_file(&index_path);
    let index_env = index_path.to_string_lossy().into_owned();
    let envs: &[(&str, &str)] = &[("GIT_INDEX_FILE", index_env.as_str())];

    let result = (|| -> Result<String, WorktreeError> {
        git(&candidate.worktree, &["add", "-A"], envs)?;
        let tree = git(&candidate.worktree, &["write-tree"], envs)?
            .trim()
            .to_string();
        let sha = git(
            &candidate.worktree,
            &[
                "commit-tree",
                tree.as_str(),
                "-p",
                candidate.snapshot_sha.as_str(),
                "-m",
                "statie candidate",
            ],
            &[],
        )?
        .trim()
        .to_string();
        Ok(sha)
    })();

    let _ = std::fs::remove_file(&index_path);
    result
}

/// Applies the candidate's diff (against its snapshot) to the real
/// sandbox working tree with `git apply`, leaving the sandbox's index and
/// HEAD untouched.
pub fn apply_to_sandbox(candidate: &Candidate) -> Result<(), WorktreeError> {
    let patch = diff(candidate)?;
    if patch.trim().is_empty() {
        return Ok(());
    }

    let patch_path = tmp_index_path(&candidate.root)?; // reuse: unique scratch path under .statie/tmp
    std::fs::write(&patch_path, &patch)?;

    let result = git(
        &candidate.root,
        &[
            "apply",
            "--whitespace=nowarn",
            patch_path
                .to_str()
                .ok_or_else(|| WorktreeError::Io("patch path is not valid UTF-8".to_string()))?,
        ],
        &[],
    )
    .map(|_| ());

    let _ = std::fs::remove_file(&patch_path);
    result
}

/// Removes the worktree (`git worktree remove --force`) and prunes stale
/// worktree metadata.
pub fn remove(candidate: Candidate) -> Result<(), WorktreeError> {
    let path_str = candidate
        .worktree
        .to_str()
        .ok_or_else(|| WorktreeError::Io("worktree path is not valid UTF-8".to_string()))?;

    // If the directory is already gone (e.g. removed out of band), still
    // try to prune so `git worktree list` doesn't keep a stale entry.
    if candidate.worktree.exists() {
        git(
            &candidate.root,
            &["worktree", "remove", "--force", path_str],
            &[],
        )?;
    }
    let _ = git(&candidate.root, &["worktree", "prune"], &[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_index_paths_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let a = tmp_index_path(dir.path()).unwrap();
        let b = tmp_index_path(dir.path()).unwrap();
        assert_ne!(a, b);
    }
}
