//! End-to-end tests for the default-fresh / `--resume` session semantics
//! (sqlite store archived under `.statie/archive/<timestamp>/` unless
//! `--resume` is passed).

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_statie")
}

/// A minimal mock script that drives the `toy` skill to `done` in one step.
fn toy_done_script() -> serde_json::Value {
    serde_json::json!([
        {
            "content": serde_json::json!({
                "patch": {"counter": 2, "recent_summary": "did step"},
                "action": {"type": "done"},
                "ack_inbox": [1],
            }).to_string()
        },
    ])
}

fn write_config(cfg_path: &std::path::Path, cfg: &serde_json::Value) {
    let mut file = std::fs::File::create(cfg_path).unwrap();
    file.write_all(serde_json::to_string(cfg).unwrap().as_bytes())
        .unwrap();
}

fn run_toy(cfg_path: &std::path::Path, extra_args: &[&str]) -> std::process::Output {
    let mut args = vec![
        "--config",
        cfg_path.to_str().unwrap(),
        "--skill",
        "toy",
        "--toy-target",
        "2",
        "-p",
        "go",
    ];
    args.extend_from_slice(extra_args);
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run statie")
}

#[test]
fn first_run_creates_sqlite_store_and_log() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let output = run_toy(&cfg_path, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");

    let db_path = sandbox_dir.path().join(".statie/session.db");
    let log_path = sandbox_dir.path().join(".statie/session.jsonl");
    assert!(db_path.exists(), "expected {} to exist", db_path.display());
    assert!(
        log_path.exists(),
        "expected {} to exist",
        log_path.display()
    );
}

#[test]
fn first_run_writes_self_ignoring_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let output = run_toy(&cfg_path, &[]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let gitignore_path = sandbox_dir.path().join(".statie/.gitignore");
    assert!(
        gitignore_path.exists(),
        "expected {} to exist",
        gitignore_path.display()
    );
    let contents = std::fs::read_to_string(&gitignore_path).unwrap();
    assert_eq!(contents, "*\n");
}

#[test]
fn preexisting_gitignore_is_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let statie_dir = sandbox_dir.path().join(".statie");
    std::fs::create_dir_all(&statie_dir).unwrap();
    let gitignore_path = statie_dir.join(".gitignore");
    std::fs::write(&gitignore_path, "custom-content\n").unwrap();

    let output = run_toy(&cfg_path, &[]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let contents = std::fs::read_to_string(&gitignore_path).unwrap();
    assert_eq!(
        contents, "custom-content\n",
        "existing .gitignore should be left unchanged"
    );
}

#[test]
fn second_run_without_resume_archives_old_session_and_starts_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let first = run_toy(&cfg_path, &[]);
    assert!(
        first.status.success(),
        "first run failed; stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );

    let db_path = sandbox_dir.path().join(".statie/session.db");
    let log_path = sandbox_dir.path().join(".statie/session.jsonl");
    assert!(db_path.exists());
    assert!(log_path.exists());

    let second = run_toy(&cfg_path, &[]);
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stderr.contains("archived previous session to"),
        "expected archive notice in stderr: {stderr}"
    );

    let archive_root = sandbox_dir.path().join(".statie/archive");
    assert!(
        archive_root.exists(),
        "expected {} to exist",
        archive_root.display()
    );

    let entries: Vec<_> = std::fs::read_dir(&archive_root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one archive timestamp dir: {entries:?}"
    );
    let archive_ts_dir = &entries[0];
    assert!(archive_ts_dir.is_dir());

    assert!(
        archive_ts_dir.join("session.db").exists(),
        "expected archived session.db in {}",
        archive_ts_dir.display()
    );
    assert!(
        archive_ts_dir.join("session.jsonl").exists(),
        "expected archived session.jsonl in {}",
        archive_ts_dir.display()
    );

    // A new session.db exists at the original path after the fresh-by-default run.
    assert!(
        db_path.exists(),
        "expected a new {} after the default (fresh) run",
        db_path.display()
    );
    assert!(
        log_path.exists(),
        "expected a new {} after the default (fresh) run",
        log_path.display()
    );
}

#[test]
fn second_run_with_resume_does_not_archive_and_prints_notice() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let first = run_toy(&cfg_path, &[]);
    assert!(
        first.status.success(),
        "first run failed; stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run_toy(&cfg_path, &["--resume"]);
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stderr.contains("resuming session at"),
        "expected resume notice in stderr: {stderr}"
    );

    let archive_dir = sandbox_dir.path().join(".statie/archive");
    assert!(
        !archive_dir.exists(),
        "no archive dir should exist with --resume"
    );
}

/// A resumed headless run replays recent log history before continuing: its
/// stdout must show the `[resumed] step ...` marker followed by the
/// replayed entries (including the earlier `user` message logged by the
/// first run), and -- since `--skill toy` is passed explicitly here, same
/// as the stored activation -- the activation it prints is `toy`.
#[test]
fn second_run_with_resume_replays_history_and_honors_explicit_skill() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let first = run_toy(&cfg_path, &[]);
    assert!(
        first.status.success(),
        "first run failed; stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run_toy(&cfg_path, &["--resume"]);
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "stdout={stdout}\nstderr={stderr}");

    assert!(
        stdout.contains("[resumed] step"),
        "expected a `[resumed] step` marker in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("user:"),
        "expected a replayed `user` log line in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("[activated] toy"),
        "expected activation of `toy` (explicit --skill toy matches the stored \
         activation) in stdout:\n{stdout}"
    );
}

#[test]
fn resume_with_no_existing_session_succeeds_without_archiving() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let output = run_toy(&cfg_path, &["--resume"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        !stderr.contains("archived previous session to"),
        "should not archive when there was nothing to resume: {stderr}"
    );

    let archive_dir = sandbox_dir.path().join(".statie/archive");
    assert!(
        !archive_dir.exists(),
        "no archive dir should exist on a first run with --resume"
    );

    let db_path = sandbox_dir.path().join(".statie/session.db");
    assert!(db_path.exists(), "expected {} to exist", db_path.display());
}

#[test]
fn memory_creates_no_files_and_never_archives_or_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.json");
    let sandbox_dir = tempfile::tempdir().unwrap();

    let cfg = serde_json::json!({
        "model": {"backend": "mock", "mock_script": toy_done_script()},
        "store": {"kind": "sqlite"},
        "augmenters": {
            "repo_bm25": {"enabled": false},
            "session_log_bm25": {"enabled": false},
        },
        "sandbox_root": sandbox_dir.path().to_str().unwrap(),
    });
    write_config(&cfg_path, &cfg);

    let output = run_toy(&cfg_path, &["--memory"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        !stderr.contains("archived previous session to"),
        "should not archive under --memory: {stderr}"
    );
    assert!(
        !stderr.contains("resuming session at"),
        "should not print resume notice under --memory: {stderr}"
    );

    let statie_dir = sandbox_dir.path().join(".statie");
    assert!(
        !statie_dir.exists(),
        "no .statie files should be created under --memory: {statie_dir:?} exists"
    );
}
