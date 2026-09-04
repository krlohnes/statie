//! `statie metrics [PATH]`: a read-only summary of one session log
//! (JSONL, the same file the running session writes -- `Config::session_log`
//! / `statie-store`'s `JsonlLog`). Also home to the `LogMetrics` computation
//! shared with `eval.rs`'s harness comparison table, so both count
//! "committed steps"/"landed edit"/etc. the same way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use statie_core::{LogEntry, LogKind, SessionLog, read_jsonl};

/// Non-progress and maintenance metrics computed from one run's
/// `SessionLog` (statie-spec.md §9 "Done when", third bullet): how much of
/// the run was rejections/errors rather than forward motion, whether edits
/// actually landed, how long it took to land the first one, and how much
/// (and how reliably) the context maintainer ran, plus the redundant-call
/// guards and green-checkpoint activity added for `statie metrics`.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct LogMetrics {
    /// System entries with `event == "rejected"` (a model step rejected
    /// before commit -- schema/harness-owned-field/stale-version, etc.).
    pub rejections: usize,
    /// System entries with `event == "failure"` (retry cap hit).
    pub failures: usize,
    /// System entries with `event == "tool_error"`.
    pub tool_errors: usize,
    /// Action entries that are tool calls named `edit_file` or
    /// `write_file`.
    pub edits_attempted: usize,
    /// Of `edits_attempted`, those whose step has no `tool_error` System
    /// entry at the same step (matched on `LogEntry::step`).
    pub edits_landed: usize,
    /// 1-based ordinal (among distinct `step` values of Action entries, in
    /// order) of the model step at which the first landed edit happened;
    /// `None` if no edit landed.
    pub steps_to_first_edit: Option<usize>,
    /// System entries with `event == "maintainer"` (a successful
    /// context-maintenance run).
    pub maintainer_runs: usize,
    /// System entries with `event == "maintainer_error"`.
    pub maintainer_errors: usize,
    /// System entries with `event == "skipped_redundant"` (7.7/7.8's
    /// redundant-verification / redundant-identical-read guards).
    pub skipped_redundant: usize,
    /// System entries with `event == "paged_read"` (7.8's read-paging
    /// special case).
    pub paged_reads: usize,
    /// System entries with `event == "continued"` (a skill's `validate`
    /// override converted a doomed-to-repeat step into `Action::Continue`).
    pub continued: usize,
    /// System entries with `event == "green_checkpoint"`.
    pub green_checkpoints: usize,
    /// System entries with `event == "restored_green"`.
    pub restored_green: usize,
}

/// One `(reason, count)` or `(tool, reason, count)` bucket in a breakdown,
/// sorted by count descending (ties broken by first-seen order).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReasonCount {
    pub count: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolReasonCount {
    pub count: usize,
    pub tool: String,
    pub reason: String,
}

/// Truncates `s` to at most `n` `char`s (not bytes), for the "first 60
/// chars" breakdown keys.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

const BREAKDOWN_REASON_CHARS: usize = 60;

/// Computes [`LogMetrics`] from a run's full `SessionLog` history. Shared by
/// `eval.rs` (the statie-vs-baseline harness table) and `statie metrics`, so
/// both agree on exactly what counts as a "committed step" / "landed edit".
pub fn log_metrics(log: &dyn SessionLog) -> LogMetrics {
    log_metrics_from_entries(&log.entries(None, usize::MAX))
}

/// Same as [`log_metrics`], computed directly over a slice of entries (e.g.
/// read from a JSONL file with [`statie_core::read_jsonl`], with no
/// `SessionLog` in play).
pub fn log_metrics_from_entries(entries: &[LogEntry]) -> LogMetrics {
    let count_event = |event: &str| -> usize {
        entries
            .iter()
            .filter(|e| {
                e.kind == LogKind::System
                    && e.payload.get("event").and_then(|v| v.as_str()) == Some(event)
            })
            .count()
    };

    let rejections = count_event("rejected");
    let failures = count_event("failure");
    let tool_errors = count_event("tool_error");
    let maintainer_runs = count_event("maintainer");
    let maintainer_errors = count_event("maintainer_error");
    let skipped_redundant = count_event("skipped_redundant");
    let paged_reads = count_event("paged_read");
    let continued = count_event("continued");
    let green_checkpoints = count_event("green_checkpoint");
    let restored_green = count_event("restored_green");

    // Steps (in order) whose System entries include a `tool_error`.
    let error_steps: std::collections::HashSet<u64> = entries
        .iter()
        .filter(|e| {
            e.kind == LogKind::System
                && e.payload.get("event").and_then(|v| v.as_str()) == Some("tool_error")
        })
        .map(|e| e.step)
        .collect();

    // Distinct `step` values of Action entries, in first-seen order, to
    // derive the 1-based model-step ordinal ("committed steps").
    let mut step_order: Vec<u64> = Vec::new();
    for e in entries.iter().filter(|e| e.kind == LogKind::Action) {
        if !step_order.contains(&e.step) {
            step_order.push(e.step);
        }
    }

    let mut edits_attempted = 0usize;
    let mut edits_landed = 0usize;
    let mut steps_to_first_edit = None;
    for e in entries.iter().filter(|e| e.kind == LogKind::Action) {
        let is_edit = e.payload.get("type").and_then(|v| v.as_str()) == Some("tool")
            && matches!(
                e.payload.get("name").and_then(|v| v.as_str()),
                Some("edit_file") | Some("write_file")
            );
        if !is_edit {
            continue;
        }
        edits_attempted += 1;
        if !error_steps.contains(&e.step) {
            edits_landed += 1;
            if steps_to_first_edit.is_none() {
                let ordinal = step_order.iter().position(|s| *s == e.step).map(|i| i + 1);
                steps_to_first_edit = ordinal;
            }
        }
    }

    LogMetrics {
        rejections,
        failures,
        tool_errors,
        edits_attempted,
        edits_landed,
        steps_to_first_edit,
        maintainer_runs,
        maintainer_errors,
        skipped_redundant,
        paged_reads,
        continued,
        green_checkpoints,
        restored_green,
    }
}

/// "Committed steps": the number of distinct `step` values carried by an
/// Action entry, or by a Patch entry whose `source` is the model's own
/// (i.e. absent, since only harness/maintainer/user/subagent-sourced
/// patches set `source` -- see `core/src/session.rs`'s `Active::step`/
/// `apply_trusted_patch`). In practice every model-committed step logs both
/// an Action and an unsourced Patch entry at the same `step`, so counting
/// distinct Action-entry steps (the same derivation `log_metrics`'s
/// `step_order` already uses for `steps_to_first_edit`) is equivalent and
/// simpler.
pub fn committed_steps(entries: &[LogEntry]) -> usize {
    let mut steps: Vec<u64> = Vec::new();
    for e in entries {
        let is_model_step = e.kind == LogKind::Action
            || (e.kind == LogKind::Patch && e.payload.get("source").is_none());
        if is_model_step && !steps.contains(&e.step) {
            steps.push(e.step);
        }
    }
    steps.len()
}

/// Ordered list of skill names from `System` entries with
/// `event == "activate"`, in log order (not deduplicated -- a resumed or
/// handed-off session activates the same skill more than once, and each
/// activation is a real transition worth showing).
pub fn activations(entries: &[LogEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.kind == LogKind::System)
        .filter_map(|e| {
            if e.payload.get("event").and_then(|v| v.as_str()) != Some("activate") {
                return None;
            }
            e.payload
                .get("skill")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The `event` field of the last `System` entry in the log, or `None` if the
/// log has no `System` entries. `statie metrics` prints this as `last=`,
/// the simplest log-derivable stand-in for "what did the session do last"
/// when no live `Outcome`/`Park` is available (those are session-loop types,
/// not logged verbatim).
pub fn last_outcome(entries: &[LogEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| e.kind == LogKind::System)
        .and_then(|e| e.payload.get("event").and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// Rejections grouped by `reason` (truncated to 60 chars), sorted by count
/// descending, ties broken by first-seen order.
pub fn rejections_by_reason(entries: &[LogEntry]) -> Vec<ReasonCount> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in entries {
        if e.kind != LogKind::System {
            continue;
        }
        if e.payload.get("event").and_then(|v| v.as_str()) != Some("rejected") {
            continue;
        }
        let reason = e
            .payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let key = truncate_chars(reason, BREAKDOWN_REASON_CHARS);
        if !counts.contains_key(&key) {
            order.push(key.clone());
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut out: Vec<ReasonCount> = order
        .into_iter()
        .map(|reason| {
            let count = counts[&reason];
            ReasonCount { count, reason }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

/// Tool errors grouped by `(tool, reason)` (reason truncated to 60 chars),
/// sorted by count descending, ties broken by first-seen order.
pub fn tool_errors_by_tool_and_reason(entries: &[LogEntry]) -> Vec<ToolReasonCount> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for e in entries {
        if e.kind != LogKind::System {
            continue;
        }
        if e.payload.get("event").and_then(|v| v.as_str()) != Some("tool_error") {
            continue;
        }
        let tool = e
            .payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reason = e
            .payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let key = (tool, truncate_chars(reason, BREAKDOWN_REASON_CHARS));
        if !counts.contains_key(&key) {
            order.push(key.clone());
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut out: Vec<ToolReasonCount> = order
        .into_iter()
        .map(|(tool, reason)| {
            let count = counts[&(tool.clone(), reason.clone())];
            ToolReasonCount {
                count,
                tool,
                reason,
            }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

/// Full summary for `statie metrics`: the log-derived numbers plus the two
/// breakdowns, ready to print as the summary line + breakdown lists, or
/// serialize as JSON with `--json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSummary {
    pub steps: usize,
    pub log: LogMetrics,
    pub activations: Vec<String>,
    pub last: Option<String>,
    pub rejections_by_reason: Vec<ReasonCount>,
    pub tool_errors_by_tool_and_reason: Vec<ToolReasonCount>,
}

impl MetricsSummary {
    pub fn from_entries(entries: &[LogEntry]) -> Self {
        MetricsSummary {
            steps: committed_steps(entries),
            log: log_metrics_from_entries(entries),
            activations: activations(entries),
            last: last_outcome(entries),
            rejections_by_reason: rejections_by_reason(entries),
            tool_errors_by_tool_and_reason: tool_errors_by_tool_and_reason(entries),
        }
    }
}

/// The one-line summary: `steps=... rej=... ... last=<label or ->`.
fn format_summary_line(s: &MetricsSummary) -> String {
    let first_edit = match s.log.steps_to_first_edit {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    };
    let activations = s.activations.join(",");
    let last = s.last.as_deref().unwrap_or("-");
    format!(
        "steps={} rej={} failures={} tool_err={} skipped={} paged={} continued={} edits={} \
         first_edit={} green={} restored={} maint={} maint_err={} activations={} last={}",
        s.steps,
        s.log.rejections,
        s.log.failures,
        s.log.tool_errors,
        s.log.skipped_redundant,
        s.log.paged_reads,
        s.log.continued,
        s.log.edits_landed,
        first_edit,
        s.log.green_checkpoints,
        s.log.restored_green,
        s.log.maintainer_runs,
        s.log.maintainer_errors,
        activations,
        last,
    )
}

/// Prints the plain-text summary line and both breakdowns (sorted by count
/// descending, reasons truncated to 60 chars) to stdout.
fn print_text(s: &MetricsSummary) {
    println!("{}", format_summary_line(s));

    println!("rejections by reason (first 60 chars):");
    for rc in &s.rejections_by_reason {
        println!("  {}  {}", rc.count, rc.reason);
    }

    println!("tool errors by tool and reason (first 60 chars):");
    for tc in &s.tool_errors_by_tool_and_reason {
        println!("  {}  {}: {}", tc.count, tc.tool, tc.reason);
    }
}

/// `statie metrics [PATH] [--json]`. `PATH` defaults to
/// `.statie/session.jsonl` under the current directory -- the same file the
/// running session writes (`Config::session_log`'s default). A missing file
/// is a clear, non-zero-exit error rather than a silently-empty summary.
pub fn cmd_metrics(path: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(default_session_log_path);
    if !path.exists() {
        anyhow::bail!("no session log at {}", path.display());
    }
    let entries = read_jsonl(&path)
        .map_err(|e| anyhow::anyhow!("failed to read session log {}: {e}", path.display()))?;

    let summary = MetricsSummary::from_entries(&entries);

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_text(&summary);
    }

    Ok(())
}

/// `.statie/session.jsonl` under the current directory -- matches
/// `Config::default()`'s `session_log` resolved against the default
/// `sandbox_root` (`.`), without needing a full `Config` load (`statie
/// metrics` is read-only tooling and should work with no config file
/// present).
fn default_session_log_path() -> PathBuf {
    Path::new(".statie/session.jsonl").to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(step: u64, name: &str) -> LogEntry {
        LogEntry {
            step,
            kind: LogKind::Action,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload: serde_json::json!({"type": "tool", "name": name, "args": {}}),
            agent: None,
        }
    }

    fn system(step: u64, event: &str, mut extra: serde_json::Value) -> LogEntry {
        let mut payload = serde_json::json!({"event": event});
        if let (serde_json::Value::Object(map), serde_json::Value::Object(extra_map)) =
            (&mut payload, &mut extra)
        {
            map.append(extra_map);
        }
        LogEntry {
            step,
            kind: LogKind::System,
            ts: "2026-09-01T00:00:00Z".to_string(),
            payload,
            agent: None,
        }
    }

    fn activate(step: u64, skill: &str) -> LogEntry {
        system(step, "activate", serde_json::json!({"skill": skill}))
    }

    /// A varied scenario exercising every counted field: two activations
    /// (router then implement), two reads, one rejection, one failed edit
    /// (tool_error), a skipped-redundant call, a paged read, a continued
    /// step, a maintainer run + error, a green checkpoint, a restore, and
    /// finally a landed edit -- with two distinct rejection reasons (one
    /// repeated) and two distinct tool errors (one repeated) to exercise
    /// the breakdowns' sort-by-count-descending.
    fn build_scenario() -> Vec<LogEntry> {
        vec![
            activate(0, "router"),
            action(1, "read_file"),
            action(2, "read_file"),
            system(3, "rejected", serde_json::json!({"reason": "bad json"})),
            system(4, "rejected", serde_json::json!({"reason": "bad json"})),
            system(
                5,
                "rejected",
                serde_json::json!({"reason": "stale version"}),
            ),
            activate(5, "implement"),
            action(6, "edit_file"),
            system(
                6,
                "tool_error",
                serde_json::json!({"name": "edit_file", "reason": "no such anchor"}),
            ),
            action(7, "shell"),
            system(
                7,
                "skipped_redundant",
                serde_json::json!({"name": "shell", "reason": "same command"}),
            ),
            action(8, "read_file"),
            system(8, "paged_read", serde_json::json!({"path": "a.rs"})),
            system(9, "continued", serde_json::json!({"note": "try again"})),
            system(10, "maintainer", serde_json::json!({})),
            system(
                11,
                "maintainer_error",
                serde_json::json!({"reason": "boom"}),
            ),
            action(12, "edit_file"),
            system(
                12,
                "tool_error",
                serde_json::json!({"name": "edit_file", "reason": "no such anchor"}),
            ),
            action(13, "test"),
            system(
                13,
                "tool_error",
                serde_json::json!({"name": "shell", "reason": "timeout"}),
            ),
            system(14, "green_checkpoint", serde_json::json!({})),
            system(15, "restored_green", serde_json::json!({})),
            action(16, "write_file"),
        ]
    }

    #[test]
    fn log_metrics_from_entries_scenario() {
        let entries = build_scenario();
        let m = log_metrics_from_entries(&entries);

        assert_eq!(m.rejections, 3);
        assert_eq!(m.failures, 0);
        assert_eq!(m.tool_errors, 3);
        assert_eq!(m.edits_attempted, 3);
        assert_eq!(m.edits_landed, 1);
        assert_eq!(m.maintainer_runs, 1);
        assert_eq!(m.maintainer_errors, 1);
        assert_eq!(m.skipped_redundant, 1);
        assert_eq!(m.paged_reads, 1);
        assert_eq!(m.continued, 1);
        assert_eq!(m.green_checkpoints, 1);
        assert_eq!(m.restored_green, 1);
    }

    #[test]
    fn committed_steps_counts_distinct_action_steps() {
        let entries = build_scenario();
        // Distinct Action-entry steps: 1,2,6,7,8,12,13,16 -> 8.
        assert_eq!(committed_steps(&entries), 8);
    }

    #[test]
    fn activations_lists_skills_in_log_order() {
        let entries = build_scenario();
        assert_eq!(
            activations(&entries),
            vec!["router".to_string(), "implement".to_string()]
        );
    }

    #[test]
    fn last_outcome_is_last_system_event() {
        let entries = build_scenario();
        // The scenario's final entry is an Action (`write_file`, step 16),
        // so the last *System* event is the `restored_green` before it.
        assert_eq!(last_outcome(&entries), Some("restored_green".to_string()));
    }

    #[test]
    fn last_outcome_none_when_no_system_entries() {
        let entries = vec![action(1, "read_file")];
        assert_eq!(last_outcome(&entries), None);
    }

    #[test]
    fn rejections_by_reason_sorted_by_count_descending() {
        let entries = build_scenario();
        let breakdown = rejections_by_reason(&entries);
        assert_eq!(
            breakdown,
            vec![
                ReasonCount {
                    count: 2,
                    reason: "bad json".to_string()
                },
                ReasonCount {
                    count: 1,
                    reason: "stale version".to_string()
                },
            ]
        );
    }

    #[test]
    fn tool_errors_by_tool_and_reason_sorted_by_count_descending() {
        let entries = build_scenario();
        let breakdown = tool_errors_by_tool_and_reason(&entries);
        assert_eq!(
            breakdown,
            vec![
                ToolReasonCount {
                    count: 2,
                    tool: "edit_file".to_string(),
                    reason: "no such anchor".to_string()
                },
                ToolReasonCount {
                    count: 1,
                    tool: "shell".to_string(),
                    reason: "timeout".to_string()
                },
            ]
        );
    }

    #[test]
    fn breakdown_reason_truncated_to_60_chars() {
        let long_reason = "x".repeat(200);
        let entries = vec![system(
            1,
            "rejected",
            serde_json::json!({"reason": long_reason}),
        )];
        let breakdown = rejections_by_reason(&entries);
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].reason.chars().count(), 60);
    }

    #[test]
    fn metrics_summary_matches_all_fields() {
        let entries = build_scenario();
        let s = MetricsSummary::from_entries(&entries);
        assert_eq!(s.steps, 8);
        assert_eq!(s.log.rejections, 3);
        assert_eq!(
            s.activations,
            vec!["router".to_string(), "implement".to_string()]
        );
        assert_eq!(s.rejections_by_reason.len(), 2);
        assert_eq!(s.tool_errors_by_tool_and_reason.len(), 2);
    }

    #[test]
    fn empty_log_is_all_zeros() {
        let m = log_metrics_from_entries(&[]);
        assert_eq!(m, LogMetrics::default());
        assert_eq!(committed_steps(&[]), 0);
        assert_eq!(activations(&[]), Vec::<String>::new());
        assert_eq!(last_outcome(&[]), None);
    }

    #[test]
    fn format_summary_line_matches_expected_shape() {
        let entries = build_scenario();
        let s = MetricsSummary::from_entries(&entries);
        let line = format_summary_line(&s);
        assert!(line.starts_with(
            "steps=8 rej=3 failures=0 tool_err=3 skipped=1 paged=1 continued=1 edits=1 first_edit="
        ));
        assert!(line.contains("activations=router,implement"));
        assert!(line.ends_with(&format!("last={}", s.last.unwrap())));
    }
}
