//! Shared helpers used by more than one tool.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use statie_core::ToolError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Writes `content` to `path` atomically: writes to a sibling temp file
/// (named after the target with a pid/nanos suffix) then renames it into
/// place.
pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::ExecutionFailed("path has no parent directory".to_string()))?;

    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .ok_or_else(|| ToolError::ExecutionFailed("path has no file name".to_string()))?
        .to_string_lossy();
    let tmp_path = parent.join(format!("{file_name}.tmp-{pid}-{nanos}"));

    std::fs::write(&tmp_path, content).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    Ok(())
}

/// Cap `s` at `max_bytes`, cutting on a char boundary, appending a trailer
/// noting how many bytes the cap was (the *original* byte length).
pub fn cap_output(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str(&format!("\n…[output capped at {max_bytes} bytes]"));
    out
}

/// Run `cmd` via a shell (`sh -c` on unix, `cmd /C` on windows) inside
/// `cwd`, racing completion against cancellation and a timeout. Returns the
/// formatted `"exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"`
/// text. A non-zero exit is not an error.
///
/// On unix, the shell is spawned as the leader of its own process group, so
/// on timeout/cancel (or when a backgrounded grandchild outlives the shell)
/// the *whole group* is killed with `SIGKILL` — not just the `sh` process.
/// Without this, `cmd & sleep N && ...` leaves the backgrounded process
/// (e.g. a dev server) running after the tool call returns.
///
/// On windows there is currently no equivalent job-object grouping, so a
/// backgrounded child can still outlive the timeout; this is a known
/// limitation (see `shell_command` below).
pub async fn run_shell(
    cmd: &str,
    cwd: &Path,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    let mut command = shell_command(cmd);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn shell: {e}")))?;

    let pgid = child_pgid(&child);

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stdout".to_string()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stderr".to_string()))?;

    // Read stdout/stderr on their own tasks, into buffers we can inspect
    // at any time, so they keep making progress (draining the pipes) no
    // matter what the main task is awaiting below. Without this, a large
    // write (bigger than the OS pipe buffer) can block the child on
    // write() while we're separately blocked on `child.wait()`,
    // deadlocking both sides. Sharing the buffers (rather than only
    // getting them back from a joined task) also means that if a
    // backgrounded grandchild keeps a pipe open past our grace period, we
    // can still report whatever was collected before we gave up.
    let stdout_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let stdout_task = tokio::spawn(read_into(stdout_pipe, stdout_buf.clone()));
    let stderr_task = tokio::spawn(read_into(stderr_pipe, stderr_buf.clone()));
    let collect_output = async {
        let _ = tokio::join!(stdout_task, stderr_task);
    };
    tokio::pin!(collect_output);

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            kill_process_group(&mut child, pgid).await;
            Err(ToolError::ExecutionFailed("cancelled".to_string()))
        }
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            kill_process_group(&mut child, pgid).await;
            Err(ToolError::ExecutionFailed(format!("timed out after {timeout_secs}s")))
        }
        status_result = child.wait() => {
            // The shell itself has exited. A backgrounded grandchild may
            // still hold the stdout/stderr pipes open (e.g. `cmd & echo
            // done`), which would otherwise make us wait until the
            // timeout even though `sh` is long gone. The readers are
            // already running concurrently on their own tasks (see
            // above), so give them a short grace period to finish
            // draining whatever is buffered, then stop waiting on them
            // and kill the group — a coding tool must not leave
            // background processes running.
            let status = status_result
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to wait for shell: {e}")))?;
            let code = status.code().unwrap_or(-1);

            let lingering = tokio::time::timeout(Duration::from_millis(500), &mut collect_output)
                .await
                .is_err();

            if lingering {
                kill_process_group(&mut child, pgid).await;
            }

            let out_bytes = stdout_buf.lock().await;
            let err_bytes = stderr_buf.lock().await;
            let out = String::from_utf8_lossy(&out_bytes);
            let err = String::from_utf8_lossy(&err_bytes);
            let mut full = format!("exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
            if lingering {
                full.push_str(
                    "\n[harness] a background process kept the output open; it was killed",
                );
            }
            Ok(full)
        }
    }
}

/// Read `pipe` to EOF, appending each chunk to `buf` as it arrives (rather
/// than only publishing the full result at the end), so a caller can
/// inspect `buf` for partial output even if this task never finishes
/// (e.g. a backgrounded grandchild keeps the pipe open).
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

/// The child's process-group id (unix only; it was spawned as the leader of
/// its own group via `process_group(0)`, so this equals its pid).
#[cfg(unix)]
fn child_pgid(child: &tokio::process::Child) -> Option<libc::pid_t> {
    child.id().map(|id| id as libc::pid_t)
}

#[cfg(windows)]
fn child_pgid(_child: &tokio::process::Child) -> Option<()> {
    None
}

/// Kill the whole process group the shell was spawned into (unix), falling
/// back to killing just the direct child, then reap it.
#[cfg(unix)]
async fn kill_process_group(child: &mut tokio::process::Child, pgid: Option<libc::pid_t>) {
    if let Some(pgid) = pgid {
        // SAFETY: killpg with a valid pgid and SIGKILL has no memory-safety
        // implications; a failure (e.g. ESRCH because the group is already
        // gone) is fine to ignore.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(windows)]
async fn kill_process_group(child: &mut tokio::process::Child, _pgid: Option<()>) {
    // No process-group/job-object equivalent wired up on windows yet, so a
    // backgrounded grandchild (e.g. `start ... & ...`) can outlive this
    // kill. Known limitation: see the module-level doc comment on
    // `run_shell`.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(unix)]
fn shell_command(cmd: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

#[cfg(windows)]
fn shell_command(cmd: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/C").arg(cmd);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_output_leaves_short_strings_untouched() {
        assert_eq!(cap_output("hello".to_string(), 100), "hello");
    }

    #[test]
    fn cap_output_cuts_and_annotates() {
        let s = "x".repeat(200);
        let capped = cap_output(s, 50);
        assert!(capped.len() <= 50 + 40);
        assert!(capped.contains("output capped at 50 bytes"));
    }

    #[test]
    fn cap_output_cuts_on_char_boundary() {
        // multi-byte chars near the boundary
        let s = "a".repeat(49) + "€€€€€"; // € is 3 bytes
        let capped = cap_output(s, 50);
        assert!(capped.is_char_boundary(capped.find('\n').unwrap()));
    }
}
