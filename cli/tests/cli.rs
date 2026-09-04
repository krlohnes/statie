//! End-to-end tests that spawn the `statie` binary itself.

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

#[test]
fn config_show_with_flag_reports_mock_backend_and_default_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, r#"{"model":{"backend":"mock","model":"m"}}"#).unwrap();

    let output = Command::new(bin())
        .args(["--config", cfg_path.to_str().unwrap(), "config", "show"])
        .output()
        .expect("failed to run statie");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse just the JSON portion (everything before the trailing "# source:" line).
    let json_part = stdout
        .rsplit_once("# source:")
        .map(|(j, _)| j)
        .unwrap_or(&stdout);
    let value: serde_json::Value =
        serde_json::from_str(json_part.trim()).expect("stdout should contain valid JSON");

    assert_eq!(value["model"]["backend"], "mock");
    assert_eq!(value["model"]["base_url"], "http://localhost:11434");
}

#[test]
fn run_headless_memory_toy_with_mock_backend_reaches_done() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": [
                {
                    "content": serde_json::json!({
                        "patch": {"counter": 1, "recent_summary": "did step 1"},
                        "action": {"type": "tool", "name": "echo", "args": {"text": "hello 1"}},
                        "ack_inbox": [1],
                    }).to_string()
                },
                {
                    "content": serde_json::json!({
                        "patch": {"counter": 2, "recent_summary": "did step 2"},
                        "action": {"type": "tool", "name": "echo", "args": {"text": "hello 2"}},
                    }).to_string()
                },
                {
                    "content": serde_json::json!({
                        "patch": {"counter": 2},
                        "action": {"type": "done"},
                    }).to_string()
                },
            ]
        }
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--memory",
            "--skill",
            "toy",
            "--toy-target",
            "2",
            "-p",
            "go",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("outcome=done"), "stdout: {stdout}");
    assert!(stdout.contains("steps=3"), "stdout: {stdout}");
}

/// `--patch` with invalid JSON exits with status 2 and a clear message on
/// stderr, before any config/session work happens (no `--config` is even
/// given here).
#[test]
fn patch_with_invalid_json_exits_2_with_a_clear_message() {
    let output = Command::new(bin())
        .args([
            "--memory", "--skill", "toy", "--patch", "not json", "-p", "go",
        ])
        .output()
        .expect("failed to run statie");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--patch") && stderr.contains("invalid JSON"),
        "stderr: {stderr}"
    );
}

/// A valid `--patch` on a `--memory` mock run is applied before the first
/// step (bumping `counter` directly, no model call spent on it) and prints
/// `[patched]` in the headless transcript.
#[test]
fn valid_patch_on_memory_mock_run_prints_patched_and_applies_before_first_step() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": [
                {
                    "content": serde_json::json!({
                        "patch": {},
                        "action": {"type": "done"},
                        "ack_inbox": [1],
                    }).to_string()
                },
            ]
        }
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--memory",
            "--skill",
            "toy",
            "--toy-target",
            "2",
            "--patch",
            r#"{"counter": 2}"#,
            "-p",
            "go",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("[patched]"), "stdout: {stdout}");
    assert!(stdout.contains("counter"), "stdout: {stdout}");
    // The patch alone already reaches target=2, so the single scripted
    // turn only needs to send `done` (no counter-incrementing tool loop),
    // proving the patch was applied before the first step.
    assert!(stdout.contains("outcome=done"), "stdout: {stdout}");
}

#[test]
fn no_subcommand_with_p_runs_headless() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": [
                {
                    "content": serde_json::json!({
                        "patch": {"counter": 1},
                        "action": {"type": "done"},
                        "ack_inbox": [1],
                    }).to_string()
                },
            ]
        }
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "-p",
            "hi",
            "--skill",
            "toy",
            "--memory",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("steps="), "stdout: {stdout}");
}

/// A positional PROMPT combined with `-p` is accepted (not rejected as a
/// conflicting combination) and both are seeded as initial messages: the
/// unit tests in `cli/src/main.rs` (`initial_messages`) cover the exact
/// ordering; this covers that the combination runs successfully
/// end-to-end through the real binary and reaches `outcome=done`, which
/// requires every seeded message's inbox item to have been acked.
#[test]
fn positional_prompt_then_p_seeds_two_messages() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": [
                {
                    "content": serde_json::json!({
                        "patch": {"counter": 2},
                        "action": {"type": "done"},
                        "ack_inbox": [1, 2],
                    }).to_string()
                },
            ]
        }
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "first message",
            "-p",
            "second message",
            "--skill",
            "toy",
            "--toy-target",
            "2",
            "--memory",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("outcome=done"), "stdout: {stdout}");
}

#[test]
fn help_lists_prompt_and_resume() {
    let output = Command::new(bin())
        .args(["--help"])
        .output()
        .expect("failed to run statie");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--prompt"), "stdout: {stdout}");
    assert!(stdout.contains("--resume"), "stdout: {stdout}");
    assert!(!stdout.contains(" run "), "stdout: {stdout}");
}

#[test]
fn run_subcommand_is_gone() {
    let output = Command::new(bin())
        .args(["run", "--headless", "--message", "hi"])
        .output()
        .expect("failed to run statie");

    assert!(
        !output.status.success(),
        "expected `statie run --headless --message hi` (old syntax) to fail to parse"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("error:"),
        "expected a clap parse error on stderr: {stderr}"
    );
}

#[test]
fn config_init_creates_default_and_refuses_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(bin())
        .current_dir(dir.path())
        .args(["config", "init"])
        .output()
        .expect("failed to run statie");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cfg_path = dir.path().join(".statie/config.json");
    assert!(cfg_path.exists());

    let text = std::fs::read_to_string(&cfg_path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).expect("should parse back");
    assert_eq!(value["model"]["base_url"], "http://localhost:11434");

    let output2 = Command::new(bin())
        .current_dir(dir.path())
        .args(["config", "init"])
        .output()
        .expect("failed to run statie");
    assert!(
        !output2.status.success(),
        "expected non-zero exit without --force"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output2.stdout),
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(!combined.is_empty(), "expected an error message");
}

#[test]
fn config_init_writes_self_ignoring_gitignore() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(bin())
        .current_dir(dir.path())
        .args(["config", "init"])
        .output()
        .expect("failed to run statie");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let gitignore_path = dir.path().join(".statie/.gitignore");
    assert!(
        gitignore_path.exists(),
        "expected {} to exist",
        gitignore_path.display()
    );
    let contents = std::fs::read_to_string(&gitignore_path).unwrap();
    assert_eq!(contents, "*\n");
}

#[test]
fn check_against_mock_backend_reports_schema_constrained_true() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, r#"{"model":{"backend":"mock"}}"#).unwrap();

    let output = Command::new(bin())
        .args(["--config", cfg_path.to_str().unwrap(), "check"])
        .output()
        .expect("failed to run statie");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("schema_constrained: true"),
        "stdout: {stdout}"
    );
}
