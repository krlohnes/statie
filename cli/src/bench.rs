//! `statie bench [--tasks DIR] [--task NAME]... [--seeds N] [--config PATH]
//! [--out DIR] [--json] [--keep]`: runs a battery of small tasks (fixed
//! format below), several seeds each, through the real `statie` binary and
//! reports landing rate and context cost per task shape. The point is to
//! measure whether the harness gets work done within a limited context, not
//! how tidy its state is.
//!
//! Task format (fixed): `<tasks_dir>/<name>/task.json` is
//! `{ "name", "prompt", "test_cmd", "protected": [...], "timeout_secs",
//! "tags": [...] }`; `<tasks_dir>/<name>/fixture/` is the sandbox contents
//! copied for each run; `<tasks_dir>/<name>/solution/` exists but is never
//! read here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use statie_core::{LogEntry, read_jsonl};

use crate::config::Config;
use crate::metrics::MetricsSummary;

/// One task's fixed on-disk description
/// (`<tasks_dir>/<name>/task.json`, ARCHITECTURE.md/spec "bench task
/// format"). Unknown fields are a hard error so a typo'd field name in a
/// fixture is caught rather than silently ignored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub name: String,
    pub prompt: String,
    pub test_cmd: String,
    #[serde(default)]
    pub protected: Vec<String>,
    pub timeout_secs: u64,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A discovered task: its spec plus the path to `fixture/`. `solution/` is
/// part of the fixed on-disk format but is never read by the bench.
#[derive(Clone, Debug)]
pub struct Task {
    pub spec: TaskSpec,
    pub fixture_dir: PathBuf,
}

/// Errors discovering/validating tasks under `--tasks`. Always carries the
/// task (directory) name so a malformed fixture is easy to place.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("task `{name}`: missing task.json")]
    MissingTaskJson { name: String },
    #[error("task `{name}`: missing fixture/ directory")]
    MissingFixture { name: String },
    #[error("task `{name}`: failed to read task.json: {source}")]
    Io {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("task `{name}`: invalid task.json: {source}")]
    Parse {
        name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("task `{name}`: task.json `name` field (`{field}`) does not match directory name")]
    NameMismatch { name: String, field: String },
}

/// Discovers every task directory under `tasks_dir` (each immediate
/// subdirectory is one task), validates it, and returns them sorted by
/// name. `filter`, if non-empty, keeps only tasks whose name is in the set
/// (in `filter`'s given order is not preserved -- callers get sorted
/// output; use `filter` purely as a name allowlist).
pub fn discover_tasks(tasks_dir: &Path, filter: &[String]) -> Result<Vec<Task>, TaskError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(tasks_dir)
        .map_err(|source| TaskError::Io {
            name: tasks_dir.display().to_string(),
            source,
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    let mut tasks = Vec::new();
    for dir in entries {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if !filter.is_empty() && !filter.contains(&name) {
            continue;
        }

        let task_json = dir.join("task.json");
        if !task_json.exists() {
            return Err(TaskError::MissingTaskJson { name });
        }
        let fixture_dir = dir.join("fixture");
        if !fixture_dir.is_dir() {
            return Err(TaskError::MissingFixture { name });
        }

        let text = std::fs::read_to_string(&task_json).map_err(|source| TaskError::Io {
            name: name.clone(),
            source,
        })?;
        let spec: TaskSpec = serde_json::from_str(&text).map_err(|source| TaskError::Parse {
            name: name.clone(),
            source,
        })?;

        if spec.name != name {
            return Err(TaskError::NameMismatch {
                name,
                field: spec.name,
            });
        }

        tasks.push(Task { spec, fixture_dir });
    }

    Ok(tasks)
}

/// Outcome label for one run, as reported by `statie bench`'s own logic
/// (not the harness's `outcome=` string, which becomes `Ran`'s payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The headless binary finished and printed a summary line; `String` is
    /// the `outcome=` label it reported (e.g. `done`, `parked`, `quit`).
    Ran(String),
    /// The process exceeded `timeout_secs` and was killed.
    Timeout,
    /// The process exited (crashed, or exited non-zero) without printing a
    /// parseable summary line.
    Crashed,
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunOutcome::Ran(label) => write!(f, "{label}"),
            RunOutcome::Timeout => write!(f, "timeout"),
            RunOutcome::Crashed => write!(f, "crashed"),
        }
    }
}

/// What a runner (real subprocess, or a test fake) produces for one run.
/// The sandbox itself is inspected separately (landing/protected checks);
/// this is just the process-level result.
#[derive(Clone, Debug)]
pub struct RunOutput {
    pub outcome: RunOutcome,
    pub wall: Duration,
    pub stdout: String,
    pub stderr: String,
}

/// Parses the `outcome=` token out of the headless summary line
/// (`cli/src/main.rs`: `"steps={} outcome={} prompt_tokens={:?}"`, printed
/// around lines 525/615). Returns `None` if no such line is present (a
/// crash before the first step, or output that never reached the summary).
pub fn parse_outcome(stdout: &str) -> Option<String> {
    for line in stdout.lines().rev() {
        if let Some(rest) = line.strip_prefix("outcome=") {
            // Whole-line form (shouldn't happen given the real format, but
            // keep it simple and permissive).
            let end = rest.find(' ').unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
        if let Some(idx) = line.find("outcome=") {
            let rest = &line[idx + "outcome=".len()..];
            let end = rest.find(' ').unwrap_or(rest.len());
            if !rest[..end].is_empty() {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// One session run, substitutable so tests don't need a real model. Takes
/// the sandbox directory (cwd for the run), the seeded prompt, and the
/// timeout; returns process-level results. Implementors are responsible
/// for actually invoking `statie -p <prompt>` (or a fake standing in for
/// it) with cwd = `sandbox`.
#[async_trait::async_trait]
pub trait SessionRunner: Send + Sync {
    async fn run_session(&self, sandbox: &Path, prompt: &str, timeout: Duration) -> RunOutput;
}

/// The real runner: spawns `std::env::current_exe()` (the `statie` binary
/// itself) with `-p <prompt> --config .statie/config.json`, cwd = sandbox,
/// stdout/stderr captured, killed after `timeout`.
pub struct BinaryRunner {
    /// Path to the `statie` binary to spawn. Defaults to
    /// `std::env::current_exe()` via `BinaryRunner::new`.
    pub exe: PathBuf,
}

impl BinaryRunner {
    pub fn new() -> anyhow::Result<Self> {
        Ok(BinaryRunner {
            exe: std::env::current_exe()?,
        })
    }
}

#[async_trait::async_trait]
impl SessionRunner for BinaryRunner {
    async fn run_session(&self, sandbox: &Path, prompt: &str, timeout: Duration) -> RunOutput {
        let start = Instant::now();
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("--config")
            .arg(".statie/config.json")
            .arg("-p")
            .arg(prompt)
            .current_dir(sandbox)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return RunOutput {
                    outcome: RunOutcome::Crashed,
                    wall: start.elapsed(),
                    stdout: String::new(),
                    stderr: format!("failed to spawn statie binary: {e}"),
                };
            }
        };

        let output = tokio::time::timeout(timeout, child.wait_with_output()).await;
        let wall = start.elapsed();

        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let outcome = match parse_outcome(&stdout) {
                    Some(label) => RunOutcome::Ran(label),
                    None => RunOutcome::Crashed,
                };
                RunOutput {
                    outcome,
                    wall,
                    stdout,
                    stderr,
                }
            }
            Ok(Err(e)) => RunOutput {
                outcome: RunOutcome::Crashed,
                wall,
                stdout: String::new(),
                stderr: format!("failed to wait on statie binary: {e}"),
            },
            Err(_) => {
                // Timed out: `child` was moved into `wait_with_output`'s
                // future, which is now dropped -- tokio's `Child` kills the
                // process on drop only if `kill_on_drop(true)` was set,
                // which we did above, so the process (and, since it's its
                // own process group leader by default on most platforms, at
                // least the direct child) is reaped by the runtime. We
                // still can't recover any partial stdout/stderr here since
                // it was piped into the now-dropped future.
                RunOutput {
                    outcome: RunOutcome::Timeout,
                    wall,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            }
        }
    }
}

/// Recursively copies `src` into `dst` (`dst` is created if missing),
/// preserving relative structure. Used both to seed a run's sandbox from
/// `fixture/` and to snapshot `fixture/` bytes for the protected-file
/// check.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
        // Symlinks in a fixture would be unusual; skip rather than fail.
    }
    Ok(())
}

/// Merges `patch` into `base`, recursively for objects (patch values win;
/// other types replace outright) -- a small JSON merge-patch, matching how
/// the harness itself treats `model.options`.
fn json_merge(base: &mut serde_json::Value, patch: serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                json_merge(base_map.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (base_slot, patch_val) => {
            *base_slot = patch_val;
        }
    }
}

/// Builds the per-run config: `base` (already loaded) with `sandbox_root`
/// set to `"."` and `model.options.seed` merged in (preserving other
/// options), serialized to pretty JSON ready to write to
/// `<sandbox>/.statie/config.json`. Store/log paths are left at their
/// configured values (defaults are already sandbox-relative -- see
/// `Config::default`), so nothing points back at the base config's
/// sandbox as long as those weren't set to absolute paths elsewhere.
pub fn build_run_config(base: &Config, seed: u64) -> anyhow::Result<serde_json::Value> {
    let mut value = serde_json::to_value(base)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config did not serialize to a JSON object"))?;
    obj.insert(
        "sandbox_root".to_string(),
        serde_json::Value::String(".".to_string()),
    );

    let model = obj.entry("model").or_insert_with(|| serde_json::json!({}));
    if let serde_json::Value::Object(model_obj) = model {
        let options = model_obj
            .entry("options")
            .or_insert_with(|| serde_json::json!({}));
        json_merge(options, serde_json::json!({"seed": seed}));
    }

    Ok(value)
}

/// Server prompt-token stats derived from `Patch`-entry `payload.usage`
/// (server-reported), across every model call in the log.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ServerTokenStats {
    pub max: u64,
    pub mean: u64,
    pub last: u64,
}

/// Pulls `payload.usage.prompt_tokens` from every `Patch` entry that has
/// one, in log order, and reduces to max/mean(rounded)/last. `None` if no
/// entry carried usage (e.g. the mock backend with no scripted usage).
pub fn server_prompt_token_stats(entries: &[LogEntry]) -> Option<ServerTokenStats> {
    let values: Vec<u64> = entries
        .iter()
        .filter(|e| e.kind == statie_core::LogKind::Patch)
        .filter_map(|e| e.payload.get("usage")?.get("prompt_tokens")?.as_u64())
        .collect();

    if values.is_empty() {
        return None;
    }

    let max = values.iter().copied().max().unwrap_or(0);
    let sum: u64 = values.iter().sum();
    let mean = ((sum as f64) / (values.len() as f64)).round() as u64;
    let last = *values.last().unwrap();

    Some(ServerTokenStats { max, mean, last })
}

/// Whether every file listed in `protected` still has byte-identical
/// contents to the fixture's copy. `None` (rather than `false`) means the
/// protected list was empty -- there's nothing to violate, so `ok` is
/// vacuously true but callers may want to distinguish "checked and fine"
/// from "nothing to check"; here we just return `true` for the empty case,
/// matching "every protected file's bytes identical" over an empty set.
pub fn protected_files_ok(fixture_dir: &Path, sandbox_dir: &Path, protected: &[String]) -> bool {
    protected.iter().all(|rel| {
        let fixture_path = fixture_dir.join(rel);
        let sandbox_path = sandbox_dir.join(rel);
        match (std::fs::read(&fixture_path), std::fs::read(&sandbox_path)) {
            (Ok(a), Ok(b)) => a == b,
            // Missing on either side counts as violated -- a protected file
            // that vanished is exactly the kind of damage this check exists
            // to catch.
            _ => false,
        }
    })
}

/// Why a run is not reported as landed, when it isn't.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotLandedReason {
    /// `test_cmd` exited non-zero.
    TestFailed,
    /// `test_cmd` exited 0, but a protected file's bytes changed.
    Protected,
    /// The run did not reach a state where verification made sense (timed
    /// out or crashed before producing a summary).
    NoRun,
}

/// The full result of one (task, seed) run: process-level result, metrics,
/// and the landing verdict.
#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    pub task: String,
    pub seed: u64,
    pub outcome: String,
    pub landed: bool,
    pub landed_reason: Option<NotLandedReason>,
    pub steps: usize,
    pub rejections: usize,
    pub tool_errors: usize,
    pub edits_landed: usize,
    pub green_checkpoints: usize,
    pub server_tokens: Option<ServerTokenStats>,
    pub wall_secs: f64,
    /// Present only on a non-landing run (verify output tail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_tail: Option<String>,
    /// Present only on crash/timeout (stderr tail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// Aggregated stats for one task across its seeds.
#[derive(Clone, Debug, Serialize)]
pub struct TaskSummary {
    pub landed: usize,
    pub total: usize,
    pub median_steps: f64,
    pub mean_server_max: f64,
    pub mean_secs: f64,
    pub outcomes: BTreeMap<String, usize>,
}

/// Options for `run_bench`/`run_one`, gathering what would otherwise be a
/// very long argument list. `tasks_dir` and `task_filter` are not read by
/// `run_bench` itself (task discovery/filtering already happened by the
/// time `tasks: &[Task]` is passed in) but are kept here so `cmd_bench`
/// has one options struct to build and pass down.
pub struct BenchOptions {
    pub seeds: u64,
    pub base_config: Config,
    pub out_dir: PathBuf,
    pub keep: bool,
    /// Print the `format_run_line` progress line for each run as it
    /// finishes. `--json` mode turns this off so stdout is exactly one JSON
    /// document.
    pub print_progress: bool,
}

/// Full bench output: every run plus the per-task summaries, in task-name
/// order.
#[derive(Clone, Debug, Serialize)]
pub struct BenchReport {
    pub runs: Vec<RunResult>,
    pub tasks: BTreeMap<String, TaskSummary>,
}

/// Tail of `s`'s last `n` lines, joined back with `\n`.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The number of trailing lines kept for stderr/verify tails in reports.
const TAIL_LINES: usize = 30;

/// Runs `sh -c test_cmd` inside `sandbox`, returning `(exit_ok, combined
/// stdout+stderr)`.
fn run_test_cmd(sandbox: &Path, test_cmd: &str) -> (bool, String) {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(test_cmd)
        .current_dir(sandbox)
        .output();
    match output {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), combined)
        }
        Err(e) => (false, format!("failed to run test_cmd: {e}")),
    }
}

/// Runs one (task, seed): prepares a fresh sandbox, invokes `runner`,
/// verifies, computes metrics, and persists artifacts under
/// `opts.out_dir/<task>/seed<seed>/`. Returns the run result; never panics
/// on a runner failure (that becomes `Crashed`/`Timeout`, a reported row).
pub async fn run_one(
    task: &Task,
    seed: u64,
    opts: &BenchOptions,
    runner: &dyn SessionRunner,
) -> anyhow::Result<RunResult> {
    let work = tempfile::tempdir()?;
    let sandbox_dir = work.path().join("sandbox");
    copy_dir_recursive(&task.fixture_dir, &sandbox_dir)?;

    let statie_dir = sandbox_dir.join(".statie");
    std::fs::create_dir_all(&statie_dir)?;
    let run_cfg = build_run_config(&opts.base_config, seed)?;
    let cfg_text = serde_json::to_string_pretty(&run_cfg)?;
    std::fs::write(statie_dir.join("config.json"), &cfg_text)?;

    let timeout = Duration::from_secs(task.spec.timeout_secs);
    let run_output = runner
        .run_session(&sandbox_dir, &task.spec.prompt, timeout)
        .await;

    let log_path = sandbox_dir.join(".statie").join("session.jsonl");
    let entries: Vec<LogEntry> = if log_path.exists() {
        read_jsonl(&log_path).unwrap_or_default()
    } else {
        Vec::new()
    };
    let summary = MetricsSummary::from_entries(&entries);
    let server_tokens = server_prompt_token_stats(&entries);

    let mut verify_full = String::new();
    let (landed, landed_reason, verify_tail) = match &run_output.outcome {
        RunOutcome::Ran(_) => {
            let (exit_ok, verify_out) = run_test_cmd(&sandbox_dir, &task.spec.test_cmd);
            verify_full = verify_out;
            if !exit_ok {
                (
                    false,
                    Some(NotLandedReason::TestFailed),
                    Some(tail_lines(&verify_full, TAIL_LINES)),
                )
            } else if !protected_files_ok(&task.fixture_dir, &sandbox_dir, &task.spec.protected) {
                (
                    false,
                    Some(NotLandedReason::Protected),
                    Some(tail_lines(&verify_full, TAIL_LINES)),
                )
            } else {
                (true, None, None)
            }
        }
        RunOutcome::Timeout | RunOutcome::Crashed => (false, Some(NotLandedReason::NoRun), None),
    };

    let stderr_tail = matches!(
        run_output.outcome,
        RunOutcome::Timeout | RunOutcome::Crashed
    )
    .then(|| tail_lines(&run_output.stderr, TAIL_LINES));

    let result = RunResult {
        task: task.spec.name.clone(),
        seed,
        outcome: run_output.outcome.to_string(),
        landed,
        landed_reason,
        steps: summary.steps,
        rejections: summary.log.rejections,
        tool_errors: summary.log.tool_errors,
        edits_landed: summary.log.edits_landed,
        green_checkpoints: summary.log.green_checkpoints,
        server_tokens,
        wall_secs: run_output.wall.as_secs_f64(),
        verify_tail,
        stderr_tail,
    };

    persist_run_artifacts(
        opts,
        &task.spec.name,
        seed,
        &sandbox_dir,
        &entries,
        &run_output,
        &cfg_text,
        &verify_full,
    )?;

    Ok(result)
}

/// Writes `<out_dir>/<task>/seed<seed>/{session.jsonl, stdout.txt,
/// stderr.txt, verify.txt, config.json}`, and (with `--keep`) the whole
/// sandbox under `.../sandbox/`.
#[allow(clippy::too_many_arguments)]
fn persist_run_artifacts(
    opts: &BenchOptions,
    task_name: &str,
    seed: u64,
    sandbox_dir: &Path,
    entries: &[LogEntry],
    run_output: &RunOutput,
    cfg_text: &str,
    verify_out: &str,
) -> anyhow::Result<()> {
    let run_dir = opts.out_dir.join(task_name).join(format!("seed{seed}"));
    std::fs::create_dir_all(&run_dir)?;

    let mut jsonl = String::new();
    for e in entries {
        jsonl.push_str(&serde_json::to_string(e)?);
        jsonl.push('\n');
    }
    std::fs::write(run_dir.join("session.jsonl"), jsonl)?;
    std::fs::write(run_dir.join("stdout.txt"), &run_output.stdout)?;
    std::fs::write(run_dir.join("stderr.txt"), &run_output.stderr)?;
    std::fs::write(run_dir.join("config.json"), cfg_text)?;

    // verify.txt: the full output of `test_cmd` (empty when the session
    // crashed or timed out and verification did not run).
    std::fs::write(run_dir.join("verify.txt"), verify_out)?;

    if opts.keep {
        let sandbox_out = run_dir.join("sandbox");
        copy_dir_recursive(sandbox_dir, &sandbox_out)?;
    }

    Ok(())
}

/// The one-line-per-run summary printed as each run finishes:
/// `task seed outcome landed steps rej tool_err edits green srv_max secs`.
pub fn format_run_line(r: &RunResult) -> String {
    let srv_max = r
        .server_tokens
        .as_ref()
        .map(|s| s.max.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{} seed={} outcome={} landed={} steps={} rej={} tool_err={} edits={} green={} srv_max={} secs={:.1}",
        r.task,
        r.seed,
        r.outcome,
        r.landed,
        r.steps,
        r.rejections,
        r.tool_errors,
        r.edits_landed,
        r.green_checkpoints,
        srv_max,
        r.wall_secs,
    )
}

/// Median of a slice of `usize`, as `f64` (average of the two middle
/// values on an even count); `0.0` for an empty slice.
fn median_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2] as f64
    } else {
        (sorted[n / 2 - 1] as f64 + sorted[n / 2] as f64) / 2.0
    }
}

fn mean_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Builds the per-task summary from that task's runs (assumed already
/// filtered to one task, any order).
pub fn summarize_task(runs: &[&RunResult]) -> TaskSummary {
    let landed = runs.iter().filter(|r| r.landed).count();
    let total = runs.len();
    let steps: Vec<usize> = runs.iter().map(|r| r.steps).collect();
    let srv_max: Vec<f64> = runs
        .iter()
        .filter_map(|r| r.server_tokens.as_ref().map(|s| s.max as f64))
        .collect();
    let secs: Vec<f64> = runs.iter().map(|r| r.wall_secs).collect();

    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    for r in runs {
        *outcomes.entry(r.outcome.clone()).or_insert(0) += 1;
    }

    TaskSummary {
        landed,
        total,
        median_steps: median_usize(&steps),
        mean_server_max: mean_f64(&srv_max),
        mean_secs: mean_f64(&secs),
        outcomes,
    }
}

/// Prints the per-task summary table:
/// `task | landed k/n | median steps | mean srv_max | mean secs | outcomes`.
pub fn print_task_table(tasks: &BTreeMap<String, TaskSummary>) {
    println!(
        "{:<24} {:>10} {:>13} {:>13} {:>10}  outcomes",
        "task", "landed", "median steps", "mean srv_max", "mean secs"
    );
    for (name, s) in tasks {
        let outcomes: Vec<String> = s.outcomes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!(
            "{:<24} {:>7}/{:<2} {:>13.1} {:>13.1} {:>10.1}  {}",
            name,
            s.landed,
            s.total,
            s.median_steps,
            s.mean_server_max,
            s.mean_secs,
            outcomes.join(","),
        );
    }
}

/// Runs the full bench battery: discovers tasks under `opts.tasks_dir`
/// (already filtered by `opts.task_filter`), runs every (task, seed) pair
/// sequentially (one GPU), printing one line per run as it finishes, then
/// returns the full report.
pub async fn run_bench(
    tasks: &[Task],
    opts: &BenchOptions,
    runner: &dyn SessionRunner,
) -> anyhow::Result<BenchReport> {
    let mut runs = Vec::new();
    for task in tasks {
        for seed in 1..=opts.seeds {
            let result = run_one(task, seed, opts, runner).await?;
            if opts.print_progress {
                println!("{}", format_run_line(&result));
            }
            runs.push(result);
        }
    }

    let mut tasks_summary: BTreeMap<String, TaskSummary> = BTreeMap::new();
    for task in tasks {
        let task_runs: Vec<&RunResult> = runs.iter().filter(|r| r.task == task.spec.name).collect();
        if !task_runs.is_empty() {
            tasks_summary.insert(task.spec.name.clone(), summarize_task(&task_runs));
        }
    }

    Ok(BenchReport {
        runs,
        tasks: tasks_summary,
    })
}

/// `statie bench` entry point: wires CLI args to `run_bench`, using
/// `BinaryRunner` (the real `statie` binary) as the session runner.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_bench(
    tasks_dir: PathBuf,
    task_filter: Vec<String>,
    seeds: u64,
    cli_config: Option<PathBuf>,
    out: Option<PathBuf>,
    json: bool,
    keep: bool,
) -> anyhow::Result<()> {
    let (base_config, _path) = crate::config::load(cli_config)?;

    let tasks = discover_tasks(&tasks_dir, &task_filter).map_err(|e| anyhow::anyhow!("{e}"))?;
    if tasks.is_empty() {
        anyhow::bail!(
            "no tasks found under {} (after filtering)",
            tasks_dir.display()
        );
    }

    let (_keep_tempdir, out_dir) = match out {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            (None, dir)
        }
        None => {
            let tmp = tempfile::tempdir()?;
            let path = tmp.path().to_path_buf();
            (Some(tmp), path)
        }
    };

    let opts = BenchOptions {
        seeds,
        base_config,
        out_dir: out_dir.clone(),
        keep,
        print_progress: !json,
    };

    let runner = BinaryRunner::new()?;
    let report = run_bench(&tasks, &opts, &runner).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_task_table(&report.tasks);
    }

    // Printed to stderr (not stdout) so `--json` mode's stdout stays exactly
    // one JSON document.
    eprintln!("artifacts: {}", out_dir.display());
    // Keep the tempdir alive until here by holding `_keep_tempdir` in
    // scope through the print above; if it's `None` (an explicit --out was
    // given) nothing needs to survive.
    if let Some(tmp) = _keep_tempdir {
        // Leak intentionally: the whole point of printing the path is that
        // the artifacts remain on disk after `statie bench` exits.
        std::mem::forget(tmp);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(dir: &Path, name: &str, spec_extra: serde_json::Value) {
        let task_dir = dir.join(name);
        std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
        std::fs::create_dir_all(task_dir.join("solution")).unwrap();
        let mut spec = serde_json::json!({
            "name": name,
            "prompt": "do the thing",
            "test_cmd": "true",
            "protected": [],
            "timeout_secs": 60,
            "tags": []
        });
        if let (serde_json::Value::Object(base), serde_json::Value::Object(extra)) =
            (&mut spec, spec_extra)
        {
            base.extend(extra);
        }
        std::fs::write(
            task_dir.join("task.json"),
            serde_json::to_string_pretty(&spec).unwrap(),
        )
        .unwrap();
    }

    // -- discovery --

    #[test]
    fn discover_tasks_finds_valid_tasks_and_applies_filter() {
        let dir = tempfile::tempdir().unwrap();
        write_task(dir.path(), "alpha", serde_json::json!({}));
        write_task(dir.path(), "beta", serde_json::json!({}));

        let all = discover_tasks(dir.path(), &[]).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].spec.name, "alpha");
        assert_eq!(all[1].spec.name, "beta");

        let filtered = discover_tasks(dir.path(), &["beta".to_string()]).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].spec.name, "beta");
    }

    #[test]
    fn discover_tasks_missing_fixture_errors_with_task_name() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("broken");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(
            task_dir.join("task.json"),
            serde_json::json!({
                "name": "broken",
                "prompt": "x",
                "test_cmd": "true",
                "timeout_secs": 60,
            })
            .to_string(),
        )
        .unwrap();

        let err = discover_tasks(dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("broken"), "{err}");
        assert!(err.to_string().contains("fixture"), "{err}");
    }

    #[test]
    fn discover_tasks_unknown_field_errors_with_task_name() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("typo");
        std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
        std::fs::write(
            task_dir.join("task.json"),
            serde_json::json!({
                "name": "typo",
                "prompt": "x",
                "test_cmd": "true",
                "timeout_secs": 60,
                "protectd": []
            })
            .to_string(),
        )
        .unwrap();

        let err = discover_tasks(dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("typo"), "{err}");
    }

    // -- sandbox prep / config --

    #[test]
    fn build_run_config_sets_sandbox_root_and_seed_preserving_other_options() {
        let mut base = Config::default();
        base.model.options = serde_json::json!({"temperature": 0.2});

        let value = build_run_config(&base, 7).unwrap();
        assert_eq!(value["sandbox_root"], serde_json::json!("."));
        assert_eq!(value["model"]["options"]["seed"], serde_json::json!(7));
        assert_eq!(
            value["model"]["options"]["temperature"],
            serde_json::json!(0.2)
        );
    }

    #[test]
    fn copy_dir_recursive_copies_nested_fixture_contents() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("a.txt"), b"a").unwrap();
        std::fs::write(src.path().join("sub/b.txt"), b"b").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("sandbox");
        copy_dir_recursive(src.path(), &target).unwrap();

        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(target.join("sub/b.txt")).unwrap(), b"b");
    }

    // -- landing / protected --

    struct FakeRunner {
        outcome: RunOutcome,
        writes: Vec<(String, Vec<u8>)>,
        wall: Duration,
    }

    #[async_trait::async_trait]
    impl SessionRunner for FakeRunner {
        async fn run_session(
            &self,
            sandbox: &Path,
            _prompt: &str,
            _timeout: Duration,
        ) -> RunOutput {
            for (rel, contents) in &self.writes {
                let path = sandbox.join(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(path, contents).unwrap();
            }
            RunOutput {
                outcome: self.outcome.clone(),
                wall: self.wall,
                stdout: format!("steps=1 outcome={} prompt_tokens=[]", self.outcome),
                stderr: String::new(),
            }
        }
    }

    fn base_opts(out_dir: PathBuf) -> BenchOptions {
        BenchOptions {
            seeds: 1,
            base_config: Config::default(),
            out_dir,
            keep: false,
            print_progress: false,
        }
    }

    #[tokio::test]
    async fn fake_runner_writing_done_file_lands_against_test_f_check() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "landing",
            serde_json::json!({"test_cmd": "test -f done.txt"}),
        );
        let tasks = discover_tasks(dir.path(), &[]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = base_opts(out.path().to_path_buf());

        let runner = FakeRunner {
            outcome: RunOutcome::Ran("done".to_string()),
            writes: vec![("done.txt".to_string(), b"ok".to_vec())],
            wall: Duration::from_secs(1),
        };

        let result = run_one(&tasks[0], 1, &opts, &runner).await.unwrap();
        assert!(result.landed, "{result:?}");
        assert_eq!(result.landed_reason, None);
    }

    #[tokio::test]
    async fn fake_runner_modifying_protected_file_reports_not_landed_protected() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("guarded");
        std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
        std::fs::create_dir_all(task_dir.join("solution")).unwrap();
        std::fs::write(task_dir.join("fixture/tests.rs"), b"original").unwrap();
        std::fs::write(
            task_dir.join("task.json"),
            serde_json::json!({
                "name": "guarded",
                "prompt": "x",
                "test_cmd": "test -f done.txt",
                "protected": ["tests.rs"],
                "timeout_secs": 60,
                "tags": []
            })
            .to_string(),
        )
        .unwrap();

        let tasks = discover_tasks(dir.path(), &[]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = base_opts(out.path().to_path_buf());

        let runner = FakeRunner {
            outcome: RunOutcome::Ran("done".to_string()),
            writes: vec![
                ("done.txt".to_string(), b"ok".to_vec()),
                ("tests.rs".to_string(), b"tampered".to_vec()),
            ],
            wall: Duration::from_secs(1),
        };

        let result = run_one(&tasks[0], 1, &opts, &runner).await.unwrap();
        assert!(!result.landed, "{result:?}");
        assert_eq!(result.landed_reason, Some(NotLandedReason::Protected));
    }

    #[tokio::test]
    async fn fake_runner_failing_test_cmd_reports_not_landed_test_failed() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "failing",
            serde_json::json!({"test_cmd": "echo verify-said-no; test -f nope.txt"}),
        );
        let tasks = discover_tasks(dir.path(), &[]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = base_opts(out.path().to_path_buf());

        let runner = FakeRunner {
            outcome: RunOutcome::Ran("done".to_string()),
            writes: Vec::new(),
            wall: Duration::from_secs(1),
        };

        let result = run_one(&tasks[0], 1, &opts, &runner).await.unwrap();
        assert!(!result.landed);
        assert_eq!(result.landed_reason, Some(NotLandedReason::TestFailed));
        assert_eq!(
            result.verify_tail.as_deref().map(str::trim),
            Some("verify-said-no")
        );
        let persisted =
            std::fs::read_to_string(out.path().join("failing").join("seed1").join("verify.txt"))
                .unwrap();
        assert_eq!(persisted.trim(), "verify-said-no");
    }

    // -- metrics --

    fn patch_entry(step: u64, prompt_tokens: u64, completion_tokens: u64) -> LogEntry {
        LogEntry {
            step,
            kind: statie_core::LogKind::Patch,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload: serde_json::json!({
                "version": step,
                "patch": {},
                "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens}
            }),
            agent: None,
        }
    }

    #[test]
    fn server_prompt_token_stats_computes_max_mean_last() {
        let entries = vec![
            patch_entry(1, 100, 10),
            patch_entry(2, 300, 20),
            patch_entry(3, 200, 15),
        ];
        let stats = server_prompt_token_stats(&entries).unwrap();
        assert_eq!(stats.max, 300);
        // mean of 100,300,200 = 200
        assert_eq!(stats.mean, 200);
        assert_eq!(stats.last, 200);
    }

    #[test]
    fn server_prompt_token_stats_none_when_no_usage() {
        let entries = vec![LogEntry {
            step: 1,
            kind: statie_core::LogKind::Patch,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload: serde_json::json!({"version": 1, "patch": {}}),
            agent: None,
        }];
        assert!(server_prompt_token_stats(&entries).is_none());
    }

    #[test]
    fn parse_outcome_reads_final_summary_line() {
        let stdout = "some transcript\nmore lines\nsteps=4 outcome=done prompt_tokens=[1, 2]\n";
        assert_eq!(parse_outcome(stdout), Some("done".to_string()));
    }

    #[test]
    fn parse_outcome_none_when_absent() {
        let stdout = "crashed before summary\n";
        assert_eq!(parse_outcome(stdout), None);
    }

    #[tokio::test]
    async fn missing_summary_line_reports_crashed() {
        let dir = tempfile::tempdir().unwrap();
        write_task(dir.path(), "nosummary", serde_json::json!({}));
        let tasks = discover_tasks(dir.path(), &[]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = base_opts(out.path().to_path_buf());

        struct NoSummaryRunner;
        #[async_trait::async_trait]
        impl SessionRunner for NoSummaryRunner {
            async fn run_session(
                &self,
                _sandbox: &Path,
                _prompt: &str,
                _timeout: Duration,
            ) -> RunOutput {
                RunOutput {
                    outcome: RunOutcome::Crashed,
                    wall: Duration::from_secs(1),
                    stdout: "no summary here".to_string(),
                    stderr: "oops".to_string(),
                }
            }
        }

        let result = run_one(&tasks[0], 1, &opts, &NoSummaryRunner)
            .await
            .unwrap();
        assert_eq!(result.outcome, "crashed");
        assert!(!result.landed);
        assert_eq!(result.landed_reason, Some(NotLandedReason::NoRun));
    }

    // -- timeout --

    #[tokio::test]
    async fn runner_that_sleeps_past_timeout_reports_timeout_row() {
        let dir = tempfile::tempdir().unwrap();
        write_task(dir.path(), "slow", serde_json::json!({"timeout_secs": 1}));
        let tasks = discover_tasks(dir.path(), &[]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = base_opts(out.path().to_path_buf());

        struct SleepyRunner;
        #[async_trait::async_trait]
        impl SessionRunner for SleepyRunner {
            async fn run_session(
                &self,
                _sandbox: &Path,
                _prompt: &str,
                timeout: Duration,
            ) -> RunOutput {
                // Simulate the real runner's own timeout enforcement: sleep
                // past the deadline, then report Timeout, exactly as
                // `BinaryRunner` would after `tokio::time::timeout` fires.
                let start = Instant::now();
                tokio::time::sleep(timeout + Duration::from_millis(200)).await;
                RunOutput {
                    outcome: RunOutcome::Timeout,
                    wall: start.elapsed(),
                    stdout: String::new(),
                    stderr: String::new(),
                }
            }
        }

        let result = run_one(&tasks[0], 1, &opts, &SleepyRunner).await.unwrap();
        assert_eq!(result.outcome, "timeout");
        assert!(!result.landed);
    }

    // -- aggregation --

    #[test]
    fn summarize_task_computes_landed_median_mean() {
        let r1 = RunResult {
            task: "t".to_string(),
            seed: 1,
            outcome: "done".to_string(),
            landed: true,
            landed_reason: None,
            steps: 2,
            rejections: 0,
            tool_errors: 0,
            edits_landed: 1,
            green_checkpoints: 0,
            server_tokens: Some(ServerTokenStats {
                max: 100,
                mean: 100,
                last: 100,
            }),
            wall_secs: 2.0,
            verify_tail: None,
            stderr_tail: None,
        };
        let mut r2 = r1.clone();
        r2.seed = 2;
        r2.steps = 4;
        r2.landed = false;
        r2.landed_reason = Some(NotLandedReason::TestFailed);
        r2.server_tokens = Some(ServerTokenStats {
            max: 200,
            mean: 200,
            last: 200,
        });
        r2.wall_secs = 4.0;

        let refs = vec![&r1, &r2];
        let summary = summarize_task(&refs);
        assert_eq!(summary.landed, 1);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.median_steps, 3.0);
        assert_eq!(summary.mean_server_max, 150.0);
        assert_eq!(summary.mean_secs, 3.0);
        assert_eq!(summary.outcomes.get("done"), Some(&2));
    }
}
