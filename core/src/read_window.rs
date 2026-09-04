//! A small, tolerant parser for `read_file`'s output header (see
//! `tools/src/read_file.rs`): `"{path} ({total} lines, showing {from}-{to})"`,
//! used by session rule 7.8's read-paging special case (`core/src/session.rs`)
//! to tell whether a repeated `read_file` call's previous window was
//! truncated (`to < total`) and, if so, what line to page forward from. This
//! module only reads the tool's existing output text -- it never changes
//! `read_file`'s output format.

/// A parsed `read_file` header: `total` lines in the file, and the `from-to`
/// (1-based, inclusive) line range the window actually showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadWindowHeader {
    pub total: u64,
    pub from: u64,
    pub to: u64,
}

impl ReadWindowHeader {
    /// True when this window reached the end of the file: nothing left to
    /// page forward to.
    pub fn at_end(&self) -> bool {
        self.to >= self.total
    }
}

/// Parses the first line of a `read_file` output for the `(N lines, showing
/// a-b)` suffix `tools/src/read_file.rs`'s `call` renders after the path,
/// e.g. `"tui/src/headless.rs (198 lines, showing 1-100)"`. Tolerant of
/// anything preceding `(` (the path may itself contain parentheses) by
/// scanning for the *last* `(...)` group on the first line that matches the
/// expected shape; returns `None` if no line matches, or the numbers don't
/// parse, or `from > to` (which would make "the next window" meaningless).
pub fn parse_read_header(text: &str) -> Option<ReadWindowHeader> {
    let first_line = text.lines().next()?;
    let open = first_line.rfind('(')?;
    let close_rel = first_line[open..].find(')')?;
    let inner = &first_line[open + 1..open + close_rel];

    // inner: "{total} lines, showing {from}-{to}"
    let (total_part, rest) = inner.split_once(" lines, showing ")?;
    let total: u64 = total_part.trim().parse().ok()?;
    let (from_part, to_part) = rest.split_once('-')?;
    let from: u64 = from_part.trim().parse().ok()?;
    let to: u64 = to_part.trim().parse().ok()?;

    if from > to {
        return None;
    }

    Some(ReadWindowHeader { total, from, to })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_truncated_window() {
        let text = "tui/src/headless.rs (198 lines, showing 1-100)\n    1 | foo\n";
        let h = parse_read_header(text).expect("should parse");
        assert_eq!(
            h,
            ReadWindowHeader {
                total: 198,
                from: 1,
                to: 100
            }
        );
        assert!(!h.at_end());
    }

    #[test]
    fn parses_a_window_at_the_end_of_the_file() {
        let text = "tui/src/headless.rs (198 lines, showing 101-198)\n";
        let h = parse_read_header(text).expect("should parse");
        assert_eq!(
            h,
            ReadWindowHeader {
                total: 198,
                from: 101,
                to: 198
            }
        );
        assert!(h.at_end());
    }

    #[test]
    fn parses_a_window_that_exactly_covers_a_single_line_file() {
        let text = "a.rs (1 lines, showing 1-1)\n    1 | only line\n";
        let h = parse_read_header(text).expect("should parse");
        assert_eq!(
            h,
            ReadWindowHeader {
                total: 1,
                from: 1,
                to: 1
            }
        );
        assert!(h.at_end());
    }

    #[test]
    fn rejects_unparsable_headers() {
        assert!(parse_read_header("").is_none());
        assert!(parse_read_header("no header here at all").is_none());
        assert!(parse_read_header("a.rs (not a number lines, showing 1-100)").is_none());
        assert!(parse_read_header("a.rs (100 lines, showing x-y)").is_none());
        assert!(parse_read_header("a.rs (100 lines, showing 100-1)").is_none());
        assert!(parse_read_header("a.rs 100 lines showing 1-100").is_none());
    }

    #[test]
    fn tolerates_a_path_containing_parentheses() {
        let text = "weird(name).rs (50 lines, showing 1-50)\n";
        let h = parse_read_header(text).expect("should parse");
        assert_eq!(
            h,
            ReadWindowHeader {
                total: 50,
                from: 1,
                to: 50
            }
        );
    }

    #[test]
    fn rejects_empty_output() {
        assert!(parse_read_header("(tool `read_file` returned no output)").is_none());
    }
}
