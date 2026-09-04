//! `filter_last_tool_call` tool: narrows a prior tool's full output (kept in
//! the session log even when the observation the model saw was truncated)
//! with a regex, instead of forcing a re-run of the underlying command.

use async_trait::async_trait;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{LogKind, Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::cap_output;

const DEFAULT_MAX_RESULTS: usize = 50;
const HARD_MAX_RESULTS: usize = 500;
const DEFAULT_CONTEXT: usize = 0;
const MAX_CONTEXT: usize = 5;
const MAX_LINE_CHARS: usize = 200;
/// How far back to scan the tool-output log to find the most recent entry
/// that was *not* itself produced by `filter_last_tool_call`.
const LOOKBACK: usize = 20;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilterLastArgs {
    /// A regular expression (Rust `regex` syntax) matched against each line.
    pub pattern: String,
    /// Lines of context to include before and after each match. Defaults to
    /// 0, capped at 5.
    pub context: Option<usize>,
    /// Match case-insensitively. Defaults to false.
    pub case_insensitive: Option<bool>,
    /// Maximum matching lines to return. Defaults to 50, hard-capped at 500.
    pub max_results: Option<usize>,
}

/// Applies a regex to the full output of the most recent prior tool call
/// (read back from the session log), returning only the matching lines
/// (with optional context). Skips over its own past outputs so repeated
/// filtering keeps targeting the original command's output.
pub struct FilterLastToolCall {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for FilterLastToolCall {
    fn name(&self) -> &str {
        "filter_last_tool_call"
    }

    fn description(&self) -> &str {
        "Apply a regex to the FULL output of the most recent tool call (kept in the session log even when the observation was truncated) and return only the matching lines. Use this instead of re-running a command whose output overflowed."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<FilterLastArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: FilterLastArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let case_insensitive = args.case_insensitive.unwrap_or(false);
        let context = args.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);
        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .min(HARD_MAX_RESULTS);

        // If `pattern` doesn't compile as a regex, the model likely passed a
        // literal snippet (e.g. a chunk of source containing regex
        // metacharacters). Fall back to matching it literally instead of
        // erroring, so the model doesn't have to figure out how to escape
        // it. A literal containing escaped `\n` sequences is unescaped to
        // real newlines first, so a multi-line snippet can still be found
        // even though matching is otherwise per line (see `line_span_matches`).
        let (regex, literal_fallback) = match RegexBuilder::new(&args.pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(regex) => (regex, false),
            Err(_) => {
                let literal = args.pattern.replace("\\n", "\n");
                let regex = RegexBuilder::new(&regex::escape(&literal))
                    .case_insensitive(case_insensitive)
                    .build()
                    .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
                (regex, true)
            }
        };

        let recent = ctx.log.entries(Some(LogKind::ToolOutput), LOOKBACK);
        let target = recent.iter().rev().find(|e| {
            e.payload
                .get("name")
                .and_then(Value::as_str)
                .is_none_or(|n| n != "filter_last_tool_call")
        });

        let entry = target.ok_or_else(|| {
            ToolError::ExecutionFailed("no previous tool output to filter".to_string())
        })?;

        let tool_name = entry
            .payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let output = entry
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("");

        let lines: Vec<&str> = output.lines().collect();
        let total = lines.len();

        // Which line indices (0-based) are matches. Ordinarily this matches
        // line-by-line (the whole tool works one line at a time). But the
        // literal fallback's pattern may itself contain real newlines (from
        // unescaping `\n`), which can never match within a single output
        // line — so in that case the whole output text is searched instead,
        // and every line the match spans is recorded, so a multi-line
        // snippet can still be found.
        let match_idxs: Vec<usize> = if regex.as_str().contains('\n') {
            line_span_matches(&regex, output)
        } else {
            lines
                .iter()
                .enumerate()
                .filter(|(_, l)| regex.is_match(l))
                .map(|(i, _)| i)
                .collect()
        };

        if match_idxs.is_empty() {
            let out = format!(
                "no lines match /{}/ in the last `{tool_name}` output ({total} lines)",
                args.pattern
            );
            return Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)));
        }

        let mut count = 0usize;
        let mut hit_cap = false;
        let mut kept_matches: Vec<usize> = Vec::new();
        for &idx in &match_idxs {
            kept_matches.push(idx);
            count += 1;
            if count >= max_results {
                hit_cap = true;
                break;
            }
        }

        // Build the set of line indices to render: each kept match plus its
        // context window, merged into contiguous groups (grep style).
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &idx in &kept_matches {
            let start = idx.saturating_sub(context);
            let end = (idx + context).min(total.saturating_sub(1));
            ranges.push((start, end));
        }

        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1 + 1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }

        let match_set: std::collections::HashSet<usize> = kept_matches.iter().copied().collect();

        let mut out = format!(
            "{count} matching line(s) of {total} in the last `{tool_name}` output (step {})\n",
            entry.step
        );

        let mut body_parts: Vec<String> = Vec::new();
        for (start, end) in &merged {
            let mut group_lines: Vec<String> = Vec::new();
            for (idx, line) in lines.iter().enumerate().take(*end + 1).skip(*start) {
                let line_no = idx + 1;
                let trimmed = line.trim_end();
                let truncated = truncate_line(trimmed);
                let sep = if match_set.contains(&idx) { ':' } else { '-' };
                group_lines.push(format!("{line_no}{sep} {truncated}"));
            }
            body_parts.push(group_lines.join("\n"));
        }
        out.push_str(&body_parts.join("\n--\n"));

        if hit_cap {
            out.push_str(&format!(
                "\n…(stopped at {max_results} matches; narrow the pattern)"
            ));
        }

        if literal_fallback {
            out = format!("note: pattern was not a valid regex; matched it literally\n{out}");
        }

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Finds every byte-offset match of `regex` in the whole `text` (used for
/// literal fallback patterns that themselves contain newlines, which can
/// never match within a single output line) and returns the 0-based line
/// indices each match spans, in ascending order with duplicates removed.
fn line_span_matches(regex: &regex::Regex, text: &str) -> Vec<usize> {
    // Byte offset each line starts at, so a match's byte range can be
    // mapped back to the line indices it covers.
    let mut line_starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in text.lines() {
        line_starts.push(offset);
        offset += line.len();
        // `str::lines` splits on `\n` (and trims a trailing `\r`), so
        // account for the newline byte itself when it's actually there.
        if text[offset..].starts_with("\r\n") {
            offset += 2;
        } else if text[offset..].starts_with('\n') {
            offset += 1;
        }
    }

    let mut idxs: Vec<usize> = Vec::new();
    for m in regex.find_iter(text) {
        let start_line = line_starts
            .partition_point(|&s| s <= m.start())
            .saturating_sub(1);
        let end_line = if m.end() == m.start() {
            start_line
        } else {
            line_starts
                .partition_point(|&s| s < m.end())
                .saturating_sub(1)
        };
        for l in start_line..=end_line {
            idxs.push(l);
        }
    }
    idxs.sort_unstable();
    idxs.dedup();
    idxs
}

/// Cuts `line` to at most `MAX_LINE_CHARS` chars, appending `…` if cut.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
    truncated.push('…');
    truncated
}
