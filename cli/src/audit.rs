//! `statie audit <run_a_dir> <run_b_dir>`: paired-run comparison for two
//! `statie bench` output directories (`bench-runs/<label>/<task>/seed<N>/`,
//! each containing `session.jsonl` -- the per-step [`statie_core::LogEntry`]
//! record `cli/src/bench.rs`'s `persist_run_artifacts` writes -- and
//! `stdout.txt`, the captured headless transcript).
//!
//! Prints, to stdout, plain text:
//! 1. One header line per run: task, seed, outcome, steps.
//! 2. The first step where the two runs' actions (tool name + primary
//!    argument) diverge.
//! 3. The first step where the two runs' harness state (`plan`, `files`,
//!    `decision`, `tests`, reconstructed by replaying each run's `Patch`
//!    entries through the real `CodingWorkspace` merge-patch strategy)
//!    diverges, field by field.
//! 4. A per-step action/prompt_tokens table for both runs, from the first
//!    divergence found (of either kind) up to 10 steps past it.
//!
//! Never panics: a missing file, unparseable record, or a section that
//! can't be computed from what the record actually carries prints `n/a` /
//! `section unavailable: <reason>` and the command still exits 0.

use std::path::{Path, PathBuf};

use serde_json::Value;
use statie_core::{LogEntry, LogKind, PatchStrategy, read_jsonl};
use statie_patch::{JsonMergePatch, MergePatch};
use statie_skills::CodingWorkspace;

const TABLE_STEPS_PAST_DIVERGENCE: u64 = 10;
const FIELD_TRUNCATE_CHARS: usize = 200;

/// One run's identity (task/seed, derived from its directory path) plus
/// whatever could be loaded from it.
struct RunData {
    label: String,
    task: String,
    seed: String,
    outcome: String,
    steps: String,
    entries: Option<Vec<LogEntry>>,
}

/// Derives `(task, seed)` from a run directory path shaped
/// `.../<task>/seed<N>/` (trailing slash tolerated). `n/a` for either piece
/// that doesn't fit the expected shape, rather than guessing.
fn task_and_seed(dir: &Path) -> (String, String) {
    let mut components: Vec<&str> = dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // Drop a trailing empty component from a trailing slash, if any (the
    // `Path` component iterator already ignores these, but stay defensive).
    while components.last().is_some_and(|c| c.is_empty()) {
        components.pop();
    }

    let seed_component = components.last().copied();
    let task_component = if components.len() >= 2 {
        Some(components[components.len() - 2])
    } else {
        None
    };

    let seed = seed_component
        .and_then(|c| c.strip_prefix("seed"))
        .filter(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()))
        .map(|n| n.to_string())
        .unwrap_or_else(|| "n/a".to_string());

    let task = task_component
        .filter(|_| seed != "n/a")
        .map(|c| c.to_string())
        .unwrap_or_else(|| "n/a".to_string());

    (task, seed)
}

/// Parses the last `steps=<N> outcome=<label> prompt_tokens=[...]` line in
/// `stdout.txt` (the headless summary `cli/src/main.rs` prints; see
/// `cli/src/bench.rs`'s `parse_outcome`, which parses the same line for
/// `outcome=`). Returns `("n/a", "n/a")` if no such line is present.
fn outcome_and_steps(stdout: &str) -> (String, String) {
    for line in stdout.lines().rev() {
        if let Some(idx) = line.find("outcome=") {
            let rest = &line[idx + "outcome=".len()..];
            let end = rest.find(' ').unwrap_or(rest.len());
            let outcome = &rest[..end];
            if outcome.is_empty() {
                continue;
            }
            let steps = line
                .find("steps=")
                .map(|si| &line[si + "steps=".len()..])
                .and_then(|rest| {
                    let end = rest.find(' ').unwrap_or(rest.len());
                    let candidate = &rest[..end];
                    (!candidate.is_empty()).then(|| candidate.to_string())
                })
                .unwrap_or_else(|| "n/a".to_string());
            return (outcome.to_string(), steps);
        }
    }
    ("n/a".to_string(), "n/a".to_string())
}

/// Loads one run directory: derives task/seed from the path, reads
/// `stdout.txt` for outcome/steps, and reads `session.jsonl` for entries.
/// Never errors -- pieces that can't be read/parsed become `n/a` /
/// `entries: None`.
fn load_run(label: &str, dir: &Path) -> RunData {
    let (task, seed) = task_and_seed(dir);

    let stdout = std::fs::read_to_string(dir.join("stdout.txt")).unwrap_or_default();
    let (outcome, steps) = outcome_and_steps(&stdout);

    let entries = read_jsonl(dir.join("session.jsonl")).ok();

    RunData {
        label: label.to_string(),
        task,
        seed,
        outcome,
        steps,
        entries,
    }
}

fn print_header(run: &RunData) {
    println!(
        "run {}: task={} seed={} outcome={} steps={}",
        run.label, run.task, run.seed, run.outcome, run.steps
    );
}

/// One `Action` log entry, reduced to `(tool name, primary argument)`.
/// `None` for a step with no tool-call action (e.g. a `done`/`yield`
/// action, or no action at all at that step).
fn tool_call_at_step(entries: &[LogEntry], step: u64) -> Option<(String, String)> {
    entries
        .iter()
        .find(|e| e.kind == LogKind::Action && e.step == step)
        .and_then(|e| {
            if e.payload.get("type").and_then(Value::as_str) != Some("tool") {
                return None;
            }
            let name = e
                .payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = e.payload.get("args").and_then(Value::as_object);
            let primary = args.and_then(primary_argument).unwrap_or_default();
            Some((name, primary))
        })
}

/// Picks the "primary" argument value out of a tool call's `args` object:
/// the first of a few common single-value keys (path/cmd/query/question/
/// code/pattern) that is present, rendered as its JSON scalar (strings
/// unquoted); otherwise the first key in the (BTreeMap-ordered, since this
/// workspace doesn't enable serde_json's `preserve_order`) map; `None` for
/// an empty args object.
fn primary_argument(args: &serde_json::Map<String, Value>) -> Option<String> {
    const PREFERRED_KEYS: &[&str] = &["path", "cmd", "query", "question", "code", "pattern"];
    for key in PREFERRED_KEYS {
        if let Some(v) = args.get(*key) {
            return Some(render_scalar(v));
        }
    }
    args.iter().next().map(|(_, v)| render_scalar(v))
}

fn render_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The highest `step` value across every entry, or `0` if there are none.
fn max_step(entries: &[LogEntry]) -> u64 {
    entries.iter().map(|e| e.step).max().unwrap_or(0)
}

/// Prints section 2 (first action divergence) and returns the diverging
/// step, if any and if both runs' entries were available.
fn print_action_divergence(a: &RunData, b: &RunData) -> Option<u64> {
    let (Some(entries_a), Some(entries_b)) = (&a.entries, &b.entries) else {
        println!(
            "action divergence: section unavailable: {}",
            missing_entries_reason(a, b)
        );
        return None;
    };

    let last_step = max_step(entries_a).max(max_step(entries_b));
    for step in 0..=last_step {
        let call_a = tool_call_at_step(entries_a, step);
        let call_b = tool_call_at_step(entries_b, step);
        if call_a != call_b {
            let render = |c: &Option<(String, String)>| match c {
                Some((name, arg)) if arg.is_empty() => name.clone(),
                Some((name, arg)) => format!("{name}({arg})"),
                None => "n/a".to_string(),
            };
            println!(
                "action divergence at step {step}: a={} b={}",
                render(&call_a),
                render(&call_b)
            );
            return Some(step);
        }
    }
    println!("action divergence: none found");
    None
}

fn missing_entries_reason(a: &RunData, b: &RunData) -> String {
    match (&a.entries, &b.entries) {
        (None, None) => format!(
            "could not read session.jsonl for {} or {}",
            a.label, b.label
        ),
        (None, Some(_)) => format!("could not read session.jsonl for {}", a.label),
        (Some(_), None) => format!("could not read session.jsonl for {}", b.label),
        (Some(_), Some(_)) => unreachable!("caller only invokes this when at least one is None"),
    }
}

/// Reconstructs `CodingWorkspace` state as of (and including) `step`, by
/// replaying every `Patch` entry with `step <= target` through the real
/// `JsonMergePatch<CodingWorkspace>` strategy (`statie-patch`) -- the same
/// merge (including the keyed-array merge for `plan`/`files`) the running
/// harness itself applies. Returns `Err(reason)` the first time a patch
/// fails to apply (e.g. the log belongs to a different, non-coding
/// workspace), so the caller can report that section unavailable rather
/// than silently showing a wrong state.
fn workspace_at_step(entries: &[LogEntry], target: u64) -> Result<CodingWorkspace, String> {
    let mut ws = CodingWorkspace::default();
    let mut patch_entries: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| e.kind == LogKind::Patch && e.step <= target)
        .collect();
    patch_entries.sort_by_key(|e| e.step);

    for entry in patch_entries {
        let Some(patch_value) = entry.payload.get("patch").cloned() else {
            continue;
        };
        let patch = MergePatch::<CodingWorkspace>::new(patch_value);
        ws = JsonMergePatch::<CodingWorkspace>::apply(&ws, &patch)
            .map_err(|e| format!("patch at step {} did not apply: {e}", entry.step))?;
    }
    Ok(ws)
}

fn truncate(s: &str, n: usize) -> String {
    let truncated: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn field_repr(label: &str, ws: &Result<CodingWorkspace, String>) -> String {
    match ws {
        Err(_) => "n/a".to_string(),
        Ok(ws) => {
            let value = match label {
                "plan" => serde_json::to_value(&ws.plan),
                "files" => serde_json::to_value(&ws.files),
                "decision" => serde_json::to_value(&ws.decision),
                "tests" => serde_json::to_value(&ws.tests),
                _ => unreachable!("only the four known fields are compared"),
            };
            match value {
                Ok(v) => truncate(&v.to_string(), FIELD_TRUNCATE_CHARS),
                Err(_) => "n/a".to_string(),
            }
        }
    }
}

/// Prints section 3 (first harness-state divergence) and returns the
/// diverging step, if any.
fn print_state_divergence(a: &RunData, b: &RunData) -> Option<u64> {
    let (Some(entries_a), Some(entries_b)) = (&a.entries, &b.entries) else {
        println!(
            "harness state divergence: section unavailable: {}",
            missing_entries_reason(a, b)
        );
        return None;
    };

    let last_step = max_step(entries_a).max(max_step(entries_b));
    const FIELDS: &[&str] = &["plan", "files", "decision", "tests"];

    for step in 0..=last_step {
        let ws_a = workspace_at_step(entries_a, step);
        let ws_b = workspace_at_step(entries_b, step);

        if let (Err(ea), Err(_)) = (&ws_a, &ws_b) {
            println!("harness state divergence: section unavailable: {ea}");
            return None;
        }

        for field in FIELDS {
            let repr_a = field_repr(field, &ws_a);
            let repr_b = field_repr(field, &ws_b);
            if repr_a != repr_b {
                println!(
                    "harness state divergence at step {step}, field {field}: a={repr_a} b={repr_b}"
                );
                return Some(step);
            }
        }
    }
    println!("harness state divergence: none found");
    None
}

/// Prints section 4: the per-step action/prompt_tokens table from
/// `start` (the first divergence found, of either kind) to `start + 10`
/// inclusive.
fn print_table(a: &RunData, b: &RunData, start: u64) {
    println!("step | action_a | action_b | prompt_tokens_a | prompt_tokens_b");
    let (Some(entries_a), Some(entries_b)) = (&a.entries, &b.entries) else {
        println!("n/a | n/a | n/a | n/a | n/a");
        return;
    };

    let end = start + TABLE_STEPS_PAST_DIVERGENCE;
    for step in start..=end {
        let action_a = render_action_cell(entries_a, step);
        let action_b = render_action_cell(entries_b, step);
        let tokens_a = prompt_tokens_at_step(entries_a, step);
        let tokens_b = prompt_tokens_at_step(entries_b, step);
        println!("{step} | {action_a} | {action_b} | {tokens_a} | {tokens_b}");
    }
}

fn render_action_cell(entries: &[LogEntry], step: u64) -> String {
    match tool_call_at_step(entries, step) {
        Some((name, arg)) if arg.is_empty() => name,
        Some((name, arg)) => format!("{name}({arg})"),
        None => "n/a".to_string(),
    }
}

/// `payload.usage.prompt_tokens` off the `Patch` entry at `step`, if any
/// (mirrors `cli/src/bench.rs`'s `server_prompt_token_stats`, but per-step
/// rather than reduced).
fn prompt_tokens_at_step(entries: &[LogEntry], step: u64) -> String {
    entries
        .iter()
        .find(|e| e.kind == LogKind::Patch && e.step == step)
        .and_then(|e| e.payload.get("usage")?.get("prompt_tokens")?.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

/// Entry point for `statie audit <run_a_dir> <run_b_dir>`. Never returns an
/// error for a missing/unparseable run: those are reported as `n/a` inline
/// and the command still exits 0 (per spec).
pub fn cmd_audit(run_a_dir: PathBuf, run_b_dir: PathBuf) -> anyhow::Result<()> {
    let a = load_run("a", &run_a_dir);
    let b = load_run("b", &run_b_dir);

    print_header(&a);
    print_header(&b);

    let action_step = print_action_divergence(&a, &b);
    let state_step = print_state_divergence(&a, &b);

    let table_start = match (action_step, state_step) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };

    match table_start {
        Some(start) => print_table(&a, &b, start),
        None => println!("per-step table: section unavailable: no divergence found"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(step: u64, kind: LogKind, payload: Value) -> LogEntry {
        LogEntry {
            step,
            kind,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload,
            agent: None,
        }
    }

    #[test]
    fn task_and_seed_parses_expected_shape() {
        let (task, seed) = task_and_seed(Path::new("bench-runs/label/my_task/seed2"));
        assert_eq!(task, "my_task");
        assert_eq!(seed, "2");
    }

    #[test]
    fn task_and_seed_falls_back_to_na_for_unexpected_shape() {
        let (task, seed) = task_and_seed(Path::new("some/random/dir"));
        assert_eq!(task, "n/a");
        assert_eq!(seed, "n/a");
    }

    #[test]
    fn outcome_and_steps_parses_summary_line() {
        let stdout = "some preamble\nsteps=29 outcome=done prompt_tokens=[1, 2, 3]\n";
        let (outcome, steps) = outcome_and_steps(stdout);
        assert_eq!(outcome, "done");
        assert_eq!(steps, "29");
    }

    #[test]
    fn outcome_and_steps_na_when_absent() {
        let (outcome, steps) = outcome_and_steps("nothing relevant here\n");
        assert_eq!(outcome, "n/a");
        assert_eq!(steps, "n/a");
    }

    #[test]
    fn primary_argument_prefers_path() {
        let args = serde_json::json!({"path": "src/lib.rs", "offset": 1})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(primary_argument(&args), Some("src/lib.rs".to_string()));
    }

    #[test]
    fn tool_call_at_step_reads_name_and_primary_arg() {
        let entries = vec![entry(
            5,
            LogKind::Action,
            serde_json::json!({"type": "tool", "name": "shell", "args": {"cmd": "cargo test"}}),
        )];
        let call = tool_call_at_step(&entries, 5);
        assert_eq!(call, Some(("shell".to_string(), "cargo test".to_string())));
    }

    #[test]
    fn workspace_at_step_replays_patches() {
        let entries = vec![
            entry(
                1,
                LogKind::Patch,
                serde_json::json!({"version": 1, "patch": {"task": "fix bug"}}),
            ),
            entry(
                2,
                LogKind::Patch,
                serde_json::json!({"version": 2, "patch": {"plan": [{"id": 1, "text": "step one"}]}}),
            ),
        ];
        let ws = workspace_at_step(&entries, 2).expect("should apply");
        assert_eq!(ws.task, "fix bug");
        assert_eq!(ws.plan.len(), 1);
        assert_eq!(ws.plan[0].text, "step one");

        // At step 1, the plan patch (step 2) hasn't been applied yet.
        let ws_before = workspace_at_step(&entries, 1).expect("should apply");
        assert!(ws_before.plan.is_empty());
    }
}
