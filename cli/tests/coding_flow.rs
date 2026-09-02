//! End-to-end test: a headless mock run through the whole
//! router -> plan -> implement -> review coding flow (milestones 5 & 6).

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

#[test]
fn headless_mock_run_completes_plan_implement_review_flow() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let turn = |content: serde_json::Value| serde_json::json!({ "content": content.to_string() });

    let mock_script = vec![
        // 1. router
        turn(serde_json::json!({
            "patch": {"task": "create hello.txt", "next_skill": "plan"},
            "action": {"type": "done"},
            "ack_inbox": [1],
        })),
        // 2. plan
        turn(serde_json::json!({
            "patch": {"recent_summary": "listing"},
            "action": {"type": "tool", "name": "list_files", "args": {}},
        })),
        // 3. plan
        turn(serde_json::json!({
            "patch": {
                "plan": [{"id": 1, "text": "write hello.txt", "status": "pending"}],
                "next_skill": "implement"
            },
            "action": {"type": "done"},
        })),
        // 4. implement
        turn(serde_json::json!({
            "patch": {
                "plan": [{"id": 1, "text": "write hello.txt", "status": "in_progress"}]
            },
            "action": {
                "type": "tool",
                "name": "write_file",
                "args": {"path": "hello.txt", "content": "hello statie\n"}
            },
        })),
        // 5. implement
        turn(serde_json::json!({
            "patch": {
                "plan": [{"id": 1, "text": "write hello.txt", "status": "done"}],
                "files": [{"path": "hello.txt", "note": "created"}],
                "next_skill": "review"
            },
            "action": {"type": "done"},
        })),
        // 6. review
        turn(serde_json::json!({
            "patch": {"recent_summary": "checking"},
            "action": {"type": "tool", "name": "read_file", "args": {"path": "hello.txt"}},
        })),
        // 7. review
        turn(serde_json::json!({
            "patch": {"findings": [], "done": true},
            "action": {"type": "done"},
        })),
    ];

    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": mock_script,
        },
        "store": {"kind": "memory"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });

    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let output = Command::new(bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--skill",
            "router",
            "-p",
            "create hello.txt",
        ])
        .output()
        .expect("failed to run statie");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; stdout={stdout}\nstderr={stderr}"
    );

    assert!(stdout.contains("[activated] plan"), "stdout: {stdout}");
    assert!(stdout.contains("[activated] implement"), "stdout: {stdout}");
    assert!(stdout.contains("[activated] review"), "stdout: {stdout}");
    assert!(stdout.contains("outcome=done"), "stdout: {stdout}");

    let hello_path = sandbox_dir.path().join("hello.txt");
    let content = std::fs::read_to_string(&hello_path).expect("hello.txt should have been written");
    assert_eq!(content, "hello statie\n");
}
