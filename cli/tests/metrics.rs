//! End-to-end tests for `statie metrics [PATH]`: writes a small JSONL
//! session log by hand and runs the binary against it, checking both the
//! plain-text summary and `--json` output.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

fn entry(step: u64, kind: &str, payload: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "step": step,
        "kind": kind,
        "ts": "2026-09-01T00:00:00Z",
        "payload": payload,
    })
}

/// A small, varied session log: one activation, a rejected step, an edit
/// that errors, an edit that lands, a maintainer run, and a
/// skipped-redundant call -- enough to exercise every summary field and
/// both breakdowns without needing the full scenario `cli/src/metrics.rs`'s
/// own unit tests already cover.
fn write_sample_log(path: &std::path::Path) {
    let lines: Vec<String> = vec![
        entry(
            0,
            "system",
            serde_json::json!({"event": "activate", "skill": "router"}),
        ),
        entry(
            1,
            "action",
            serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": "a.rs"}}),
        ),
        entry(
            2,
            "system",
            serde_json::json!({"event": "rejected", "reason": "bad json"}),
        ),
        entry(
            3,
            "action",
            serde_json::json!({"type": "tool", "name": "edit_file", "args": {"path": "a.rs"}}),
        ),
        entry(
            3,
            "system",
            serde_json::json!({"event": "tool_error", "name": "edit_file", "reason": "no such anchor"}),
        ),
        entry(
            4,
            "system",
            serde_json::json!({"event": "skipped_redundant", "name": "shell", "reason": "same command"}),
        ),
        entry(5, "system", serde_json::json!({"event": "maintainer"})),
        entry(
            6,
            "action",
            serde_json::json!({"type": "tool", "name": "write_file", "args": {"path": "b.rs"}}),
        ),
    ]
    .into_iter()
    .map(|v| v.to_string())
    .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn metrics_text_summary_matches_expected_fields() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("session.jsonl");
    write_sample_log(&log_path);

    let output = Command::new(bin())
        .args(["metrics", log_path.to_str().unwrap()])
        .output()
        .expect("failed to run statie metrics");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();

    let summary_line = lines.next().expect("summary line");
    // steps: distinct Action-entry steps are 1, 3, 6 -> 3.
    assert!(summary_line.contains("steps=3"), "{summary_line}");
    assert!(summary_line.contains("rej=1"), "{summary_line}");
    assert!(summary_line.contains("failures=0"), "{summary_line}");
    assert!(summary_line.contains("tool_err=1"), "{summary_line}");
    assert!(summary_line.contains("skipped=1"), "{summary_line}");
    assert!(summary_line.contains("paged=0"), "{summary_line}");
    assert!(summary_line.contains("continued=0"), "{summary_line}");
    // edits: edit_file at step 3 errored (does not land), write_file at
    // step 6 lands -> edits=1. Committed (Action) steps in order are
    // 1, 3, 6, so the landed edit at step 6 is the 3rd.
    assert!(summary_line.contains("edits=1"), "{summary_line}");
    assert!(summary_line.contains("first_edit=3"), "{summary_line}");
    assert!(summary_line.contains("green=0"), "{summary_line}");
    assert!(summary_line.contains("restored=0"), "{summary_line}");
    assert!(summary_line.contains("maint=1"), "{summary_line}");
    assert!(summary_line.contains("maint_err=0"), "{summary_line}");
    assert!(
        summary_line.contains("activations=router"),
        "{summary_line}"
    );
    // The log ends with an Action entry (write_file, step 6); the last
    // *System* entry before it is the maintainer run at step 5.
    assert!(summary_line.contains("last=maintainer"), "{summary_line}");

    let rest: Vec<&str> = lines.collect();
    assert!(rest.contains(&"rejections by reason (first 60 chars):"));
    assert!(rest.iter().any(|l| l.trim() == "1  bad json"));
    assert!(rest.contains(&"tool errors by tool and reason (first 60 chars):"));
    assert!(
        rest.iter()
            .any(|l| l.trim() == "1  edit_file: no such anchor")
    );
}

#[test]
fn metrics_json_has_expected_fields() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("session.jsonl");
    write_sample_log(&log_path);

    let output = Command::new(bin())
        .args(["metrics", "--json", log_path.to_str().unwrap()])
        .output()
        .expect("failed to run statie metrics --json");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");

    assert_eq!(value["steps"], 3);
    assert_eq!(value["log"]["rejections"], 1);
    assert_eq!(value["log"]["failures"], 0);
    assert_eq!(value["log"]["tool_errors"], 1);
    assert_eq!(value["log"]["edits_attempted"], 2);
    assert_eq!(value["log"]["edits_landed"], 1);
    assert_eq!(value["log"]["steps_to_first_edit"], 3);
    assert_eq!(value["log"]["maintainer_runs"], 1);
    assert_eq!(value["log"]["maintainer_errors"], 0);
    assert_eq!(value["log"]["skipped_redundant"], 1);
    assert_eq!(value["log"]["paged_reads"], 0);
    assert_eq!(value["log"]["continued"], 0);
    assert_eq!(value["log"]["green_checkpoints"], 0);
    assert_eq!(value["log"]["restored_green"], 0);
    assert_eq!(value["activations"], serde_json::json!(["router"]));

    let rejections = value["rejections_by_reason"]
        .as_array()
        .expect("rejections_by_reason array");
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0]["count"], 1);
    assert_eq!(rejections[0]["reason"], "bad json");

    let tool_errors = value["tool_errors_by_tool_and_reason"]
        .as_array()
        .expect("tool_errors_by_tool_and_reason array");
    assert_eq!(tool_errors.len(), 1);
    assert_eq!(tool_errors[0]["count"], 1);
    assert_eq!(tool_errors[0]["tool"], "edit_file");
    assert_eq!(tool_errors[0]["reason"], "no such anchor");
}

#[test]
fn metrics_missing_file_errors_with_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.jsonl");

    let output = Command::new(bin())
        .args(["metrics", missing.to_str().unwrap()])
        .output()
        .expect("failed to run statie metrics");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no session log"), "stderr was: {stderr}");
}

#[test]
fn metrics_empty_log_prints_all_zeros_and_dash_fields() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("session.jsonl");
    std::fs::write(&log_path, "").unwrap();

    let output = Command::new(bin())
        .args(["metrics", log_path.to_str().unwrap()])
        .output()
        .expect("failed to run statie metrics");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary_line = stdout.lines().next().expect("summary line");
    assert!(summary_line.contains("steps=0"), "{summary_line}");
    assert!(summary_line.contains("first_edit=-"), "{summary_line}");
    assert!(summary_line.contains("activations="), "{summary_line}");
    assert!(summary_line.contains("last=-"), "{summary_line}");
}
