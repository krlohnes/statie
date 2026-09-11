//! End-to-end tests for `statie bench`: task discovery/validation, and one
//! full run through the real `statie` binary (mock backend) landing a
//! trivial fixture.

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

fn write_task(tasks_dir: &std::path::Path, name: &str, task_json: serde_json::Value) {
    let task_dir = tasks_dir.join(name);
    std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
    std::fs::create_dir_all(task_dir.join("solution")).unwrap();
    std::fs::write(
        task_dir.join("task.json"),
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();
}

fn default_task_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "prompt": "do the thing",
        "test_cmd": "true",
        "protected": [],
        "timeout_secs": 30,
        "tags": []
    })
}

/// `--task` filters to a subset; a malformed task directory elsewhere under
/// `--tasks` still errors (task discovery validates every directory before
/// any run happens), naming the offending task.
#[test]
fn bench_discovery_errors_on_malformed_task_before_running_anything() {
    let dir = tempfile::tempdir().unwrap();
    write_task(dir.path(), "good_one", default_task_json("good_one"));
    write_task(dir.path(), "good_two", default_task_json("good_two"));

    // A third task directory with no fixture/ -- malformed.
    let broken_dir = dir.path().join("broken");
    std::fs::create_dir_all(&broken_dir).unwrap();
    std::fs::write(
        broken_dir.join("task.json"),
        serde_json::to_string_pretty(&default_task_json("broken")).unwrap(),
    )
    .unwrap();

    let output = Command::new(bin())
        .args(["bench", "--tasks", dir.path().to_str().unwrap()])
        .output()
        .expect("failed to run statie bench");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("broken"), "stderr: {stderr}");
}

/// A `--tasks` dir whose task's mock backend has an empty script: the
/// process runs, the harness ends without a scripted turn, `statie bench`
/// still parses whatever outcome comes back (or reports `crashed`/
/// `timeout`), reports the run as non-landing, and does not panic. This
/// covers the real spawn path (the actual `statie` binary, cwd = a fresh
/// sandbox copied from `fixture/`) without needing the full multi-step
/// router -> implement -> review flow.
#[test]
fn bench_spawn_path_with_empty_mock_script_reports_non_landing_no_panic() {
    let tasks_dir = tempfile::tempdir().unwrap();
    let task_dir = tasks_dir.path().join("empty_script");
    std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
    std::fs::create_dir_all(task_dir.join("solution")).unwrap();
    std::fs::write(
        task_dir.join("task.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": "empty_script",
            "prompt": "do the thing",
            "test_cmd": "test -f done.txt",
            "protected": [],
            "timeout_secs": 30,
            "tags": []
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg_path = cfg_dir.path().join("config.json");
    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": []
        },
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
    });
    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let out_dir = tempfile::tempdir().unwrap();

    let output = Command::new(bin())
        .args([
            "bench",
            "--tasks",
            tasks_dir.path().to_str().unwrap(),
            "--config",
            cfg_path.to_str().unwrap(),
            "--out",
            out_dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("failed to run statie bench");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bench itself should exit 0 even though the run inside it doesn't land; \
         stdout={stdout}\nstderr={stderr}"
    );

    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");
    let runs = value["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["landed"], false);
    assert_eq!(runs[0]["task"], "empty_script");

    // Artifacts were written for the run.
    let run_dir = out_dir.path().join("empty_script").join("seed1");
    assert!(run_dir.join("stdout.txt").exists());
    assert!(run_dir.join("stderr.txt").exists());
    assert!(run_dir.join("config.json").exists());
}

/// Full end-to-end run through the real router -> implement -> review flow
/// (the same scripted shape as `coding_flow.rs`'s
/// `headless_mock_run_completes_plan_implement_review_flow`), but writing
/// `hello.txt` -- a fixture whose `test_cmd` is `test -f hello.txt` --
/// checked here via `statie bench` itself rather than a raw `statie -p`
/// invocation, proving the bench's fixture-copy -> spawn -> verify ->
/// metrics pipeline lands a real (mock-backed) run end to end.
#[test]
fn bench_lands_trivial_fixture_through_real_binary_and_mock_backend() {
    let tasks_dir = tempfile::tempdir().unwrap();
    let task_dir = tasks_dir.path().join("write_hello");
    std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
    std::fs::create_dir_all(task_dir.join("solution")).unwrap();
    std::fs::write(
        task_dir.join("task.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": "write_hello",
            "prompt": "create hello.txt",
            "test_cmd": "test -f hello.txt",
            "protected": [],
            "timeout_secs": 60,
            "tags": ["trivial"]
        }))
        .unwrap(),
    )
    .unwrap();

    let turn = |content: serde_json::Value| serde_json::json!({ "content": content.to_string() });
    let mock_script = vec![
        // 1. router -> implement directly (no plan needed for this trivial
        // fixture).
        turn(serde_json::json!({
            "patch": {"task": "create hello.txt", "next_skill": "implement"},
            "action": {"type": "done"},
            "ack_inbox": [1],
        })),
        // 2. implement: write the file.
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
        // 3. implement: a passing test call sets green_step.
        turn(serde_json::json!({
            "patch": {
                "plan": [{"id": 1, "text": "write hello.txt", "status": "done"}],
                "files": [{"path": "hello.txt", "note": "created"}]
            },
            "action": {"type": "tool", "name": "test", "args": {"cmd": "true"}},
        })),
        // 4. implement -> review, using the green_step from step 3.
        turn(serde_json::json!({
            "patch": {"next_skill": "review"},
            "action": {"type": "done"},
        })),
        // 5. review: approve (green_step survives the handoff).
        turn(serde_json::json!({
            "patch": {"findings": [], "done": true},
            "action": {"type": "done"},
        })),
    ];

    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg_path = cfg_dir.path().join("config.json");
    let cfg = serde_json::json!({
        "model": {
            "backend": "mock",
            "mock_script": mock_script,
        },
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
    });
    let mut file = std::fs::File::create(&cfg_path).unwrap();
    file.write_all(serde_json::to_string(&cfg).unwrap().as_bytes())
        .unwrap();
    drop(file);

    let out_dir = tempfile::tempdir().unwrap();

    let output = Command::new(bin())
        .args([
            "bench",
            "--tasks",
            tasks_dir.path().to_str().unwrap(),
            "--config",
            cfg_path.to_str().unwrap(),
            "--out",
            out_dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("failed to run statie bench");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");

    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");
    let runs = value["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "stdout: {stdout}");
    assert_eq!(runs[0]["task"], "write_hello");
    assert_eq!(runs[0]["landed"], true, "run: {}", runs[0]);
    assert_eq!(runs[0]["outcome"], "done");

    let task_summary = &value["tasks"]["write_hello"];
    assert_eq!(task_summary["landed"], 1);
    assert_eq!(task_summary["total"], 1);
}
