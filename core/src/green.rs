//! Green checkpoint (statie-spec.md "10. Working set (recent observations)",
//! subsection "Green checkpoint"): when the `test` tool reports
//! `result: pass`, the session snapshots every file the activation has
//! edited so far (from the log's own `edit_file`/`write_file` actions,
//! the same derivation `working_set.rs` uses for its edit windows) and
//! keeps the snapshot in memory. If the activation later parks (or hands
//! off, or finishes) with the most recent `test` result not a pass, the
//! snapshot is written back to the sandbox so a green state is never lost
//! to further edits that broke it.
//!
//! Lives entirely in the session (C2: derived/kept in memory, never
//! persisted to the store) -- see `Session::green_checkpoint` /
//! `Session::last_test_result` in `session.rs`, which own the state this
//! module's functions operate on.

use serde_json::Value;

use crate::log::{LogEntry, LogKind, SessionLog};
use crate::sandbox::SandboxRoot;

/// A large-enough window to see effectively the whole log for one
/// activation; mirrors `working_set.rs`'s `LOG_SCAN_LIMIT` (core cannot
/// import that constant -- it's private to that module).
const LOG_SCAN_LIMIT: usize = 1_000_000;

/// Files larger than this are not checkpointed; the `green_checkpoint` log
/// entry lists them under `skipped` instead.
pub const GREEN_CHECKPOINT_FILE_CAP: usize = 1 << 20;

/// One checkpointed file: its sandbox-relative path, and its content at
/// checkpoint time (`None` means the file did not exist -- restoring it
/// means deleting whatever is there, not writing empty content).
#[derive(Clone, Debug)]
pub struct CheckpointFile {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

/// The green checkpoint: every file the activation had edited as of the
/// step verification last passed, snapshotted from the sandbox at that
/// moment. Replaces any earlier checkpoint (`Session::green_checkpoint` is
/// a single `Option`, not a history).
#[derive(Clone, Debug)]
pub struct GreenCheckpoint {
    /// The harness step at which `test` reported `result: pass`.
    pub step: u64,
    pub files: Vec<CheckpointFile>,
}

/// True when `text` (a `test` tool's `ToolOutputFull`, per
/// `tools/src/test_tool.rs::format_result`) starts with a `result: pass`
/// line (after the leading `command: ...` line). `false` for a `result:
/// fail (...)` line, and also when no `result:` line is present at all
/// (the tool's exit code couldn't be parsed) -- absence of a positive
/// result is not a pass.
pub fn is_pass(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "result: pass")
}

/// Every distinct `args.path` of a successful `edit_file`/`write_file`
/// action in this activation's own log (`agent.is_none()`, same scope
/// `working_set.rs` uses), in first-seen order. "Successful" mirrors
/// `working_set.rs`: no `tool_error` System entry logged at that step.
pub fn edited_paths(log: &dyn SessionLog) -> Vec<String> {
    let actions = own_entries(log, LogKind::Action);
    let system = own_entries(log, LogKind::System);

    let tool_error_steps: std::collections::HashSet<u64> = system
        .iter()
        .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
        .map(|e| e.step)
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for entry in &actions {
        if entry.payload.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let name = entry.payload.get("name").and_then(Value::as_str);
        if !matches!(name, Some("edit_file") | Some("write_file")) {
            continue;
        }
        if tool_error_steps.contains(&entry.step) {
            continue;
        }
        let Some(path) = entry
            .payload
            .get("args")
            .and_then(|a| a.get("path"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if seen.insert(path.to_string()) {
            paths.push(path.to_string());
        }
    }
    paths
}

/// Result of `snapshot`: the checkpoint (files within the cap) plus the
/// paths that were skipped for being over `GREEN_CHECKPOINT_FILE_CAP`.
pub struct Snapshot {
    pub checkpoint: GreenCheckpoint,
    pub skipped: Vec<String>,
}

/// Reads `paths` from the sandbox rooted at `root`, building a
/// `GreenCheckpoint` at `step`. A path that does not currently exist is
/// recorded as absent (`content: None`) so `restore` deletes it. A path
/// whose file is larger than `GREEN_CHECKPOINT_FILE_CAP` is left out of
/// `checkpoint.files` entirely and listed in `skipped` instead. A path
/// that fails to resolve through the sandbox (escapes the root, etc.) is
/// treated the same as over-cap: skipped, not checkpointed -- there is no
/// safe way to snapshot it.
pub fn snapshot(root: &SandboxRoot, step: u64, paths: &[String]) -> Snapshot {
    let mut files = Vec::new();
    let mut skipped = Vec::new();

    for path in paths {
        let resolved = match root.join(path) {
            Ok(p) => p,
            Err(_) => {
                skipped.push(path.clone());
                continue;
            }
        };
        let fs_path = resolved.as_path();
        if !fs_path.exists() {
            files.push(CheckpointFile {
                path: path.clone(),
                content: None,
            });
            continue;
        }
        match std::fs::metadata(fs_path) {
            Ok(meta) if meta.len() as usize > GREEN_CHECKPOINT_FILE_CAP => {
                skipped.push(path.clone());
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                skipped.push(path.clone());
                continue;
            }
        }
        match std::fs::read(fs_path) {
            Ok(bytes) => files.push(CheckpointFile {
                path: path.clone(),
                content: Some(bytes),
            }),
            Err(_) => skipped.push(path.clone()),
        }
    }

    Snapshot {
        checkpoint: GreenCheckpoint { step, files },
        skipped,
    }
}

/// Writes every file in `checkpoint` back to the sandbox rooted at `root`:
/// a file with `Some(bytes)` is written (parent directories created as
/// needed, same as `write_file`/`edit_file`'s own behaviour); a file with
/// `content: None` (absent at checkpoint time) is deleted if it currently
/// exists. Best-effort: a path that fails to resolve or write/delete is
/// left out of the returned list rather than aborting the rest of the
/// restore (the checkpoint is a safety net -- a partial restore is still
/// better than none). Returns the paths actually restored, in
/// `checkpoint.files` order.
pub fn restore(root: &SandboxRoot, checkpoint: &GreenCheckpoint) -> Vec<String> {
    let mut restored = Vec::new();

    for file in &checkpoint.files {
        let Ok(resolved) = root.join(&file.path) else {
            continue;
        };
        let fs_path = resolved.as_path();

        match &file.content {
            Some(bytes) => {
                if let Some(parent) = fs_path.parent()
                    && std::fs::create_dir_all(parent).is_err()
                {
                    continue;
                }
                if write_atomic(fs_path, bytes).is_ok() {
                    restored.push(file.path.clone());
                }
            }
            None => {
                if !fs_path.exists() {
                    // Already absent: nothing to do, but still counts as
                    // "restored" -- the checkpointed state (no file) holds.
                    restored.push(file.path.clone());
                } else if std::fs::remove_file(fs_path).is_ok() {
                    restored.push(file.path.clone());
                }
            }
        }
    }

    restored
}

/// Writes `content` to `path` atomically (temp file + rename), same
/// technique as `tools/src/common.rs::write_atomic` (not reachable from
/// here: it's private to that crate).
fn write_atomic(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp_path = parent.join(format!("{file_name}.tmp-{pid}-{nanos}"));

    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn own_entries(log: &dyn SessionLog, kind: LogKind) -> Vec<LogEntry> {
    log.entries(Some(kind), LOG_SCAN_LIMIT)
        .into_iter()
        .filter(|e| e.agent.is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pass_recognizes_pass_line() {
        assert!(is_pass("command: cargo test\nresult: pass\nexit: 0\n"));
    }

    #[test]
    fn is_pass_rejects_fail_line() {
        assert!(!is_pass(
            "command: cargo test\nresult: fail (exit 1)\n--- failure summary ---\n"
        ));
    }

    #[test]
    fn is_pass_rejects_missing_result_line() {
        assert!(!is_pass("command: cargo test\nexit: 0\nsome output\n"));
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();

        let snap = snapshot(&root, 5, &["a.txt".to_string(), "missing.txt".to_string()]);
        assert!(snap.skipped.is_empty());
        assert_eq!(snap.checkpoint.files.len(), 2);

        std::fs::write(dir.path().join("a.txt"), b"changed").unwrap();
        std::fs::write(dir.path().join("missing.txt"), b"new file").unwrap();

        let restored = restore(&root, &snap.checkpoint);
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
        assert!(!dir.path().join("missing.txt").exists());
    }

    #[test]
    fn snapshot_skips_oversize_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.txt"),
            vec![0u8; GREEN_CHECKPOINT_FILE_CAP + 1],
        )
        .unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();

        let snap = snapshot(&root, 1, &["big.txt".to_string()]);
        assert_eq!(snap.skipped, vec!["big.txt".to_string()]);
        assert!(snap.checkpoint.files.is_empty());
    }
}
