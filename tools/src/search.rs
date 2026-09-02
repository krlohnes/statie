//! `search` tool.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::cap_output;
use crate::list_files::SKIP_DIRS;

const DEFAULT_MAX_RESULTS: usize = 50;
const HARD_MAX_RESULTS: usize = 500;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_LINE_CHARS: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// A regular expression (Rust `regex` syntax) matched against each line.
    pub pattern: String,
    /// Directory (or single file) relative to the sandbox root to search
    /// under. Defaults to the root.
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

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let case_insensitive = args.case_insensitive.unwrap_or(false);
        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .min(HARD_MAX_RESULTS);

        let regex = RegexBuilder::new(&args.pattern)
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

        let mut result_lines: Vec<String> = Vec::new();
        let mut count = 0usize;
        let mut hit_cap = false;

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

            for (idx, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    let line_no = idx + 1;
                    let trimmed = line.trim_end();
                    let truncated = truncate_line(trimmed);
                    result_lines.push(format!("{rel_display}:{line_no}: {truncated}"));
                    count += 1;
                    if count >= max_results {
                        hit_cap = true;
                        break 'files;
                    }
                }
            }
        }

        let out = if result_lines.is_empty() {
            format!("no matches for /{}/", args.pattern)
        } else {
            let mut out = format!("{count} match(es) for /{}/\n", args.pattern);
            out.push_str(&result_lines.join("\n"));
            if hit_cap {
                out.push_str(&format!(
                    "\n…(stopped at {max_results} results; narrow the pattern or path)"
                ));
            }
            out
        };

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
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
