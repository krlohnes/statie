//! `edit_file` tool.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use statie_core::{Schema, Tool, ToolCtx, ToolError, ToolOutputFull};

use crate::common::{backup_before_edit, cap_output, join_string_array_args, write_atomic};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Path to the file, relative to the sandbox root. Must already exist
    /// and be a UTF-8 text file.
    #[serde(alias = "file", alias = "file_path")]
    pub path: String,
    /// Exact text to replace with `new` (must occur exactly once). Use
    /// this OR `after`/`before`.
    /// if `old` is given, `after`/`before` are ignored.
    #[serde(default)]
    pub old: Option<String>,
    /// Replacement text (with `old`) or the text to insert (with
    /// `after`/`before`).
    pub new: String,
    /// One or more consecutive lines (copied verbatim from read_file,
    /// without line numbers) that occur exactly once in the file; `new` is
    /// inserted after them. Easiest way to add code: anchor on one line.
    #[serde(default)]
    pub after: Option<String>,
    /// One or more consecutive lines (copied verbatim from read_file,
    /// without line numbers) that occur exactly once in the file; `new` is
    /// inserted before them. Easiest way to add code: anchor on one line.
    #[serde(default)]
    pub before: Option<String>,
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
        "Edit a file in place: replace one exact occurrence of `old` with `new`, or insert `new` after/before a single unique anchor line (`after`/`before`). Prefer the anchor form for additions."
    }

    fn args_schema(&self) -> Schema {
        Schema::of::<EditFileArgs>()
    }

    async fn call(&self, mut args: Value, ctx: &ToolCtx) -> Result<ToolOutputFull, ToolError> {
        join_string_array_args(&mut args, &["path", "old", "new", "after", "before"]);
        let mut args: EditFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let mode_count = [
            args.old.is_some(),
            args.after.is_some(),
            args.before.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();

        // `old` wins whenever it is set (and non-empty; the empty case is
        // rejected below): if `after` and/or `before` are also set, they are
        // ignored and the edit proceeds as a plain `old` -> `new` replace.
        // When the ignored anchor(s) carry the *same* text as `old`, this is
        // silent (the pre-existing identical-text tolerance). When they
        // differ, a note naming the ignored field(s) is appended to the
        // success output so the caller can see the anchors were dropped
        // rather than honored.
        let mut ignored_anchor_note: Option<String> = None;
        if mode_count >= 2
            && let Some(old) = &args.old
            && !old.is_empty()
        {
            let mut ignored_fields = Vec::new();
            if let Some(after) = &args.after {
                if old.trim() != after.trim() {
                    ignored_fields.push("`after`");
                }
                args.after = None;
            }
            if let Some(before) = &args.before {
                if old.trim() != before.trim() {
                    ignored_fields.push("`before`");
                }
                args.before = None;
            }
            if !ignored_fields.is_empty() {
                ignored_anchor_note = Some(format!(
                    "note: ignored {}: `old` already locates the text",
                    ignored_fields.join(" and ")
                ));
            }
        }

        let mode_count = [
            args.old.is_some(),
            args.after.is_some(),
            args.before.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if mode_count != 1 {
            return Err(ToolError::InvalidArgs(
                "exactly one of `old`, `after`, `before` must be set".to_string(),
            ));
        }

        if let Some(old) = &args.old
            && old.is_empty()
        {
            return Err(ToolError::InvalidArgs(
                "old must not be empty; use write_file to create or overwrite a whole file"
                    .to_string(),
            ));
        }

        let anchor = args.after.as_deref().or(args.before.as_deref());

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

        let (new_content, start_line, end_line, header) = if let Some(old) = &args.old {
            let match_positions: Vec<usize> = content
                .match_indices(old.as_str())
                .map(|(i, _)| i)
                .collect();

            match match_positions.len() {
                0 => match whitespace_tolerant_replace(&content, old, &args.new) {
                    WhitespaceMatch::Unique {
                        new_content,
                        start_line,
                        end_line,
                    } => {
                        let header = if args.new.is_empty() {
                            let deleted = end_line - start_line + 1;
                            format!(
                                "edited {}: deleted {deleted} line(s) (lines {start_line}-{end_line})",
                                args.path
                            )
                        } else {
                            format!(
                                "edited {}: replaced 1 occurrence (lines {start_line}-{end_line})",
                                args.path
                            )
                        };
                        (
                            new_content,
                            start_line,
                            end_line,
                            format!(
                                "note: matched `old` ignoring whitespace differences (lines {start_line}-{end_line}); {header}"
                            ),
                        )
                    }
                    WhitespaceMatch::Multiple(n) => {
                        return Err(ToolError::ExecutionFailed(format!(
                            "old text occurs {n} times in {} ignoring whitespace; include more surrounding lines so it is unique",
                            args.path
                        )));
                    }
                    WhitespaceMatch::None => match collapsed_whitespace_find(&content, old) {
                        CollapsedMatch::Unique {
                            start_line,
                            end_line,
                        } => {
                            let file_lines: Vec<&str> = content.lines().collect();
                            let ends_with_newline = content.ends_with('\n');
                            let mut new_content = String::new();
                            for line in &file_lines[..start_line - 1] {
                                new_content.push_str(line);
                                new_content.push('\n');
                            }
                            new_content.push_str(&args.new);
                            if end_line < file_lines.len() {
                                if !args.new.ends_with('\n') {
                                    new_content.push('\n');
                                }
                                for (i, line) in file_lines[end_line..].iter().enumerate() {
                                    new_content.push_str(line);
                                    if i + 1 < file_lines[end_line..].len() || ends_with_newline {
                                        new_content.push('\n');
                                    }
                                }
                            } else if ends_with_newline && !args.new.ends_with('\n') {
                                new_content.push('\n');
                            }

                            let header = if args.new.is_empty() {
                                let deleted = end_line - start_line + 1;
                                format!(
                                    "edited {}: deleted {deleted} line(s) (lines {start_line}-{end_line})",
                                    args.path
                                )
                            } else {
                                format!(
                                    "edited {}: replaced 1 occurrence (lines {start_line}-{end_line})",
                                    args.path
                                )
                            };
                            (
                                new_content,
                                start_line,
                                end_line,
                                format!(
                                    "note: matched `old` ignoring line breaks and whitespace (lines {start_line}-{end_line}); {header}"
                                ),
                            )
                        }
                        CollapsedMatch::Multiple(ranges) => {
                            let ranges_str: Vec<String> = ranges
                                .iter()
                                .map(|(s, e)| {
                                    if s == e {
                                        format!("{s}")
                                    } else {
                                        format!("{s}-{e}")
                                    }
                                })
                                .collect();
                            return Err(ToolError::ExecutionFailed(format!(
                                "old text occurs {} times in {} ignoring line breaks and whitespace (lines {}); include more surrounding lines so it is unique",
                                ranges.len(),
                                args.path,
                                ranges_str.join(", ")
                            )));
                        }
                        CollapsedMatch::None => {
                            let hint = closest_line_hint(&content, old);
                            return Err(ToolError::ExecutionFailed(format!(
                                "old text not found in {}; re-read the file and copy the text exactly (whitespace matters){hint}",
                                args.path
                            )));
                        }
                    },
                },
                1 => {
                    let match_start = match_positions[0];
                    let match_end = match_start + old.len();

                    let mut new_content =
                        String::with_capacity(content.len() - old.len() + args.new.len());
                    new_content.push_str(&content[..match_start]);
                    new_content.push_str(&args.new);
                    new_content.push_str(&content[match_end..]);

                    // 1-based starting line of the replaced region: count
                    // newlines before the match start.
                    let start_line = content[..match_start].matches('\n').count() + 1;

                    if args.new.is_empty() {
                        // Deletion: report the span of the removed `old`
                        // text rather than the (empty) replacement.
                        let old_line_count = old.matches('\n').count() + 1;
                        let end_line = start_line + old_line_count - 1;
                        let header = format!(
                            "edited {}: deleted {old_line_count} line(s) (lines {start_line}-{end_line})",
                            args.path
                        );
                        (new_content, start_line, end_line, header)
                    } else {
                        // The new text spans this many lines (at least 1).
                        let new_line_count = args.new.matches('\n').count() + 1;
                        let end_line = start_line + new_line_count - 1;
                        let header = format!(
                            "edited {}: replaced 1 occurrence (lines {start_line}-{end_line})",
                            args.path
                        );
                        (new_content, start_line, end_line, header)
                    }
                }
                n => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "old text occurs {n} times in {}; include more surrounding lines so it is unique",
                        args.path
                    )));
                }
            }
        } else {
            let anchor = anchor.expect("mode_count==1 guarantees after or before is set");
            let insert_after = args.after.is_some();

            let (stripped_new, repeated_anchor_note) =
                strip_repeated_anchor(anchor, &args.new, insert_after)?;

            check_not_already_present(&content, stripped_new, &args.path)?;
            check_no_duplicate_definition(&content, stripped_new, &args.path)?;

            let (new_content, start_line, end_line, header) =
                insert_by_anchor(&content, anchor, stripped_new, insert_after, &args.path)?;
            (
                new_content,
                start_line,
                end_line,
                match repeated_anchor_note {
                    Some(note) => format!("{note}; {header}"),
                    None => header,
                },
            )
        };

        backup_before_edit(&ctx.root, &args.path)?;

        write_atomic(path, new_content.as_bytes())?;

        let new_lines: Vec<&str> = new_content.lines().collect();
        let total = new_lines.len();
        let ctx_start = start_line.saturating_sub(3).max(1);
        let ctx_end = (end_line + 3).min(total.max(1));

        let mut out = format!("{header}\n");
        if total > 0 {
            for (i, line) in new_lines[(ctx_start - 1)..ctx_end].iter().enumerate() {
                let n = ctx_start + i;
                out.push_str(&format!("{n:>5} | {line}\n"));
            }
        }
        if let Some(note) = &ignored_anchor_note {
            out.push_str(note);
            out.push('\n');
        }
        if let Some(note) = brace_balance_note(&args.path, &content, &new_content) {
            out.push_str(&note);
            out.push('\n');
        }

        Ok(ToolOutputFull::new(cap_output(out, self.max_output_bytes)))
    }
}

/// If `new` (whitespace-tolerant, per-line) begins (in `after` mode) or ends
/// (in `before` mode) with `anchor`'s text, strips that repeated anchor
/// (plus the one adjoining newline) and returns the remaining text along
/// with a note describing the strip. Returns an error if stripping would
/// leave nothing to insert. If `new` does not begin/end with the anchor,
/// returns `new` unchanged with no note.
fn strip_repeated_anchor<'a>(
    anchor: &str,
    new: &'a str,
    insert_after: bool,
) -> Result<(&'a str, Option<&'static str>), ToolError> {
    let anchor_lines: Vec<&str> = anchor.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if anchor_lines.is_empty() || new_lines.len() < anchor_lines.len() {
        return Ok((new, None));
    }

    let lines_match_ws_tolerant = |a: &str, b: &str| normalize_line(a) == normalize_line(b);

    let (repeated, stripped, note): (bool, &str, &'static str) = if insert_after {
        let matches = new_lines[..anchor_lines.len()]
            .iter()
            .zip(anchor_lines.iter())
            .all(|(n, a)| lines_match_ws_tolerant(n, a));
        if !matches {
            (false, new, "")
        } else {
            // Find the byte offset right after the anchor_lines.len()-th
            // line (and the following newline, if any) in `new`.
            let mut offset = 0usize;
            let mut newline_count = 0usize;
            for (i, ch) in new.char_indices() {
                if ch == '\n' {
                    newline_count += 1;
                    if newline_count == anchor_lines.len() {
                        offset = i + 1;
                        break;
                    }
                }
            }
            if newline_count < anchor_lines.len() {
                // `new` has no trailing newline after the last anchor line,
                // meaning `new` *is* exactly the anchor lines (nothing
                // follows).
                offset = new.len();
            }
            (
                true,
                &new[offset..],
                "note: `new` began with the `after` anchor; inserted only the lines after it",
            )
        }
    } else {
        let start = new_lines.len() - anchor_lines.len();
        let matches = new_lines[start..]
            .iter()
            .zip(anchor_lines.iter())
            .all(|(n, a)| lines_match_ws_tolerant(n, a));
        if !matches {
            (false, new, "")
        } else {
            // Find the byte offset right before the start-th line (i.e. cut
            // off the trailing anchor block and its preceding newline).
            let mut offset = new.len();
            let mut newline_count = 0usize;
            let mut last_nl_before_cut = None;
            for (i, ch) in new.char_indices() {
                if ch == '\n' {
                    newline_count += 1;
                    if newline_count == start {
                        last_nl_before_cut = Some(i);
                    }
                }
            }
            if start == 0 {
                offset = 0;
            } else if let Some(i) = last_nl_before_cut {
                offset = i + 1;
            }
            (
                true,
                &new[..offset],
                "note: `new` ended with the `before` anchor; inserted only the lines before it",
            )
        }
    };

    if !repeated {
        return Ok((new, None));
    }

    if stripped.trim().is_empty() {
        return Err(ToolError::ExecutionFailed(
            "`new` is just the anchor itself; nothing to insert. To replace the anchored block, use old/new."
                .to_string(),
        ));
    }

    Ok((stripped, Some(note)))
}

/// Renders `file_lines[first_line..=last_line]` (1-based, inclusive) in the
/// same numbered style `read_file` uses (`{n:>5} | {line}`), capped at 40
/// printed lines: if the range is longer, only the first 40 lines are shown
/// followed by a trailer line noting how many more there are.
fn numbered_window(file_lines: &[&str], first_line: usize, last_line: usize) -> String {
    let last_shown = last_line.min(first_line + 39);
    let mut out = String::new();
    for (i, line) in file_lines[(first_line - 1)..last_shown].iter().enumerate() {
        let n = first_line + i;
        out.push_str(&format!("{n:>5} | {line}\n"));
    }
    if last_shown < last_line {
        let more = last_line - last_shown;
        out.push_str(&format!("  ... ({more} more lines)\n"));
    }
    out
}

/// Refuses an `after`/`before` insertion whose text is already present in
/// the file. A small model retrying "add these lines" after a partial
/// success (or a confused re-read) tends to re-send text that is already
/// there rather than noticing it landed; inserting it again silently
/// duplicates it and, worse, makes every future anchor in that region
/// ambiguous.
///
/// Only applies when `new` (the text that would actually be inserted, i.e.
/// after `strip_repeated_anchor`) has at least 2 non-blank lines — a
/// single-line insertion is commonly boilerplate (a closing brace, `);`, an
/// empty line) that legitimately recurs throughout a file and must not be
/// blocked.
///
/// Matching is the same whitespace-tolerant, per-line comparison used
/// elsewhere in this file, and blank lines are ignored on both sides (so a
/// blank-line gap in `new` doesn't defeat the match against a
/// differently-spaced block already in the file). If found, names the
/// first (lowest-line-numbered) occurrence and quotes the matched range (in
/// `read_file`'s numbered style, capped at 40 lines) so the caller can see
/// what is actually there rather than just being told it exists.
fn check_not_already_present(content: &str, new: &str, path: &str) -> Result<(), ToolError> {
    let new_lines: Vec<&str> = new
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if new_lines.len() < 2 {
        return Ok(());
    }
    let new_normalized: Vec<String> = new_lines.iter().map(|l| normalize_line(l)).collect();

    let file_lines: Vec<&str> = content.lines().collect();
    // Non-blank file lines, each paired with its 1-based line number, so a
    // contiguous match in this filtered view can be reported by its real
    // (possibly non-contiguous-in-the-filtered-sense) start/end line
    // numbers in the original file.
    let non_blank: Vec<(usize, String)> = file_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i + 1, normalize_line(l)))
        .collect();

    let window_len = new_normalized.len();
    if non_blank.len() < window_len {
        return Ok(());
    }

    for start in 0..=(non_blank.len() - window_len) {
        let window = &non_blank[start..start + window_len];
        let is_match = window
            .iter()
            .zip(new_normalized.iter())
            .all(|((_, file_norm), new_norm)| file_norm == new_norm);
        if is_match {
            let first_line = window.first().expect("window_len >= 2").0;
            let last_line = window.last().expect("window_len >= 2").0;
            let quoted = numbered_window(&file_lines, first_line, last_line);
            return Err(ToolError::ExecutionFailed(format!(
                "the text to insert is already in {path} at lines {first_line}-{last_line}, so this edit is already applied; nothing was changed. If the test still fails, the block that is there is wrong, not missing: replace it with old/new, copying `old` from these lines exactly.\n{quoted}"
            )));
        }
    }

    Ok(())
}

/// Handle `after`/`before` anchor insertion: match `anchor` (one or more
/// consecutive lines) as a whole block (exact first, then per-line
/// whitespace-normalized), require a unique match, insert `new` after or
/// before the block, and return `(new_content, start_line, end_line,
/// header)` in the same shape the `old`/`new` replace path uses, where
/// `start_line`/`end_line` span the inserted text for the context window.
fn insert_by_anchor(
    content: &str,
    anchor: &str,
    new: &str,
    insert_after: bool,
    path: &str,
) -> Result<(String, usize, usize, String), ToolError> {
    let file_lines: Vec<&str> = content.lines().collect();
    let anchor_lines: Vec<&str> = anchor.lines().collect();
    let window_len = anchor_lines.len().max(1);

    let find_windows = |eq: &dyn Fn(&str, &str) -> bool| -> Vec<usize> {
        if window_len == 0 || file_lines.len() < window_len {
            return Vec::new();
        }
        (0..=(file_lines.len() - window_len))
            .filter(|&start| {
                file_lines[start..start + window_len]
                    .iter()
                    .zip(anchor_lines.iter())
                    .all(|(file_line, anchor_line)| eq(file_line, anchor_line))
            })
            .collect()
    };

    let exact_matches = find_windows(&|a: &str, b: &str| a == b);

    // `matches` is `None` when a tier found zero windows, so the next tier
    // is tried; `Some(vec)` (even empty is never the case for `Some` below,
    // since a tier that finds nothing returns `None` to fall through, not
    // `Some(vec![])`) means that tier is the one whose result decides the
    // outcome (unique match, ambiguity error, or note text).
    let per_line_note = "note: matched anchor ignoring whitespace differences; ";
    let (matches, note): (Vec<usize>, &str) = if !exact_matches.is_empty() {
        (exact_matches, "")
    } else {
        let norm_matches = find_windows(&|a: &str, b: &str| normalize_line(a) == normalize_line(b));
        if !norm_matches.is_empty() {
            (norm_matches, per_line_note)
        } else {
            (Vec::new(), "")
        }
    };

    let tried_collapsed_tier = matches.is_empty();
    let (anchor_start_idx, anchor_end_idx) = if !matches.is_empty() {
        match matches.len() {
            1 => (matches[0], matches[0] + window_len - 1),
            n => {
                let line_nos: Vec<String> = matches.iter().map(|i| (i + 1).to_string()).collect();
                return Err(ToolError::ExecutionFailed(format!(
                    "anchor line occurs {n} times in {path} (lines {}); pick a unique line",
                    line_nos.join(", ")
                )));
            }
        }
    } else {
        match collapsed_whitespace_find(content, anchor) {
            CollapsedMatch::Unique {
                start_line,
                end_line,
            } => (start_line - 1, end_line - 1),
            CollapsedMatch::Multiple(ranges) => {
                let ranges_str: Vec<String> = ranges
                    .iter()
                    .map(|(s, e)| {
                        if s == e {
                            format!("{s}")
                        } else {
                            format!("{s}-{e}")
                        }
                    })
                    .collect();
                return Err(ToolError::ExecutionFailed(format!(
                    "anchor line occurs {} times in {path} ignoring line breaks and whitespace (lines {}); pick a unique line",
                    ranges.len(),
                    ranges_str.join(", ")
                )));
            }
            CollapsedMatch::None => {
                let hint = closest_line_hint(content, anchor);
                return Err(ToolError::ExecutionFailed(format!(
                    "anchor line not found in {path}; re-read the file and copy the line(s) exactly (whitespace matters){hint}"
                )));
            }
        }
    };
    let note: String = if tried_collapsed_tier {
        format!(
            "note: matched anchor ignoring line breaks and whitespace (lines {}-{}); ",
            anchor_start_idx + 1,
            anchor_end_idx + 1
        )
    } else {
        note.to_string()
    };
    let note = note.as_str();

    let ends_with_newline = content.ends_with('\n');
    let insert_pos = if insert_after {
        anchor_end_idx + 1
    } else {
        anchor_start_idx
    };

    let mut new_lines_owned = new.to_string();
    if !new_lines_owned.ends_with('\n') {
        new_lines_owned.push('\n');
    }
    let inserted_line_count = new_lines_owned.matches('\n').count();

    let mut new_content = String::new();
    for line in &file_lines[..insert_pos] {
        new_content.push_str(line);
        new_content.push('\n');
    }
    new_content.push_str(&new_lines_owned);
    for (i, line) in file_lines[insert_pos..].iter().enumerate() {
        new_content.push_str(line);
        if i + 1 < file_lines[insert_pos..].len() || ends_with_newline {
            new_content.push('\n');
        }
    }

    // 1-based line numbers of the inserted text in the *new* file.
    let start_line = insert_pos + 1;
    let end_line = start_line + inserted_line_count - 1;

    let anchor_line_no = if insert_after {
        anchor_end_idx + 1
    } else {
        anchor_start_idx + 1
    };
    let header = format!(
        "{note}edited {path}: inserted {inserted_line_count} line(s) {} line {anchor_line_no}",
        if insert_after { "after" } else { "before" }
    );

    Ok((new_content, start_line, end_line, header))
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

/// Result of attempting a collapsed-whitespace (all whitespace removed,
/// including newlines) match over the file. This is the third and final
/// matching tier, tried only after exact and per-line whitespace-tolerant
/// matching have both failed to find a unique window; it exists to bridge a
/// line-break difference between the needle (`old`, or an `after`/`before`
/// anchor) and the file, which per-line matching cannot do.
enum CollapsedMatch {
    /// Exactly one match; carries the 1-based (start, end) line range in the
    /// original file that contains the matched text.
    Unique { start_line: usize, end_line: usize },
    /// More than one match; carries each match's 1-based (start, end) line
    /// range in the original file, in file order.
    Multiple(Vec<(usize, usize)>),
    /// No match (including a match that failed the token-boundary check).
    None,
}

/// Strip all whitespace (as defined by `char::is_whitespace`, which includes
/// spaces, tabs and newlines) from `s`, returning the collapsed string along
/// with, for each retained character, the 1-based line number it came from
/// in `s`.
fn collapse_whitespace_with_line_map(s: &str) -> (String, Vec<usize>) {
    let mut collapsed = String::with_capacity(s.len());
    let mut line_map = Vec::with_capacity(s.len());
    let mut line_no = 1usize;
    for ch in s.chars() {
        if ch == '\n' {
            line_no += 1;
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        collapsed.push(ch);
        line_map.push(line_no);
    }
    (collapsed, line_map)
}

/// Whether `c` counts as an identifier character for the token-boundary
/// check (`[A-Za-z0-9_]`).
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Third-tier match: compare `content` and `needle` with all whitespace
/// (spaces, tabs, newlines) removed, find `needle`'s collapsed form as a
/// substring of `content`'s collapsed form, and map any match back to the
/// original file's 1-based line range (the first line containing the
/// match's start character through the line containing its last character).
///
/// A candidate match is discarded unless it falls on a token boundary in the
/// original text: if the needle's collapsed form starts with an identifier
/// character (`[A-Za-z0-9_]`), the collapsed character immediately before
/// the match must not also be an identifier character (and symmetrically at
/// the end), so a needle like `foo` cannot match inside `foobar`. A needle
/// that starts/ends with a non-identifier character (e.g. punctuation) has
/// no such restriction on that side.
///
/// Returns `CollapsedMatch::None` if `needle` collapses to the empty string,
/// if `content` does, or if no boundary-respecting match is found;
/// `CollapsedMatch::Multiple` naming every boundary-respecting match's line
/// range if more than one is found; `CollapsedMatch::Unique` otherwise.
fn collapsed_whitespace_find(content: &str, needle: &str) -> CollapsedMatch {
    let (needle_collapsed, _) = collapse_whitespace_with_line_map(needle);
    if needle_collapsed.is_empty() {
        return CollapsedMatch::None;
    }
    let (content_collapsed, line_map) = collapse_whitespace_with_line_map(content);
    if content_collapsed.is_empty() {
        return CollapsedMatch::None;
    }

    let content_chars: Vec<char> = content_collapsed.chars().collect();
    let needle_chars: Vec<char> = needle_collapsed.chars().collect();
    let needle_len = needle_chars.len();
    if content_chars.len() < needle_len {
        return CollapsedMatch::None;
    }

    let needle_starts_ident = needle_chars[0].is_ascii() && is_ident_char(needle_chars[0]);
    let needle_ends_ident =
        needle_chars[needle_len - 1].is_ascii() && is_ident_char(needle_chars[needle_len - 1]);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for start in 0..=(content_chars.len() - needle_len) {
        if content_chars[start..start + needle_len] != needle_chars[..] {
            continue;
        }
        if needle_starts_ident
            && start > 0
            && content_chars[start - 1].is_ascii()
            && is_ident_char(content_chars[start - 1])
        {
            continue;
        }
        let end = start + needle_len - 1;
        if needle_ends_ident
            && end + 1 < content_chars.len()
            && content_chars[end + 1].is_ascii()
            && is_ident_char(content_chars[end + 1])
        {
            continue;
        }
        let start_line = line_map[start];
        let end_line = line_map[end];
        ranges.push((start_line, end_line));
    }

    match ranges.len() {
        0 => CollapsedMatch::None,
        1 => CollapsedMatch::Unique {
            start_line: ranges[0].0,
            end_line: ranges[0].1,
        },
        _ => CollapsedMatch::Multiple(ranges),
    }
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
/// line of `old`, and format a hint showing a numbered window of the file
/// around it (so the model can copy the real text instead of guessing).
/// Returns an empty string if `old` has no non-empty line or the file has
/// no lines.
fn closest_line_hint(content: &str, old: &str) -> String {
    let needle = match old.lines().find(|l| !l.trim().is_empty()) {
        Some(l) => normalize_line(l),
        None => return String::new(),
    };
    if needle.is_empty() {
        return String::new();
    }
    let needle_tokens = tokenize(&needle);

    let file_lines: Vec<&str> = content.lines().collect();

    let mut best: Option<(usize, f64)> = None;
    for (i, line) in file_lines.iter().enumerate() {
        let line_norm = normalize_line(line);
        let score = line_score(&needle, &needle_tokens, &line_norm);
        let better = match best {
            None => true,
            Some((best_i, best_score)) => {
                score > best_score
                    || (score == best_score
                        && is_closer_prefix(
                            &needle,
                            &line_norm,
                            &normalize_line(file_lines[best_i - 1]),
                        ))
            }
        };
        if better {
            best = Some((i + 1, score));
        }
    }

    let line_no = match best {
        Some((line_no, _)) => line_no,
        None => return String::new(),
    };

    let total = file_lines.len();
    let old_line_count = old.lines().count();
    let window_start = line_no.saturating_sub(3).max(1);
    let mut window_end = line_no
        .saturating_add(12.max(old_line_count + 4))
        .min(total.max(1));
    // Cap the window at 40 lines.
    if window_end - window_start + 1 > 40 {
        window_end = window_start + 39;
    }

    let mut window = String::new();
    for (i, line) in file_lines[(window_start - 1)..window_end]
        .iter()
        .enumerate()
    {
        let n = window_start + i;
        window.push_str(&format!("{n:>5} | {line}\n"));
    }

    format!(
        "; closest match is near line {line_no}; the file there reads:\n{window}copy the lines you want to replace from this window into `old`, verbatim."
    )
}

/// Split a line into identifier-ish tokens: runs matching
/// `[A-Za-z_][A-Za-z0-9_]*`, plus any other non-whitespace punctuation-free
/// "words" (runs of non-whitespace, non-identifier characters collapsed to
/// single-character tokens). This lets scoring recognize e.g. `Skill` and
/// `trait` as shared tokens between an anchor and a candidate file line
/// regardless of surrounding punctuation.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        } else {
            // Punctuation / symbol: single-character token.
            tokens.push(c.to_string());
            i += 1;
        }
    }
    tokens
}

/// Score how well `line_norm` matches the anchor line `needle`/`needle_tokens`:
/// `2 * (number of needle tokens present in line_norm, in order, as a
/// subsequence) + 1 if line_norm starts with the same first token as needle
/// (after trimming) + a similarity tie-breaker scaled into `[0, 1)`.
fn line_score(needle: &str, needle_tokens: &[String], line_norm: &str) -> f64 {
    let line_tokens = tokenize(line_norm);

    // Count needle tokens present in line_tokens, in order, as a
    // subsequence (greedy scan is optimal for a simple presence count).
    let mut li = 0;
    let mut subseq_hits = 0usize;
    for nt in needle_tokens {
        while li < line_tokens.len() && &line_tokens[li] != nt {
            li += 1;
        }
        if li < line_tokens.len() {
            subseq_hits += 1;
            li += 1;
        }
    }

    let same_first_token = match (needle_tokens.first(), line_tokens.first()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };

    let sim = similarity(needle, line_norm);

    2.0 * subseq_hits as f64 + if same_first_token { 1.0 } else { 0.0 } + sim * 0.999
}

/// Tie-breaker: does `candidate`'s longest common prefix with `needle` beat
/// `current_best`'s? Used only when two lines have equal `line_score`.
fn is_closer_prefix(needle: &str, candidate: &str, current_best: &str) -> bool {
    common_prefix_len(needle, candidate) > common_prefix_len(needle, current_best)
}

/// Length (in chars) of the longest common prefix of `a` and `b`.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
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

/// File extensions (without the leading dot) for which a bracket-balance
/// check is performed after a successful edit.
const BRACE_CHECKED_EXTENSIONS: &[&str] = &[
    "rs", "c", "h", "cpp", "hpp", "java", "js", "ts", "tsx", "jsx", "go", "swift", "kt", "cs",
    "scala",
];

/// Prefixes (after trimming leading whitespace) that mark a trimmed line as
/// "looks like a definition" for [`looks_like_definition`]. Checked in
/// order; the first match wins. `let ` is handled separately since it only
/// counts with no leading indentation.
const DEFINITION_PREFIXES: &[&str] = &[
    "use ",
    "import ",
    "from ",
    "pub(crate) fn ",
    "pub async fn ",
    "async fn ",
    "pub fn ",
    "fn ",
    "async def ",
    "def ",
    "class ",
    "pub struct ",
    "struct ",
    "pub enum ",
    "enum ",
    "impl ",
    "pub trait ",
    "trait ",
    "pub const ",
    "const ",
    "pub static ",
    "static ",
    "pub type ",
    "type ",
    "pub mod ",
    "mod ",
];

/// Whether `line` (as it appears in a file, i.e. with its original leading
/// indentation and any trailing whitespace) "looks like a definition": an
/// import, a function/struct/enum/trait/impl/const/static/type/mod
/// declaration, a top-level `let`, a Rust struct field at 4-space
/// indentation, or an all-caps assignment at column 0. Used to decide
/// whether inserting a duplicate of this line is likely to create a real
/// duplicate definition (as opposed to duplicating an incidental line like
/// `}` or `return x;` that legitimately recurs).
fn looks_like_definition(line: &str) -> bool {
    let trimmed_end = line.trim_end();
    if trimmed_end.trim().is_empty() {
        return false;
    }
    let trimmed = trimmed_end.trim_start();
    let indent_len = trimmed_end.len() - trimmed.len();

    for prefix in DEFINITION_PREFIXES {
        if trimmed.starts_with(prefix) {
            return true;
        }
    }

    // `let ` only counts at column 0 (no leading indentation).
    if indent_len == 0 && trimmed.starts_with("let ") {
        return true;
    }

    // Rust struct field pattern: `<ident>: <type>,` at exactly 4-space
    // indentation.
    if indent_len == 4 && is_struct_field_line(trimmed) {
        return true;
    }

    // Python/JS assignment at column 0: `<IDENT> = ...` where IDENT is all
    // caps (allowing digits and underscores, e.g. `TAX_RATE`).
    if indent_len == 0 && is_all_caps_assignment(trimmed) {
        return true;
    }

    false
}

/// Whether `trimmed` (already leading/trailing-whitespace-trimmed) matches
/// the shape `<ident>: <type>,` — a Rust struct field.
fn is_struct_field_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_suffix(',') else {
        return false;
    };
    let Some((name, ty)) = rest.split_once(':') else {
        return false;
    };
    let name = name.trim();
    let ty = ty.trim();
    if name.is_empty() || ty.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `trimmed` (already trimmed) matches `<IDENT> = ...` where IDENT
/// is all-caps (letters, digits, underscores; at least one letter).
fn is_all_caps_assignment(trimmed: &str) -> bool {
    let Some(eq_idx) = trimmed.find('=') else {
        return false;
    };
    // Reject `==`, `!=`, `<=`, `>=` masquerading as assignment, and augmented
    // assignment operators like `+=` are still fine (name part excludes the
    // operator character).
    if trimmed[eq_idx + 1..].starts_with('=') {
        return false;
    }
    let name_part = trimmed[..eq_idx].trim_end();
    let name = name_part
        .strip_suffix(['+', '-', '*', '/'])
        .unwrap_or(name_part);
    if name.is_empty() {
        return false;
    }
    let has_letter = name.chars().any(|c| c.is_ascii_alphabetic());
    let all_valid = name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    has_letter && all_valid
}

/// Refuses an `after`/`before` insertion where a non-blank line of the text
/// to insert already exists in the file (exact match after `trim_end`,
/// leading indentation included) and looks like a definition. A small model
/// that means "replace" but sends an insert leaves the old definition in
/// place: a duplicate import, a duplicate constant, two overlapping copies
/// of a function. Unlike [`check_not_already_present`] (which requires a
/// multi-line block match), this fires on a single matching definition
/// line, since a single duplicated `use` or `const` is exactly the failure
/// mode this exists to catch.
fn check_no_duplicate_definition(content: &str, new: &str, path: &str) -> Result<(), ToolError> {
    let file_lines: Vec<&str> = content.lines().collect();

    for new_line in new.lines() {
        let new_trimmed = new_line.trim_end();
        if new_trimmed.trim().is_empty() {
            continue;
        }
        if !looks_like_definition(new_line) {
            continue;
        }
        for (i, file_line) in file_lines.iter().enumerate() {
            if file_line.trim_end() == new_trimmed {
                let line_no = i + 1;
                return Err(ToolError::ExecutionFailed(format!(
                    "line {line_no} of {path} already has `{}`; your insert would duplicate it. Use old/new to replace the existing code instead.",
                    new_trimmed.trim()
                )));
            }
        }
    }

    Ok(())
}

/// Bracket kinds tracked by [`bracket_balance`], in the order their deltas
/// are reported: braces, brackets, then parens.
const BRACKET_PAIRS: [(char, char); 3] = [('{', '}'), ('[', ']'), ('(', ')')];

/// Counts of (`{` minus `}`, `[` minus `]`, `(` minus `)`) in `text`,
/// ignoring characters inside string literals and comments as far as a
/// simple single-pass scanner can tell: double-quoted strings with
/// backslash escapes, single-quoted char literals (1-2 chars, e.g. `'a'` or
/// `'\n'`), `//` and `#` line comments, and `/* */` block comments. This is
/// not a real tokenizer (it doesn't know about raw strings, nested block
/// comments, etc.) but is good enough to avoid false positives from braces
/// that appear inside a string or comment.
fn bracket_balance(text: &str) -> (i64, i64, i64) {
    let mut braces: i64 = 0;
    let mut brackets: i64 = 0;
    let mut parens: i64 = 0;

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            '\'' => {
                // Char literal: '<char>' or '\<escape>' — at most a couple
                // chars before the closing quote. If no closing quote is
                // found within a few characters, treat the `'` as a stray
                // character (e.g. a lifetime or apostrophe) and move on.
                let start = i;
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '\\' {
                    j += 1;
                }
                if j < chars.len() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '\'' {
                    i = j + 1;
                    continue;
                }
                i = start + 1;
                continue;
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '#' => {
                i += 1;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                continue;
            }
            '{' => braces += 1,
            '}' => braces -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            '(' => parens += 1,
            ')' => parens -= 1,
            _ => {}
        }
        i += 1;
    }

    (braces, brackets, parens)
}

/// After a successful edit to a file whose extension is one of
/// `BRACE_CHECKED_EXTENSIONS`, compares the `{}`/`[]`/`()` balance before
/// and after the edit (via [`bracket_balance`]). Returns a note line
/// listing every bracket kind whose balance *changed* (regardless of
/// whether the file was already unbalanced before the edit); returns `None`
/// when no kind's balance changed, including when the file was already
/// unbalanced and the edit didn't touch that imbalance. Never blocks the
/// edit — this is purely informational.
fn brace_balance_note(path: &str, before: &str, after: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?;
    if !BRACE_CHECKED_EXTENSIONS.contains(&ext) {
        return None;
    }

    let before_balance = bracket_balance(before);
    let after_balance = bracket_balance(after);

    let before_arr = [before_balance.0, before_balance.1, before_balance.2];
    let after_arr = [after_balance.0, after_balance.1, after_balance.2];

    let mut deltas: Vec<String> = Vec::new();
    for (idx, (open, close)) in BRACKET_PAIRS.iter().enumerate() {
        let delta = after_arr[idx] - before_arr[idx];
        if delta == 0 {
            continue;
        }
        let ch = if delta > 0 { open } else { close };
        deltas.push(format!("{delta:+} `{ch}`"));
    }

    if deltas.is_empty() {
        return None;
    }

    Some(format!(
        "note: this edit changed the file's brace balance ({}); check the edited region before running tests.",
        deltas.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_definition_positives() {
        assert!(looks_like_definition("use foo::Bar;"));
        assert!(looks_like_definition("fn x() {"));
        assert!(looks_like_definition("pub struct S {"));
        assert!(looks_like_definition("const A: u32 = 1;"));
        assert!(looks_like_definition("def f(x):"));
        assert!(looks_like_definition("TAX_RATE = 0.08"));
        assert!(looks_like_definition("    name: String,"));
    }

    #[test]
    fn looks_like_definition_negatives() {
        assert!(!looks_like_definition("    return x"));
        assert!(!looks_like_definition("    let y = 2;"));
        assert!(!looks_like_definition("}"));
        assert!(!looks_like_definition(""));
        assert!(!looks_like_definition("// comment"));
        assert!(!looks_like_definition("x = 1"));
    }

    #[test]
    fn looks_like_definition_more_prefixes() {
        assert!(looks_like_definition("import os"));
        assert!(looks_like_definition("from foo import bar"));
        assert!(looks_like_definition("pub fn f() {"));
        assert!(looks_like_definition("pub(crate) fn f() {"));
        assert!(looks_like_definition("async fn f() {"));
        assert!(looks_like_definition("pub async fn f() {"));
        assert!(looks_like_definition("async def f():"));
        assert!(looks_like_definition("class Foo:"));
        assert!(looks_like_definition("pub enum E {"));
        assert!(looks_like_definition("enum E {"));
        assert!(looks_like_definition("impl Foo {"));
        assert!(looks_like_definition("trait Bar {"));
        assert!(looks_like_definition("pub trait Bar {"));
        assert!(looks_like_definition("pub const A: u32 = 1;"));
        assert!(looks_like_definition("static A: u32 = 1;"));
        assert!(looks_like_definition("pub static A: u32 = 1;"));
        assert!(looks_like_definition("type Foo = Bar;"));
        assert!(looks_like_definition("pub type Foo = Bar;"));
        assert!(looks_like_definition("mod foo;"));
        assert!(looks_like_definition("pub mod foo;"));
        assert!(looks_like_definition("let y = 2;")); // column 0
    }

    #[test]
    fn looks_like_definition_indented_let_is_not_a_definition() {
        // `let` only counts with no leading indentation.
        assert!(!looks_like_definition("    let y = 2;"));
        assert!(!looks_like_definition("\tlet y = 2;"));
    }

    #[test]
    fn looks_like_definition_lowercase_assignment_is_not_a_definition() {
        assert!(!looks_like_definition("tax_rate = 0.08"));
        assert!(!looks_like_definition("Tax_Rate = 0.08"));
    }

    #[test]
    fn looks_like_definition_struct_field_requires_four_space_indent() {
        assert!(looks_like_definition("    name: String,"));
        assert!(!looks_like_definition("  name: String,"));
        assert!(!looks_like_definition("        name: String,"));
        // No trailing comma: not a field.
        assert!(!looks_like_definition("    name: String"));
    }

    #[test]
    fn bracket_balance_counts_all_three_kinds() {
        assert_eq!(bracket_balance("{[()]}"), (0, 0, 0));
        assert_eq!(bracket_balance("{{{"), (3, 0, 0));
        assert_eq!(bracket_balance("]]"), (0, -2, 0));
        assert_eq!(bracket_balance("((("), (0, 0, 3));
    }

    #[test]
    fn bracket_balance_ignores_string_literal_contents() {
        // A lone `}` inside a string literal must not count.
        assert_eq!(bracket_balance(r#"let s = "}";"#), (0, 0, 0));
        assert_eq!(bracket_balance(r#"let s = "{ [ (";"#), (0, 0, 0));
    }

    #[test]
    fn bracket_balance_handles_escaped_quotes_in_strings() {
        // The escaped quote must not end the string early, so the `}` right
        // after it is still inside the string and ignored.
        assert_eq!(bracket_balance(r#"let s = "\"}"; "#), (0, 0, 0));
    }

    #[test]
    fn bracket_balance_ignores_char_literals() {
        assert_eq!(bracket_balance("let c = '}';"), (0, 0, 0));
        assert_eq!(bracket_balance("let c = '\\n';"), (0, 0, 0));
    }

    #[test]
    fn bracket_balance_ignores_line_comments() {
        assert_eq!(bracket_balance("let x = 1; // }"), (0, 0, 0));
        assert_eq!(bracket_balance("x = 1  # }"), (0, 0, 0));
    }

    #[test]
    fn bracket_balance_ignores_block_comments() {
        assert_eq!(bracket_balance("/* { [ ( */ let x = 1;"), (0, 0, 0));
    }

    #[test]
    fn bracket_balance_counts_real_code_outside_comments_and_strings() {
        let text = "fn f() {\n    let s = \"}\"; // }\n}\n";
        assert_eq!(bracket_balance(text), (0, 0, 0));
    }
}
