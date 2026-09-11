use super::support;

use std::time::Duration;

use serde_json::json;
use statie_core::{Tool, ToolCtx, ToolError};
use statie_tools::{Shell, ToolConfig, default_tools};

fn tool() -> Shell {
    Shell {
        default_timeout_secs: 120,
        max_output_bytes: 200_000,
    }
}

#[tokio::test]
async fn captures_exit_code_stdout_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"cmd": "echo hi; echo err 1>&2; exit 3"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("exit: 3"), "got: {text}");
    assert!(text.contains("hi"), "got: {text}");
    assert!(text.contains("err"), "got: {text}");
}

#[tokio::test]
async fn runs_in_sandbox_root() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool().call(json!({"cmd": "pwd"}), &ctx).await.unwrap();
    let text = out.to_string();

    let canonical = ctx.root.path().to_string_lossy().to_string();
    assert!(
        text.contains(&canonical),
        "got: {text}, expected root: {canonical}"
    );
}

#[tokio::test]
async fn cancel_kills_and_errors_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());
    let cancel = ctx.cancel.clone();

    let ctx_for_task = ToolCtx {
        root: statie_core::SandboxRoot::new(dir.path()).unwrap(),
        log: ctx.log.clone(),
        cancel: ctx.cancel.clone(),
    };

    let handle =
        tokio::spawn(async move { tool().call(json!({"cmd": "sleep 10"}), &ctx_for_task).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("did not finish within 1s")
        .expect("task panicked");

    let err = result.unwrap_err();
    assert!(
        matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("cancelled")),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn timeout_errors_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool().call(json!({"cmd": "sleep 10", "timeout_secs": 1}), &ctx),
    )
    .await
    .expect("did not finish within 3s");

    let err = result.unwrap_err();
    assert!(
        matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("timed out")),
        "got: {err:?}"
    );
}

#[cfg(unix)]
fn pgrep_marker_count(marker: &str) -> usize {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(marker)
        .output();
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
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg(marker)
        .status();
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_backgrounded_children() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    // Per-process marker so an overlapping `cargo test` run (another test
    // binary, a parallel CI job) can't pkill this one's children or be
    // counted by its pgrep.
    let marker = format!("statie-test-marker-timeout-{}", std::process::id());
    let marker = marker.as_str();
    // Clean up defensively in case a previous failed run left one behind.
    pkill_marker(marker);

    let cmd = format!("sh -c 'exec -a {marker} sleep 30' & sh -c 'exec -a {marker} sleep 30'");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tool().call(json!({"cmd": cmd, "timeout_secs": 1}), &ctx),
    )
    .await
    .expect("did not finish within 2s");

    let err = result.unwrap_err();
    assert!(
        matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("timed out")),
        "got: {err:?}"
    );

    // Give the OS a brief moment to finish tearing down, then verify no
    // marked process survived.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let remaining = pgrep_marker_count(marker);
    pkill_marker(marker); // defensive cleanup regardless of assertion outcome
    assert_eq!(remaining, 0, "expected no lingering {marker} processes");
}

#[cfg(unix)]
#[tokio::test]
async fn exited_shell_with_lingering_background_child_returns_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    // Per-process marker so an overlapping `cargo test` run (another test
    // binary, a parallel CI job) can't pkill this one's children or be
    // counted by its pgrep.
    let marker = format!("statie-test-marker-lingering-{}", std::process::id());
    let marker = marker.as_str();
    pkill_marker(marker);

    let cmd = format!("sh -c 'exec -a {marker} sleep 30' & echo started");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tool().call(json!({"cmd": cmd, "timeout_secs": 10}), &ctx),
    )
    .await
    .expect("did not finish within 2s");

    let text = result.unwrap().to_string();
    assert!(text.contains("started"), "got: {text}");
    assert!(text.contains("[harness]"), "got: {text}");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let remaining = pgrep_marker_count(marker);
    pkill_marker(marker);
    assert_eq!(remaining, 0, "expected no lingering {marker} processes");
}

#[test]
fn allow_shell_false_excludes_shell_and_test() {
    let cfg = ToolConfig {
        allow_shell: false,
        ..ToolConfig::default()
    };
    let tools = default_tools(&cfg);
    assert!(tools.get("shell").is_none());
    assert!(tools.get("test").is_none());
}

// ---------------------------------------------------------------------
// Fix 3: `command` is accepted as an alias for `cmd`, but the schema shown
// to the model still lists only the canonical `cmd` field (schemars
// doesn't know about serde aliases).
// ---------------------------------------------------------------------

#[tokio::test]
async fn command_alias_is_accepted_for_cmd() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let out = tool()
        .call(json!({"command": "echo hi"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("hi"), "got: {text}");
}

// ---------------------------------------------------------------------
// Refuse in-place editor invocations (`sed -i`, `perl -i`) that edit a
// file from remembered line numbers instead of anchoring on its current
// text via `edit_file`.
// ---------------------------------------------------------------------

#[tokio::test]
async fn sed_i_is_refused_and_leaves_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let file_path = dir.path().join("f.txt");
    let original = "line1\nline2\nline3\n";
    std::fs::write(&file_path, original).unwrap();

    let err = tool()
        .call(json!({"cmd": "sed -i 's/a/b/' f.txt"}), &ctx)
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            ToolError::ExecutionFailed(ref msg)
                if msg == "refused: `sed -i` edits a file in place from remembered line numbers. Use edit_file (old/new, or after/before + new) so the change is anchored on the file's current text."
        ),
        "got: {err:?}"
    );

    let after = std::fs::read(&file_path).unwrap();
    assert_eq!(after, original.as_bytes(), "file must be byte-identical");
}

#[tokio::test]
async fn sed_n_without_i_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = support::ctx_for(dir.path());

    let file_path = dir.path().join("f.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let out = tool()
        .call(json!({"cmd": "sed -n '1p' f.txt"}), &ctx)
        .await
        .unwrap();
    let text = out.to_string();

    assert!(text.contains("line1"), "got: {text}");
}

#[test]
fn schema_lists_cmd_not_command() {
    let schema = tool().args_schema();
    let value = schema.as_value();
    let properties = value
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("schema should have properties");

    assert!(
        properties.contains_key("cmd"),
        "schema properties should list `cmd`: {properties:?}"
    );
    assert!(
        !properties.contains_key("command"),
        "schema properties should not list the `command` alias: {properties:?}"
    );
}
