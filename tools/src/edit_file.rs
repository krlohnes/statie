//! `edit_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, write_atomic};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Path to the file, relative to the sandbox root. Must already exist
    /// and be a UTF-8 text file.
    pub path: String,
    /// The exact text to replace. Must occur exactly once in the file.
    pub old: String,
    /// The replacement text. It replaces `old` entirely; to insert text
    /// after `old`, include `old` followed by the new text.
    pub new: String,
}

/// Replaces a single exact occurrence of `old` with `new` in an existing
/// text file, writing atomically (temp file + rename).
pub struct EditFile {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace one exact occurrence of `old` with `new` in an existing file. `new` REPLACES `old` entirely, so to insert after `old`, repeat `old` at the start of `new`. `old` must match exactly once; prefer this over write_file for changes to existing files."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<EditFileArgs>()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: EditFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if args.old.is_empty() {
            return Err(ToolError::InvalidArgs(
                "old must not be empty; use write_file to create or overwrite a whole file"
                    .to_string(),
            ));
        }

        let sandbox_path = ctx.root.join(&args.path)?;
        let path = sandbox_path.as_path();

        if !path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "no such file: {}",
                args.path
            )));
        }
        if !path.is_file() {
            return Err(ToolError::ExecutionFailed(format!(
                "not a regular file: {}",
                args.path
            )));
        }

        let bytes = std::fs::read(path).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| ToolError::ExecutionFailed(format!("not valid utf-8: {}", args.path)))?;

        let match_positions: Vec<usize> = content
            .match_indices(args.old.as_str())
            .map(|(i, _)| i)
            .collect();

        let (new_content, start_line, end_line, note) = match match_positions.len() {
            0 => match whitespace_tolerant_replace(&content, &args.old, &args.new) {
                WhitespaceMatch::Unique {
                    new_content,
                    start_line,
                    end_line,
                } => (
                    new_content,
                    start_line,
                    end_line,
                    Some(format!(
                        "note: matched `old` ignoring whitespace differences (lines {start_line}-{end_line}); "
                    )),
                ),
                WhitespaceMatch::Multiple(n) => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "old text occurs {n} times in {} ignoring whitespace; include more surrounding lines so it is unique",
                        args.path
                    )));
                }
                WhitespaceMatch::None => {
                    let hint = closest_line_hint(&content, &args.old);
                    return Err(ToolError::ExecutionFailed(format!(
                        "old text not found in {}; re-read the file and copy the text exactly (whitespace matters){hint}",
                        args.path
                    )));
                }
            },
            1 => {
                let match_start = match_positions[0];
                let match_end = match_start + args.old.len();

                let mut new_content =
                    String::with_capacity(content.len() - args.old.len() + args.new.len());
                new_content.push_str(&content[..match_start]);
                new_content.push_str(&args.new);
                new_content.push_str(&content[match_end..]);

                // 1-based starting line of the replaced region: count
                // newlines before the match start.
                let start_line = content[..match_start].matches('\n').count() + 1;
                // The new text spans this many lines (at least 1).
                let new_line_count = args.new.matches('\n').count() + 1;
                let end_line = start_line + new_line_count - 1;

                (new_content, start_line, end_line, None)
            }
            n => {
                return Err(ToolError::ExecutionFailed(format!(
                    "old text occurs {n} times in {}; include more surrounding lines so it is unique",
                    args.path
                )));
            }
        };

        write_atomic(path, new_content.as_bytes())?;

        let new_lines: Vec<&str> = new_content.lines().collect();
        let total = new_lines.len();
        let ctx_start = start_line.saturating_sub(3).max(1);
        let ctx_end = (end_line + 3).min(total.max(1));

        let mut out = format!(
            "{}edited {}: replaced 1 occurrence (lines {start_line}-{end_line})\n",
            note.unwrap_or_default(),
            args.path
        );
        if total > 0 {
            for (i, line) in new_lines[(ctx_start - 1)..ctx_end].iter().enumerate() {
                let n = ctx_start + i;
                out.push_str(&format!("{n:>5} | {line}\n"));
            }
        }

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// Result of attempting a whitespace-tolerant match over the file.
enum WhitespaceMatch {
    /// Exactly one window matched; carries the already-substituted content
    /// and the 1-based (start, end) line range of the matched window in
    /// the *original* file.
    Unique {
        new_content: String,
        start_line: usize,
        end_line: usize,
    },
    /// More than one window matched (count).
    Multiple(usize),
    /// No window matched.
    None,
}

/// Normalize a line for comparison: trim leading/trailing whitespace and
/// collapse internal runs of whitespace to a single space.
fn normalize_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Try to find a window of lines in `content` whose line count matches
/// `old`'s line count and whose normalized lines equal `old`'s normalized
/// lines. If exactly one such window exists, replace it with `new` and
/// return the resulting file content plus the matched window's 1-based
/// line range (in the original file). `new` is inserted verbatim (not
/// normalized), and everything outside the window is preserved exactly.
fn whitespace_tolerant_replace(content: &str, old: &str, new: &str) -> WhitespaceMatch {
    // Preserve trailing-newline structure: `lines()` drops a final empty
    // element for content ending in '\n', which is what we want, since we
    // rejoin with '\n' and re-append the trailing newline separately.
    let file_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();

    if old_lines.is_empty() || file_lines.len() < old_lines.len() {
        return WhitespaceMatch::None;
    }

    let old_normalized: Vec<String> = old_lines.iter().map(|l| normalize_line(l)).collect();

    let mut matches: Vec<usize> = Vec::new(); // 0-based start index into file_lines
    let window_len = old_lines.len();
    for start in 0..=(file_lines.len() - window_len) {
        let window = &file_lines[start..start + window_len];
        let is_match = window
            .iter()
            .zip(old_normalized.iter())
            .all(|(file_line, old_norm)| &normalize_line(file_line) == old_norm);
        if is_match {
            matches.push(start);
        }
    }

    match matches.len() {
        0 => WhitespaceMatch::None,
        1 => {
            let start = matches[0];
            let end = start + window_len; // exclusive, 0-based

            let ends_with_newline = content.ends_with('\n');

            let mut new_content = String::new();
            for line in &file_lines[..start] {
                new_content.push_str(line);
                new_content.push('\n');
            }
            new_content.push_str(new);
            // Ensure the replacement is followed by a newline before any
            // remaining lines, unless it's the very end of the file.
            if end < file_lines.len() {
                if !new.ends_with('\n') {
                    new_content.push('\n');
                }
                for (i, line) in file_lines[end..].iter().enumerate() {
                    new_content.push_str(line);
                    if i + 1 < file_lines[end..].len() || ends_with_newline {
                        new_content.push('\n');
                    }
                }
            } else if ends_with_newline && !new.ends_with('\n') {
                new_content.push('\n');
            }

            WhitespaceMatch::Unique {
                new_content,
                start_line: start + 1,
                end_line: end,
            }
        }
        n => WhitespaceMatch::Multiple(n),
    }
}

/// Find the file line with the highest similarity to the first non-empty
/// line of `old`, and format a hint pointing at it. Returns an empty
/// string if `old` has no non-empty line or the file has no lines.
fn closest_line_hint(content: &str, old: &str) -> String {
    let needle = match old.lines().find(|l| !l.trim().is_empty()) {
        Some(l) => normalize_line(l),
        None => return String::new(),
    };
    if needle.is_empty() {
        return String::new();
    }

    let mut best: Option<(usize, f64, &str)> = None;
    for (i, line) in content.lines().enumerate() {
        let score = similarity(&needle, &normalize_line(line));
        let better = match best {
            None => true,
            Some((_, best_score, _)) => score > best_score,
        };
        if better {
            best = Some((i + 1, score, line));
        }
    }

    match best {
        Some((line_no, _, text)) => {
            let truncated = truncate_chars(text, 120);
            format!("; closest line is {line_no}: {truncated}")
        }
        None => String::new(),
    }
}

/// Truncate `s` to at most `max_chars` characters (not bytes), on a char
/// boundary.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

/// A simple normalized similarity score in `[0, 1]` between two strings,
/// based on longest-common-subsequence length over the longer string's
/// length. No external dependency; good enough to rank candidate lines.
fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let lcs_len = lcs_length(&a_chars, &b_chars);
    let max_len = a_chars.len().max(b_chars.len());
    lcs_len as f64 / max_len as f64
}

/// Classic O(n*m) longest-common-subsequence length via dynamic
/// programming with a rolling two-row table.
fn lcs_length(a: &[char], b: &[char]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut curr = vec![0usize; b.len() + 1];

    for &ac in a {
        for (j, &bc) in b.iter().enumerate() {
            curr[j + 1] = if ac == bc {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}
