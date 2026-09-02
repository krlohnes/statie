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

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse just the JSON portion (everything before the trailing "# source:" line).
    let json_part = stdout.rsplit_once("# source:").map(|(j, _)| j).unwrap_or(&stdout);
    let value: serde_json::Value = serde_json::from_str(json_part.trim()).expect("stdout should contain valid JSON");

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
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes()).unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "run",
            "--headless",
            "--memory",
            "--skill",
            "toy",
            "--toy-target",
            "2",
            "--message",
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

#[test]
fn config_init_creates_default_and_refuses_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(bin())
        .current_dir(dir.path())
        .args(["config", "init"])
        .output()
        .expect("failed to run statie");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

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
    assert!(!output2.status.success(), "expected non-zero exit without --force");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output2.stdout),
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(!combined.is_empty(), "expected an error message");
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

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("schema_constrained: true"), "stdout: {stdout}");
}
