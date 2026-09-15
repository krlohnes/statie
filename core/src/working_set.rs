//! The working set (statie-spec.md "10. Working set (recent observations)"):
//! a derived, budget-bounded view of what the model just did and read,
//! rebuilt from the session log every step (C2: derived, never persisted).
//!
//! Windows come in two kinds: read/search windows (`read_file`/`search`
//! outputs) and edit windows (successful `edit_file`/`write_file` outputs).
//! An edit window on path P replaces every earlier window on P, read or
//! edit; a read/search window on P made after the newest edit of P is kept
//! alongside it. See `WorkingSet::from_log` for the derivation.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::log::{LogKind, SessionLog};
use crate::promptable::{Promptable, sealed};
use crate::tokenizer::Tokenizer;

/// A large-enough window to see effectively the whole log for one
/// activation; `SessionLog::entries` takes the most recent `limit` of a
/// kind, oldest first, so this just needs to be bigger than any real
/// session gets in practice.
const LOG_SCAN_LIMIT: usize = 1_000_000;

/// Cap on one recent-calls line's rendered length.
const RECENT_CALL_LINE_CAP: usize = 160;

/// `read_file`'s default `offset`/`limit`, used to render a window header's
/// line range when the args omit them.
const DEFAULT_READ_OFFSET: u64 = 1;
const DEFAULT_READ_LIMIT: u64 = 100;

/// Configuration for `WorkingSet::from_log`. `Default` is "on": 4 windows,
/// 6 recent-call lines. `max_windows: 0` (with `recent_calls: 0`) disables
/// the section and restores pre-working-set behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkingSetConfig {
    pub max_windows: usize,
    pub recent_calls: usize,
}

impl Default for WorkingSetConfig {
    fn default() -> Self {
        WorkingSetConfig {
            max_windows: 4,
            recent_calls: 6,
        }
    }
}

/// The working-set prompt section: the last few executed tool calls
/// (one line each) plus the newest distinct windows, rendered verbatim
/// under a header naming the tool, path, range (`read_file` only), and
/// step. A window is either a `read_file`/`search` output, or a successful
/// `edit_file`/`write_file` output (an "edit window", keyed by path, that
/// replaces every earlier window -- read/search or edit -- on that path).
/// Sealed `Promptable` like `Notes`; only `from_log` produces one.
#[derive(Clone, Debug)]
pub struct WorkingSet {
    text: String,
    /// The step of each window actually rendered into `text` (the newest
    /// `cfg.max_windows` distinct windows selected by `from_log`, after
    /// edit-window replacement). Used by `Session`'s redundant-read guard
    /// (item 19) to tell whether an earlier read's window is still visible
    /// to the model or has been evicted.
    window_steps: BTreeSet<u64>,
}

impl WorkingSet {
    pub(crate) fn new(s: String, window_steps: BTreeSet<u64>) -> Self {
        WorkingSet {
            text: s,
            window_steps,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Steps whose windows this working set actually rendered (see the
    /// `window_steps` field doc). Empty when `from_log` returned no windows
    /// (recent-calls-only, or `None` entirely).
    pub fn window_steps(&self) -> &BTreeSet<u64> {
        &self.window_steps
    }

    /// Builds the working set from the session log's own (non-agent)
    /// entries: the last `cfg.recent_calls` executed tool calls as one-line
    /// summaries, and the newest `cfg.max_windows` distinct windows
    /// rendered verbatim. A window is either a `read_file`/`search` output,
    /// or a successful `edit_file`/`write_file` output (an "edit window").
    /// Both kinds are keyed by path: a later window on path P (read, search,
    /// or edit) replaces an earlier read/search window on P, and a later
    /// edit window on P replaces every earlier window on P (read/search or
    /// edit) up to and including that edit; a read/search window made after
    /// the newest edit of P is kept alongside it. `None` when there is
    /// nothing to show, or when `cfg.max_windows == 0 && cfg.recent_calls
    /// == 0`.
    pub fn from_log(
        log: &dyn SessionLog,
        tokenizer: &dyn Tokenizer,
        budget: usize,
        cfg: &WorkingSetConfig,
        current_step: u64,
    ) -> Option<WorkingSet> {
        if cfg.max_windows == 0 && cfg.recent_calls == 0 {
            return None;
        }

        let actions = own_entries(log, LogKind::Action);
        let tool_outputs = own_entries(log, LogKind::ToolOutput);
        let system = own_entries(log, LogKind::System);

        // Per-step lookups. Multiple entries can share a step (e.g. a Patch
        // and an Action), but at most one Action/ToolOutput/tool_error/
        // skipped_redundant System entry exists per step in practice, so
        // "first match wins" is fine either way.
        let action_by_step: HashMap<u64, &Value> = actions
            .iter()
            .filter(|e| e.payload.get("type").and_then(Value::as_str) == Some("tool"))
            .map(|e| (e.step, &e.payload))
            .collect();
        let tool_output_by_step: HashMap<u64, &Value> =
            tool_outputs.iter().map(|e| (e.step, &e.payload)).collect();
        let tool_error_by_step: HashMap<u64, &Value> = system
            .iter()
            .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("tool_error"))
            .map(|e| (e.step, &e.payload))
            .collect();
        let skipped_by_step: HashMap<u64, &Value> = system
            .iter()
            .filter(|e| e.payload.get("event").and_then(Value::as_str) == Some("skipped_redundant"))
            .map(|e| (e.step, &e.payload))
            .collect();

        // --- Recent calls: last `cfg.recent_calls` distinct steps with a
        // tool Action, oldest first. ---
        let mut tool_steps: Vec<u64> = action_by_step.keys().copied().collect();
        tool_steps.sort_unstable();
        let recent_start = tool_steps.len().saturating_sub(cfg.recent_calls);
        let recent_steps = &tool_steps[recent_start..];

        let mut recent_lines: Vec<String> = Vec::new();
        for &step in recent_steps {
            let payload = action_by_step[&step];
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
            let args = payload.get("args").cloned().unwrap_or(Value::Null);
            let args_str = args.to_string();

            let arrow = if let Some(err) = tool_error_by_step.get(&step) {
                let reason = err.get("reason").and_then(Value::as_str).unwrap_or("");
                format!("error: {}", first_line(reason))
            } else if skipped_by_step.contains_key(&step) {
                "skipped (redundant)".to_string()
            } else if let Some(out) = tool_output_by_step.get(&step) {
                let output = out.get("output").and_then(Value::as_str).unwrap_or("");
                first_non_empty_line(output).to_string()
            } else {
                String::new()
            };

            let line = format!("step {step}: {name} {args_str} -> {arrow}");
            recent_lines.push(cap_line(&line, RECENT_CALL_LINE_CAP));
        }

        // --- Windows: read_file/search ToolOutput entries (read/search
        // windows) plus successful edit_file/write_file ToolOutput entries
        // (edit windows -- a "successful" edit is one with no `tool_error`
        // System entry at that step), newest first, deduped by
        // "{name} {args}", newest kept. ---
        let mut window_outputs: Vec<&crate::log::LogEntry> = tool_outputs
            .iter()
            .filter(|e| {
                let Some(name) = action_by_step
                    .get(&e.step)
                    .and_then(|p| p.get("name").and_then(Value::as_str))
                else {
                    return false;
                };
                match name {
                    "read_file" | "search" => true,
                    "edit_file" | "write_file" => !tool_error_by_step.contains_key(&e.step),
                    _ => false,
                }
            })
            .collect();
        window_outputs.sort_by(|a, b| b.step.cmp(&a.step)); // newest first

        // The single newest ToolOutput entry overall (across all tools) is
        // this step's observation, already shown in the Observation
        // section; never repeat it in a window. Bounded by `current_step`
        // defensively, since the working set is built from committed
        // history up to (and including) the step just taken.
        let newest_output_step = tool_outputs
            .iter()
            .map(|e| e.step)
            .filter(|&s| s <= current_step)
            .max();

        // Steps at which a later *successful* edit_file/write_file touched a
        // given path: for each such mutating action, the path it names. A
        // failed edit (a `tool_error` System entry at that step) never
        // drops a window (rule 6), so it is excluded here.
        let mutating_paths_after: Vec<(u64, String)> = actions
            .iter()
            .filter(|e| {
                matches!(
                    e.payload.get("name").and_then(Value::as_str),
                    Some("edit_file") | Some("write_file")
                ) && !tool_error_by_step.contains_key(&e.step)
            })
            .filter_map(|e| {
                e.payload
                    .get("args")
                    .and_then(|a| a.get("path"))
                    .and_then(Value::as_str)
                    .map(|p| (e.step, p.to_string()))
            })
            .collect();

        let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut windows: Vec<(u64, String, String, Value)> = Vec::new(); // (step, name, path, args)
        for entry in window_outputs {
            if Some(entry.step) == newest_output_step {
                continue;
            }
            let Some(action_payload) = action_by_step.get(&entry.step) else {
                continue;
            };
            let name = action_payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = action_payload.get("args").cloned().unwrap_or(Value::Null);
            let key = format!("{name} {args}");
            if !seen_keys.insert(key) {
                continue;
            }

            let path = args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Drop a window (read/search or an earlier edit) on path P if a
            // later (higher-step) successful edit_file/write_file touched
            // P. The newest edit of P survives this check since it can
            // never be later than itself; a read/search window made after
            // the newest edit of P also survives, since nothing edits P
            // after it.
            let edited_later = mutating_paths_after
                .iter()
                .any(|(mstep, mpath)| *mstep > entry.step && mpath == &path);
            if edited_later {
                continue;
            }

            windows.push((entry.step, name, path, args));
            if windows.len() >= cfg.max_windows {
                break;
            }
        }

        if recent_lines.is_empty() && windows.is_empty() {
            return None;
        }

        // Render each window's body text once, so fitting can drop/truncate
        // by rendered size without re-rendering. Kept paired with its step
        // so the surviving set after budget-fitting (`fit_to_budget`) can
        // report exactly which windows made it into the final text.
        let mut rendered_windows: Vec<(u64, String)> = Vec::with_capacity(windows.len());
        for (step, name, path, args) in &windows {
            let output = tool_output_by_step
                .get(step)
                .and_then(|p| p.get("output"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let range = if name == "read_file" {
                let offset = args
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_READ_OFFSET);
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_READ_LIMIT);
                let end = offset + limit.saturating_sub(1);
                format!(" lines {offset}-{end}")
            } else {
                // search, edit_file, write_file: no line range -- the tool
                // output already carries its own line numbers for edits.
                String::new()
            };
            let header = format!("--- {name} {path}{range} (step {step}) ---");
            rendered_windows.push((*step, format!("{header}\n{output}")));
        }

        fit_to_budget(recent_lines, rendered_windows, budget, tokenizer)
    }
}

/// Fits the recent-calls lines and rendered windows (newest-first order,
/// matching `windows`/`rendered_windows` above) to `budget`: drop the
/// oldest window (whole) until the rest fits; once down to a single
/// window, that window is head-truncated with a marker (rather than
/// dropped) so it alone still fits; if no windows remain at all and it
/// still overflows, cut recent-call lines oldest-first.
fn fit_to_budget(
    recent_lines: Vec<String>,
    mut rendered_windows: Vec<(u64, String)>,
    budget: usize,
    tokenizer: &dyn Tokenizer,
) -> Option<WorkingSet> {
    // Drop the oldest window (whole) while more than one remains and it
    // still doesn't fit.
    while rendered_windows.len() > 1 {
        let bodies: Vec<&str> = rendered_windows.iter().map(|(_, s)| s.as_str()).collect();
        let text = render(&recent_lines, &bodies);
        if tokenizer.count(&text) <= budget {
            return Some(WorkingSet::new(text, window_steps_of(&rendered_windows)));
        }
        rendered_windows.pop();
    }

    // Zero or one window left. If it fits as-is (with the recent-call
    // lines), we're done.
    let bodies: Vec<&str> = rendered_windows.iter().map(|(_, s)| s.as_str()).collect();
    let text = render(&recent_lines, &bodies);
    if !text.is_empty() && tokenizer.count(&text) <= budget {
        return Some(WorkingSet::new(text, window_steps_of(&rendered_windows)));
    }

    // Still over budget (or nothing rendered because `text` was empty only
    // when both are empty, handled below): cut recent-call lines
    // oldest-first before giving up the window.
    let mut lines = recent_lines;
    loop {
        let bodies: Vec<&str> = rendered_windows.iter().map(|(_, s)| s.as_str()).collect();
        let text = render(&lines, &bodies);
        if !text.is_empty() && tokenizer.count(&text) <= budget {
            return Some(WorkingSet::new(text, window_steps_of(&rendered_windows)));
        }
        if !lines.is_empty() {
            lines.remove(0);
            continue;
        }
        break;
    }

    // No recent-call lines left. A single remaining window that still
    // doesn't fit is head-truncated (never dropped) so it alone fits.
    if let Some((step, only)) = rendered_windows.first() {
        let truncated = truncate_window_to_budget(only, budget, tokenizer);
        let mut steps = BTreeSet::new();
        steps.insert(*step);
        return Some(WorkingSet::new(truncated, steps));
    }

    None
}

/// The set of steps for the windows that survived budget-fitting.
fn window_steps_of(rendered_windows: &[(u64, String)]) -> BTreeSet<u64> {
    rendered_windows.iter().map(|(step, _)| *step).collect()
}

/// Head-truncates `window` (a full `"--- header ---\n<output>"` block) so
/// it fits `budget` tokens, replacing the cut tail with a
/// `[... N chars omitted ...]` marker.
fn truncate_window_to_budget(window: &str, budget: usize, tokenizer: &dyn Tokenizer) -> String {
    if tokenizer.count(window) <= budget {
        return window.to_string();
    }
    let chars: Vec<char> = window.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let head: String = chars[..mid].iter().collect();
        let omitted = chars.len() - mid;
        let candidate = format!("{head}\n[... {omitted} chars omitted ...]");
        if tokenizer.count(&candidate) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let head: String = chars[..lo].iter().collect();
    let omitted = chars.len() - lo;
    format!("{head}\n[... {omitted} chars omitted ...]")
}

/// Renders the section body: `recent calls:` header + lines, blank line,
/// then each window newest-first (the caller keeps `windows` in that
/// order). Empty when both are empty.
fn render(recent_lines: &[String], windows: &[&str]) -> String {
    if recent_lines.is_empty() && windows.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if !recent_lines.is_empty() {
        let mut block = String::from("recent calls:\n");
        block.push_str(&recent_lines.join("\n"));
        parts.push(block);
    }
    if !windows.is_empty() {
        parts.push(windows.join("\n\n"));
    }
    parts.join("\n\n")
}

fn own_entries(log: &dyn SessionLog, kind: LogKind) -> Vec<crate::log::LogEntry> {
    log.entries(Some(kind), LOG_SCAN_LIMIT)
        .into_iter()
        .filter(|e| e.agent.is_none())
        .collect()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn first_non_empty_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

fn cap_line(s: &str, cap: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        return s.to_string();
    }
    let keep = cap.saturating_sub(1);
    let head: String = chars[..keep].iter().collect();
    format!("{head}…")
}

impl sealed::Sealed for WorkingSet {}
impl Promptable for WorkingSet {
    fn render(&self) -> String {
        self.text.clone()
    }
}
