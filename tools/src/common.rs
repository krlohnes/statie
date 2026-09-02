//! Shared helpers used by more than one tool.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use statie_core::ToolError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

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

    let mut child = command
        .spawn()
        .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn shell: {e}")))?;

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stdout".to_string()))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stderr".to_string()))?;

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let read_stdout = stdout_pipe.read_to_end(&mut stdout_buf);
    let read_stderr = stderr_pipe.read_to_end(&mut stderr_buf);
    let collect_output = async {
        let _ = tokio::join!(read_stdout, read_stderr);
    };

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(ToolError::ExecutionFailed("cancelled".to_string()))
        }
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(ToolError::ExecutionFailed(format!("timed out after {timeout_secs}s")))
        }
        status_result = async {
            collect_output.await;
            child.wait().await
        } => {
            let status = status_result
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to wait for shell: {e}")))?;
            let code = status.code().unwrap_or(-1);
            let out = String::from_utf8_lossy(&stdout_buf);
            let err = String::from_utf8_lossy(&stderr_buf);
            Ok(format!("exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"))
        }
    }
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
