//! End-to-end tests for `statie audit <run_a_dir> <run_b_dir>`: writes two
//! small `bench-runs/<label>/<task>/seed<N>/{session.jsonl,stdout.txt}`
//! fixtures by hand (small inline strings, not a real `statie bench` run)
//! and runs the binary against them.

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

fn write_jsonl(path: &std::path::Path, entries: &[serde_json::Value]) {
    let lines: Vec<String> = entries.iter().map(|v| v.to_string()).collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Builds `<root>/<task>/seed<seed>/{session.jsonl,stdout.txt}`, returning
/// the run directory. Both runs share identical steps 0-2 (activation, a
/// `task`+`plan` patch, a `next_skill` patch) and then diverge at step 3:
/// the caller supplies step 3's action payload, `decision.answer`, and the
/// final `prompt_tokens` entry (used for both the patch's `usage` and the
/// `stdout.txt` summary line).
fn write_run(
    root: &std::path::Path,
    task: &str,
    seed: u64,
    step3_action: serde_json::Value,
    step3_decision_answer: &str,
    step3_prompt_tokens: u64,
) -> std::path::PathBuf {
    let dir = root.join(task).join(format!("seed{seed}"));
    std::fs::create_dir_all(&dir).unwrap();

    let entries = vec![
        entry(
            0,
            "system",
            serde_json::json!({"event": "activate", "skill": "router"}),
        ),
        entry(
            1,
            "patch",
            serde_json::json!({
                "version": 1,
                "patch": {
                    "task": "fix the bug",
                    "plan": [{"id": 1, "text": "look around"}],
                },
                "usage": {"prompt_tokens": 100, "completion_tokens": 10},
            }),
        ),
        entry(1, "action", serde_json::json!({"type": "done"})),
        entry(
            2,
            "patch",
            serde_json::json!({
                "version": 2,
                "patch": {"next_skill": "implement"},
                "usage": {"prompt_tokens": 200, "completion_tokens": 20},
            }),
        ),
        entry(2, "action", serde_json::json!({"type": "done"})),
        entry(3, "action", step3_action),
        entry(
            3,
            "patch",
            serde_json::json!({
                "version": 3,
                "patch": {
                    "decision": {
                        "question": "is it a rounding bug?",
                        "answer": step3_decision_answer,
                        "step": 3,
                    },
                },
                "usage": {"prompt_tokens": step3_prompt_tokens, "completion_tokens": 30},
            }),
        ),
    ];
    write_jsonl(&dir.join("session.jsonl"), &entries);

    let stdout = format!(
        "[info] session: fresh\n\
         steps=3 outcome=done prompt_tokens=[100, 200, {step3_prompt_tokens}]\n"
    );
    std::fs::write(dir.join("stdout.txt"), stdout).unwrap();

    dir
}

#[test]
fn audit_reports_header_action_and_state_divergence_at_step_3() {
    let root = tempfile::tempdir().unwrap();

    let run_a = write_run(
        root.path(),
        "mytask",
        1,
        serde_json::json!({"type": "tool", "name": "read_file", "args": {"path": "src/lib.rs"}}),
        "yes, a rounding bug",
        300,
    );
    let run_b = write_run(
        root.path(),
        "mytask",
        2,
        serde_json::json!({"type": "tool", "name": "shell", "args": {"cmd": "cargo test"}}),
        "no, not a rounding bug",
        350,
    );

    let output = Command::new(bin())
        .arg("audit")
        .arg(&run_a)
        .arg(&run_b)
        .output()
        .expect("failed to run statie audit");

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    // 1. Header lines.
    assert_eq!(
        lines[0], "run a: task=mytask seed=1 outcome=done steps=3",
        "full stdout:\n{stdout}"
    );
    assert_eq!(
        lines[1], "run b: task=mytask seed=2 outcome=done steps=3",
        "full stdout:\n{stdout}"
    );

    // 2. First action divergence: step 3, read_file(src/lib.rs) vs shell(cargo test).
    assert!(
        lines.contains(&"action divergence at step 3: a=read_file(src/lib.rs) b=shell(cargo test)"),
        "full stdout:\n{stdout}"
    );

    // 3. First harness-state divergence: step 3, field `decision` (the only
    // field that differs between the two runs' reconstructed workspaces).
    let state_line = lines
        .iter()
        .find(|l| l.starts_with("harness state divergence at step"))
        .unwrap_or_else(|| panic!("no harness state divergence line in:\n{stdout}"));
    assert!(
        state_line.starts_with("harness state divergence at step 3, field decision:"),
        "state_line={state_line}"
    );
    assert!(
        state_line.contains("yes, a rounding bug"),
        "state_line={state_line}"
    );
    assert!(
        state_line.contains("no, not a rounding bug"),
        "state_line={state_line}"
    );

    // 4. Per-step table: header row plus the step-3 row with the two
    // diverging actions and their exact prompt_tokens.
    assert!(
        lines.contains(&"step | action_a | action_b | prompt_tokens_a | prompt_tokens_b"),
        "full stdout:\n{stdout}"
    );
    assert!(
        lines.contains(&"3 | read_file(src/lib.rs) | shell(cargo test) | 300 | 350"),
        "full stdout:\n{stdout}"
    );
}

#[test]
fn audit_missing_session_log_prints_na_and_exits_zero() {
    let root = tempfile::tempdir().unwrap();
    let run_a = root.path().join("mytask").join("seed1");
    let run_b = root.path().join("mytask").join("seed2");
    std::fs::create_dir_all(&run_a).unwrap();
    std::fs::create_dir_all(&run_b).unwrap();
    // Neither run has a session.jsonl or stdout.txt.

    let output = Command::new(bin())
        .arg("audit")
        .arg(&run_a)
        .arg(&run_b)
        .output()
        .expect("failed to run statie audit");

    assert!(
        output.status.success(),
        "expected exit 0, stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(lines[0], "run a: task=mytask seed=1 outcome=n/a steps=n/a");
    assert_eq!(lines[1], "run b: task=mytask seed=2 outcome=n/a steps=n/a");
    assert!(
        lines.iter().any(|l| l.starts_with(
            "action divergence: section unavailable: could not read session.jsonl for"
        )),
        "full stdout:\n{stdout}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with(
            "harness state divergence: section unavailable: could not read session.jsonl for"
        )),
        "full stdout:\n{stdout}"
    );
    assert!(
        lines.contains(&"per-step table: section unavailable: no divergence found"),
        "full stdout:\n{stdout}"
    );
}
