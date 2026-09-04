//! `search` tool.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{cap_output, join_string_array_args};
use crate::list_files::SKIP_DIRS;

const DEFAULT_MAX_RESULTS: usize = 50;
const HARD_MAX_RESULTS: usize = 500;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_LINE_CHARS: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// A regular expression (Rust `regex` syntax) matched against each line.
    /// Required unless `path` names a single file, in which case omitting
    /// `pattern` reads the start of that file instead of searching.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Directory (or single file) relative to the sandbox root to search
    /// under. Defaults to the root.
    #[serde(alias = "file", alias = "file_path")]
    pub path: Option<String>,
    /// Only search files whose file name matches this glob, e.g. `*.rs` or
    /// `test_*.py`.
    pub glob: Option<String>,
    /// Match case-insensitively. Defaults to false.
    pub case_insensitive: Option<bool>,
    /// Maximum matching lines to return. Defaults to 50, hard-capped at 500.
    pub max_results: Option<usize>,
}

/// Recursively greps file contents under a sandbox path with a regular
/// expression, skipping the same noise directories as `list_files`.
pub struct Search {
    pub max_output_bytes: usize,
}

#[async_trait]
impl Tool for Search {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search file contents under the sandbox with a regular expression; returns path:line: text. Use it to find definitions and usages before reading whole files."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<SearchArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["pattern", "path", "glob"]);
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let pattern_is_empty = args.pattern.as_deref().is_none_or(|p| p.is_empty());
        if pattern_is_empty {
            // No `pattern`: if `path` names a single existing file, the
            // model almost certainly means "show me this file" rather than
            // "search with an empty pattern" — fall back to read_file's
            // default window instead of erroring. Otherwise (no `path`, or
            // `path` is a directory) there is nothing useful to fall back
            // to, so report what's needed.
            if let Some(rel) = &args.path {
                let sandbox_path = ctx.root.join(rel)?;
                let base = sandbox_path.as_path().to_path_buf();
                if base.is_file() {
                    let body = crate::read_file::read_window(
                        rel,
                        1,
                        crate::read_file::default_limit(),
                        crate::read_file::default_max_window_bytes(),
                        ctx,
                    )?;
                    let out = format!(
                        "note: search needs a `pattern`; showed the start of {rel} instead. To find a spot, send pattern + path.\n{body}"
                    );
                    return Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)));
                }
            }
            return Err(ToolError::InvalidArgs(
                "search needs `pattern` (a regex) and optionally `path`; to see a file, use read_file"
                    .to_string(),
            ));
        }
        let pattern = args.pattern.clone().expect("checked non-empty above");

        let case_insensitive = args.case_insensitive.unwrap_or(false);
        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .min(HARD_MAX_RESULTS);

        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let matcher: Option<GlobMatcher> = match &args.glob {
            Some(g) => Some(
                Glob::new(g)
                    .map_err(|e| ToolError::InvalidArgs(e.to_string()))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let rel = args.path.unwrap_or_else(|| ".".to_string());
        let sandbox_path = ctx.root.join(&rel)?;
        let base = sandbox_path.as_path().to_path_buf();

        if !base.exists() {
            return Err(ToolError::ExecutionFailed(format!("no such path: {rel}")));
        }

        let mut files: Vec<PathBuf> = Vec::new();
        if base.is_file() {
            files.push(base.clone());
        } else {
            collect_files(&base, &matcher, &mut files)?;
        }

        // Paths are always reported relative to the sandbox root (not to
        // `path`), so a hit can be passed straight to `read_file`/`edit_file`.
        let display_root = ctx.root.path().to_path_buf();

        // Matches are collected per-file (each file's lines kept alongside,
        // since the ≤5 rendering below needs to build context windows once
        // the total count is known) before any rendering decision is made.
        let mut file_matches: Vec<FileMatches> = Vec::new();
        let mut count = 0usize;
        let mut hit_cap = false;
        let mut first_match: Option<(String, usize)> = None;

        'files: for file in &files {
            let content = match read_text_file(file) {
                Some(c) => c,
                None => continue,
            };

            let rel_display = file
                .strip_prefix(&display_root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");

            let file_lines: Vec<String> =
                content.lines().map(|l| l.trim_end().to_string()).collect();

            let mut match_lines: Vec<usize> = Vec::new();
            for (idx, line) in file_lines.iter().enumerate() {
                if regex.is_match(line) {
                    let line_no = idx + 1;
                    match_lines.push(line_no);
                    if first_match.is_none() {
                        first_match = Some((rel_display.clone(), line_no));
                    }
                    count += 1;
                    if count >= max_results {
                        hit_cap = true;
                        break;
                    }
                }
            }

            if !match_lines.is_empty() {
                file_matches.push(FileMatches {
                    rel_display,
                    file_lines,
                    match_lines,
                });
            }

            if hit_cap {
                break 'files;
            }
        }

        let out = if count == 0 {
            format!("no matches for /{pattern}/")
        } else {
            let mut out = format!("{count} match(es) for /{pattern}/\n");
            if count <= 5 {
                out.push_str(&render_with_context(&file_matches));
            } else {
                out.push_str(&render_compact(&file_matches));
            }
            if hit_cap {
                out.push_str(&format!(
                    "\n…(stopped at {max_results} results; narrow the pattern or path)"
                ));
            }
            if let Some((path, line_no)) = first_match {
                let offset = line_no.saturating_sub(5).max(1);
                let match_text = first_match_line_text(&file_matches);
                out.push_str(&format!(
                    "\nto edit here: edit_file {{\"path\": \"{path}\", \"before\": \"{match_text}\", \"new\": \"...\"}} — or read_file {{\"path\": \"{path}\", \"offset\": {offset}, \"limit\": 40}}"
                ));
            }
            out
        };

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// One file's collected matches, along with its full (trimmed) line content
/// so that context windows can be built once the total match count across
/// all files is known.
struct FileMatches {
    rel_display: String,
    file_lines: Vec<String>,
    /// 1-based line numbers of matching lines, in ascending order.
    match_lines: Vec<usize>,
}

/// Today's compact rendering: each match plus one following line of
/// context, truncated per-line. Used when there are more than 5 matches.
fn render_compact(file_matches: &[FileMatches]) -> String {
    let mut result_lines: Vec<String> = Vec::new();
    for fm in file_matches {
        for &line_no in &fm.match_lines {
            let idx = line_no - 1;
            let truncated = truncate_line(&fm.file_lines[idx]);
            result_lines.push(format!("{}:{line_no}: {truncated}", fm.rel_display));
            if let Some(next_line) = fm.file_lines.get(idx + 1) {
                let next_truncated = truncate_line(next_line);
                result_lines.push(format!(
                    "{}:{}- {next_truncated}",
                    fm.rel_display,
                    line_no + 1
                ));
            }
        }
    }
    result_lines.join("\n")
}

/// Grep-style rendering with 2 lines of context before and 6 after each
/// match. Overlapping/adjacent windows within a file merge into one; a `--`
/// separator is emitted between windows that are non-adjacent (a gap in the
/// same file) or that belong to different files. Used when there are ≤5
/// matches total.
fn render_with_context(file_matches: &[FileMatches]) -> String {
    const BEFORE: usize = 2;
    const AFTER: usize = 6;

    let mut blocks: Vec<String> = Vec::new();

    for fm in file_matches {
        // Merge each match's [line_no - BEFORE, line_no + AFTER] window
        // (1-based, clamped to the file) into a list of non-overlapping,
        // non-adjacent ranges.
        let mut windows: Vec<(usize, usize)> = Vec::new();
        let last_line = fm.file_lines.len();
        for &line_no in &fm.match_lines {
            let start = line_no.saturating_sub(BEFORE).max(1);
            let end = (line_no + AFTER).min(last_line);
            match windows.last_mut() {
                // Adjacent or overlapping (start is within one line of the
                // previous window's end) merges into the same window.
                Some((_, prev_end)) if start <= *prev_end + 1 => {
                    *prev_end = (*prev_end).max(end);
                }
                _ => windows.push((start, end)),
            }
        }

        let match_set: std::collections::HashSet<usize> = fm.match_lines.iter().copied().collect();

        for (start, end) in windows {
            let mut block = String::new();
            for line_no in start..=end {
                let text = truncate_line(&fm.file_lines[line_no - 1]);
                let sep = if match_set.contains(&line_no) {
                    ':'
                } else {
                    '-'
                };
                if !block.is_empty() {
                    block.push('\n');
                }
                block.push_str(&format!("{}:{line_no}{sep} {text}", fm.rel_display));
            }
            blocks.push(block);
        }
    }

    blocks.join("\n--\n")
}

/// The first match's line text (trimmed, truncated, JSON-escaped) for the
/// `edit_file` trailer's `"before"` field.
fn first_match_line_text(file_matches: &[FileMatches]) -> String {
    let Some(fm) = file_matches.first() else {
        return String::new();
    };
    let Some(&line_no) = fm.match_lines.first() else {
        return String::new();
    };
    let text = truncate_line(fm.file_lines[line_no - 1].trim_start());
    json_escape(&text)
}

/// Escapes `s` for embedding inside a JSON string literal.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Recursively walk `dir`, collecting file paths that pass the glob filter
/// (if any), skipping noise directories and following no symlinks.
fn collect_files(
    dir: &Path,
    matcher: &Option<GlobMatcher>,
    out: &mut Vec<PathBuf>,
) -> Result<(), ToolError> {
    let read = std::fs::read_dir(dir).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    let mut entries: Vec<_> = read
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_files(&path, matcher, out)?;
        } else if file_type.is_file() {
            if let Some(m) = matcher
                && !m.is_match(name_str.as_ref())
            {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

/// Reads `path` as UTF-8 text, returning `None` (silently skipping) if the
/// file is too large or not valid UTF-8.
fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
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
