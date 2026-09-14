//! Shared helpers used by more than one tool.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use statie_core::{SandboxRoot, ToolError};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Normalizes string-typed tool arguments before deserialization: for each
/// field name in `fields` that is present in the top-level `args` object and
/// holds a JSON array of strings, replaces it with a single string joining
/// the elements with `"\n"` (so a one-element array becomes that element,
/// unchanged). This exists because schema-constrained models occasionally
/// wrap a single-string anchor/argument in a one-element array; the model
/// clearly means the string.
///
/// A field that is not present, is not an array, or is an array containing
/// any non-string element is left completely untouched — in the
/// non-string-element case specifically so the real serde "invalid type"
/// error still surfaces rather than being silently papered over.
///
/// Like the existing `#[serde(alias = ...)]` argument aliases, this
/// normalization is silent: it does not add a note to the tool output.
pub(crate) fn join_string_array_args(args: &mut Value, fields: &[&str]) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    for &field in fields {
        let Some(value) = obj.get_mut(field) else {
            continue;
        };
        let Some(arr) = value.as_array() else {
            continue;
        };
        let mut joined = String::new();
        let mut all_strings = true;
        for (i, elem) in arr.iter().enumerate() {
            match elem.as_str() {
                Some(s) => {
                    if i > 0 {
                        joined.push('\n');
                    }
                    joined.push_str(s);
                }
                None => {
                    all_strings = false;
                    break;
                }
            }
        }
        if all_strings {
            *value = Value::String(joined);
        }
    }
}

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

/// Suffix appended to a backup path to mark that, before this session's
/// first edit, `<rel_path>` did not exist. An empty file at
/// `<root>/.statie/orig/<rel_path>.absent` records this; `revert_file` sees
/// the marker and deletes the (session-created) file rather than trying to
/// restore missing bytes.
const ABSENT_MARKER_SUFFIX: &str = ".absent";

/// The path, under `<root>/.statie/orig/`, that holds the pre-edit backup of
/// `rel_path` (its content, if it existed) or, with `ABSENT_MARKER_SUFFIX`
/// appended, the empty marker recording that it did not exist yet.
pub(crate) fn backup_content_path(root: &SandboxRoot, rel_path: &str) -> std::path::PathBuf {
    root.path().join(".statie").join("orig").join(rel_path)
}

/// The absent-marker sibling of `backup_content_path`'s result: its
/// existence means `rel_path` did not exist before this session's first
/// edit of it (see `ABSENT_MARKER_SUFFIX`).
pub(crate) fn absent_marker_path(root: &SandboxRoot, rel_path: &str) -> std::path::PathBuf {
    path_with_appended_suffix(&backup_content_path(root, rel_path), ABSENT_MARKER_SUFFIX)
}

/// Records a pre-edit backup of `rel_path` under `<root>/.statie/orig/`, but
/// only the first time this is called for a given path: if a backup (or its
/// absent-marker) already exists, this is a no-op, so the backup always
/// holds the content from before the session's *first* modification of that
/// path, never a later intermediate state.
///
/// If `rel_path` currently exists, its bytes are copied verbatim to
/// `<root>/.statie/orig/<rel_path>`. If it does not exist yet (this call is
/// about to `write_file` it into existence), an empty marker file is
/// written instead at `<root>/.statie/orig/<rel_path>.absent` — see
/// `ABSENT_MARKER_SUFFIX`.
///
/// Callers (`write_file`, `edit_file`) invoke this right before their
/// `write_atomic`, only once the edit has otherwise been validated and is
/// about to succeed: a refused edit must never create a backup, since the
/// backup is meant to capture the state a *successful* first edit started
/// from.
pub(crate) fn backup_before_edit(root: &SandboxRoot, rel_path: &str) -> Result<(), ToolError> {
    let backup_path = backup_content_path(root, rel_path);
    let marker_path = absent_marker_path(root, rel_path);

    if backup_path.exists() || marker_path.exists() {
        return Ok(());
    }

    let Some(parent) = backup_path.parent() else {
        return Err(ToolError::ExecutionFailed(
            "backup path has no parent directory".to_string(),
        ));
    };
    std::fs::create_dir_all(parent).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

    let source = root.path().join(rel_path);
    if source.is_file() {
        let bytes =
            std::fs::read(&source).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        write_atomic(&backup_path, &bytes)?;
    } else {
        write_atomic(&marker_path, &[])?;
    }
    Ok(())
}

/// Appends `suffix` to `path`'s file name (not its extension), returning the
/// resulting sibling path.
fn path_with_appended_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
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

/// If `cmd` contains an in-place editor invocation (`sed -i ...` or
/// `perl -i`/`-pi`/`-ni ...`), returns the offending tool label (`"sed -i"`
/// or `"perl -i"`) so the caller can refuse the command. A small model that
/// edits files with `sed -i '174,182d' file` — deleting a line range it
/// remembered from an earlier read — routinely breaks the file when the
/// content has since shifted; `edit_file`'s old/new (or after/before + new)
/// anchoring is safe against that, so in-place stream editing is refused
/// entirely rather than risked.
///
/// Detection is a heuristic over whitespace/`|`/`;`/`&`-separated tokens,
/// not a full shell parser: it looks for a `sed` or `perl` token anywhere in
/// the command, followed later in the same pipeline segment by a flag token
/// that indicates in-place editing (`-i`, `-i''`, `-i.bak`, or, for `perl`,
/// also `-pi...`/`-ni...`). Plain `sed`/`perl` without such a flag (e.g.
/// `sed -n '1p' f`, `perl -e '...'`) is left alone, as is any other command
/// (`grep -i`, `cat`, ...).
fn detect_in_place_editor(cmd: &str) -> Option<&'static str> {
    // Split into pipeline/command segments on common shell separators so a
    // `-i` flag belonging to one command in a chain (`cmd; sed -i ...`)
    // isn't attributed to a different one.
    for segment in cmd.split(['|', ';', '\n']).flat_map(|s| s.split("&&")) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();

        for (idx, tok) in tokens.iter().enumerate() {
            let is_sed = *tok == "sed" || tok.ends_with("/sed");
            let is_perl = *tok == "perl" || tok.ends_with("/perl");

            if !is_sed && !is_perl {
                continue;
            }

            for later in &tokens[idx + 1..] {
                // Strip a single leading `&` in case `&&` wasn't preceded
                // by whitespace (e.g. `cmd&&sed -i ...` isn't split above
                // since `split_whitespace` already separated on the space
                // before `sed`, but be defensive for a bare `&` split too).
                let later = later.trim_start_matches('&');
                if is_sed && later.starts_with("-i") {
                    return Some("sed -i");
                }
                if is_perl
                    && (later.starts_with("-i")
                        || later.starts_with("-pi")
                        || later.starts_with("-ni"))
                {
                    return Some("perl -i");
                }
            }
        }
    }
    None
}

/// Run `cmd` via a shell (`sh -c` on unix, `cmd /C` on windows) inside
/// `cwd`, racing completion against cancellation and a timeout. Returns the
/// formatted `"exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"`
/// text. A non-zero exit is not an error.
///
/// Before doing anything else, refuses commands that look like an in-place
/// editor invocation (`sed -i` / `perl -i`/`-pi`/`-ni`) — see
/// `detect_in_place_editor`.
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
    if let Some(tool) = detect_in_place_editor(cmd) {
        return Err(ToolError::ExecutionFailed(format!(
            "refused: `{tool}` edits a file in place from remembered line numbers. Use edit_file (old/new, after/before + new, or lines + new) so the change is checked against the file's current text."
        )));
    }

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

    #[test]
    fn detects_sed_i_deleting_a_line_range() {
        assert_eq!(
            detect_in_place_editor("sed -i '174,182d' file"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("sed -i '181,183d' file"),
            Some("sed -i")
        );
    }

    #[test]
    fn detects_sed_i_variants() {
        assert_eq!(
            detect_in_place_editor("sed -i 's/a/b/' f.txt"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("sed -i'' -e 's/a/b/' f.txt"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("sed -i.bak 's/a/b/' f.txt"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("sed -i '' 's/a/b/' f.txt"),
            Some("sed -i")
        );
        // other flags before -i are fine
        assert_eq!(
            detect_in_place_editor("sed -E -i 's/a/b/' f.txt"),
            Some("sed -i")
        );
    }

    #[test]
    fn detects_sed_i_inside_pipelines_and_chains() {
        assert_eq!(
            detect_in_place_editor("echo hi && sed -i 's/a/b/' f.txt"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("true; sed -i 's/a/b/' f.txt"),
            Some("sed -i")
        );
        assert_eq!(
            detect_in_place_editor("cat f.txt | sed -i 's/a/b/' f.txt"),
            Some("sed -i")
        );
    }

    #[test]
    fn detects_perl_i_variants() {
        assert_eq!(
            detect_in_place_editor("perl -i -pe 's/a/b/' f.txt"),
            Some("perl -i")
        );
        assert_eq!(
            detect_in_place_editor("perl -i.bak -pe 's/a/b/' f.txt"),
            Some("perl -i")
        );
        assert_eq!(
            detect_in_place_editor("perl -pi -e 's/a/b/' f.txt"),
            Some("perl -i")
        );
        assert_eq!(
            detect_in_place_editor("perl -ni -e 'print' f.txt"),
            Some("perl -i")
        );
    }

    #[test]
    fn allows_sed_without_i_flag() {
        assert_eq!(detect_in_place_editor("sed -n '1,5p' f"), None);
        assert_eq!(detect_in_place_editor("sed -n '1p' f.txt"), None);
    }

    #[test]
    fn allows_unrelated_commands() {
        assert_eq!(detect_in_place_editor("grep -i x f"), None);
        assert_eq!(detect_in_place_editor("cat f.txt"), None);
        assert_eq!(detect_in_place_editor("perl -e 'print 1'"), None);
        assert_eq!(detect_in_place_editor("echo 'sed -i is dangerous'"), None);
    }
}
