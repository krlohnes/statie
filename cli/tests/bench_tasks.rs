//! Self-check for the `bench/tasks/**` task battery used by `statie bench`.
//!
//! For every task directory under `bench/tasks/`:
//! - `task.json` parses against the fixed format and is internally
//!   consistent (its `name` matches the directory, every `protected` path
//!   exists in `fixture/`).
//! - Copying `fixture/` into a fresh tempdir and running `test_cmd` there
//!   FAILS (non-zero exit) -- the fixture must not already satisfy the task.
//! - Overlaying `solution/` on top of that copy and running `test_cmd`
//!   again PASSES (exit 0) -- the reference solution must satisfy the task.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Deserialize;

/// Mirrors the fixed `bench/tasks/<name>/task.json` format. `deny_unknown_fields`
/// so a typo'd or stray key fails loudly instead of being silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskJson {
    name: String,
    prompt: String,
    test_cmd: String,
    protected: Vec<String>,
    timeout_secs: u64,
    tags: Vec<String>,
}

/// The list of task directory names the battery is expected to contain.
/// Kept explicit (rather than only discovered) so a missing task directory
/// fails with a clear name instead of just "found 6, expected 7".
const EXPECTED_TASKS: &[&str] = &[
    "add_function",
    "fix_off_by_one",
    "match_arms",
    "rename_across_files",
    "add_struct_field",
    "new_module",
    "python_method",
];

/// `bench/tasks`, resolved relative to this crate's manifest dir so the
/// test works regardless of the directory `cargo test` was invoked from.
fn bench_tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate should have a workspace-root parent")
        .join("bench")
        .join("tasks")
}

fn task_dir(name: &str) -> PathBuf {
    bench_tasks_dir().join(name)
}

/// Parses and validates `task.json` for `name`, returning the parsed struct.
/// Panics with a descriptive message on any problem.
fn load_task_json(name: &str) -> TaskJson {
    let dir = task_dir(name);
    let path = dir.join("task.json");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("[{name}] failed to read {}: {e}", path.display()));
    let task: TaskJson = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("[{name}] failed to parse {}: {e}", path.display()));

    assert_eq!(
        task.name, name,
        "[{name}] task.json `name` ({}) does not match directory name ({name})",
        task.name
    );
    assert!(
        !task.prompt.trim().is_empty(),
        "[{name}] task.json `prompt` should not be empty"
    );
    assert!(
        !task.test_cmd.trim().is_empty(),
        "[{name}] task.json `test_cmd` should not be empty"
    );
    assert!(
        task.timeout_secs > 0,
        "[{name}] task.json `timeout_secs` should be positive"
    );
    assert!(
        !task.tags.is_empty(),
        "[{name}] task.json `tags` should not be empty"
    );

    let fixture_dir = dir.join("fixture");
    for rel in &task.protected {
        let p = fixture_dir.join(rel);
        assert!(
            p.is_file(),
            "[{name}] protected path `{rel}` does not exist as a file under fixture/ ({})",
            p.display()
        );
    }

    task
}

/// Copies the contents of `src` into `dst` recursively. `dst` must already
/// exist.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).unwrap_or_else(|e| panic!("reading {}: {e}", src.display())) {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&dst_path).unwrap();
            copy_dir_recursive(&src_path, &dst_path);
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::copy(&src_path, &dst_path).unwrap_or_else(|e| {
                panic!(
                    "copying {} -> {}: {e}",
                    src_path.display(),
                    dst_path.display()
                )
            });
        }
        // Symlinks and other file types are not expected in these fixtures
        // and are intentionally skipped.
    }
}

fn run_test_cmd(dir: &Path, test_cmd: &str) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(test_cmd)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{test_cmd}` in {}: {e}", dir.display()))
}

fn describe_output(output: &Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Runs the fixture-fails / solution-passes lifecycle for one task, given
/// its already-loaded and validated `task.json`.
fn check_task_lifecycle(name: &str, task: &TaskJson) {
    let dir = task_dir(name);
    let fixture_dir = dir.join("fixture");
    let solution_dir = dir.join("solution");

    assert!(
        fixture_dir.is_dir(),
        "[{name}] missing fixture/ directory at {}",
        fixture_dir.display()
    );
    assert!(
        solution_dir.is_dir(),
        "[{name}] missing solution/ directory at {}",
        solution_dir.display()
    );

    let tmp =
        tempfile::tempdir().unwrap_or_else(|e| panic!("[{name}] failed to create tempdir: {e}"));
    copy_dir_recursive(&fixture_dir, tmp.path());

    let fixture_output = run_test_cmd(tmp.path(), &task.test_cmd);
    assert!(
        !fixture_output.status.success(),
        "[{name}] expected `{}` to FAIL against the raw fixture, but it succeeded.\n{}",
        task.test_cmd,
        describe_output(&fixture_output)
    );

    // Force a clean rebuild for the solution phase: cargo's fingerprinting
    // is mtime-based, and overlaying solution/ onto the fixture copy can
    // land within the same filesystem-mtime tick as the fixture build,
    // which would otherwise make cargo believe nothing changed and skip
    // recompiling the (now-fixed) sources.
    let _ = fs::remove_dir_all(tmp.path().join("target"));

    copy_dir_recursive(&solution_dir, tmp.path());

    let solution_output = run_test_cmd(tmp.path(), &task.test_cmd);
    assert!(
        solution_output.status.success(),
        "[{name}] expected `{}` to PASS after overlaying solution/, but it failed.\n{}",
        task.test_cmd,
        describe_output(&solution_output)
    );
}

fn run_one(name: &str) {
    let task = load_task_json(name);
    check_task_lifecycle(name, &task);
}

#[test]
fn add_function_fixture_fails_then_solution_passes() {
    run_one("add_function");
}

#[test]
fn fix_off_by_one_fixture_fails_then_solution_passes() {
    run_one("fix_off_by_one");
}

#[test]
fn match_arms_fixture_fails_then_solution_passes() {
    run_one("match_arms");
}

#[test]
fn rename_across_files_fixture_fails_then_solution_passes() {
    run_one("rename_across_files");
}

#[test]
fn add_struct_field_fixture_fails_then_solution_passes() {
    run_one("add_struct_field");
}

#[test]
fn new_module_fixture_fails_then_solution_passes() {
    run_one("new_module");
}

#[test]
fn python_method_fixture_fails_then_solution_passes() {
    run_one("python_method");
}

/// Every directory under `bench/tasks/` must have all three parts
/// (`task.json`, `fixture/`, `solution/`), and the set of directories must
/// match exactly what this battery is expected to contain -- catching a
/// stray or forgotten task directory.
#[test]
fn every_task_directory_has_all_three_parts() {
    let dir = bench_tasks_dir();
    let mut found: BTreeSet<String> = BTreeSet::new();

    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display())) {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let task_path = entry.path();

        assert!(
            task_path.join("task.json").is_file(),
            "[{name}] missing task.json"
        );
        assert!(
            task_path.join("fixture").is_dir(),
            "[{name}] missing fixture/ directory"
        );
        assert!(
            task_path.join("solution").is_dir(),
            "[{name}] missing solution/ directory"
        );

        found.insert(name);
    }

    let expected: BTreeSet<String> = EXPECTED_TASKS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        found, expected,
        "bench/tasks directory set does not match the expected task battery"
    );
}
