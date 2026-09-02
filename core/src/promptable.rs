//! The prompt gate: a sealed trait so only core-blessed types can ever be
//! rendered into a prompt. `Reasoning` and `ToolOutputFull` deliberately do
//! not implement it.

use serde::{Deserialize, Serialize};

use crate::tokenizer::Tokenizer;

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Anything that can render itself into a prompt section. Sealed: only types
/// defined in `statie-core` may implement it.
pub trait Promptable: sealed::Sealed {
    fn render(&self) -> String;
}

/// Model reasoning / chain-of-thought text. `Display`, `Serialize`,
/// `Deserialize`, `AsRef<str>`, `Clone` — but never `Promptable`. It is a
/// `LogEntry` payload only; it must never reach the model again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reasoning(String);

impl Reasoning {
    pub fn new(s: impl Into<String>) -> Self {
        Reasoning(s.into())
    }
}

impl std::fmt::Display for Reasoning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Reasoning {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A tool's full, untruncated output. Logged whole; `Observation` is its
/// truncated, prompt-safe view. Never `Promptable`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolOutputFull(String);

impl ToolOutputFull {
    pub fn new(s: String) -> Self {
        ToolOutputFull(s)
    }
}

impl std::fmt::Display for ToolOutputFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ToolOutputFull {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A bounded, prompt-safe view of a tool's output or a harness-generated
/// rejection/park message. No public constructor: only `truncate` and the
/// small set of `pub(crate)` factories below ever produce one.
#[derive(Clone, Debug, Serialize)]
pub struct Observation(String);

#[allow(dead_code)]
const TRUNCATE_HEAD_PCT: f64 = 0.60;
#[allow(dead_code)]
const TRUNCATE_TAIL_PCT: f64 = 0.40;

// The constructors below (besides `as_str`) are the harness's only ways to
// produce an `Observation`. `truncate` is exercised directly by this
// module's unit tests; the rest are forward API surface for the loop unit
// (A7: observations are the only history channel into the prompt), so they
// are unused by a plain, non-test build until that unit lands.
impl Observation {
    /// Head/tail truncation: keep ~60% of the cap from the front and ~40%
    /// from the back, measured in tokens via `tok`, joined by a
    /// `\n…[truncated N chars]…\n` marker. `full` under the cap is returned
    /// unchanged.
    #[allow(dead_code)]
    pub(crate) fn truncate(full: &ToolOutputFull, cap_tokens: usize, tok: &dyn Tokenizer) -> Self {
        let text = full.as_ref();
        if tok.count(text) <= cap_tokens {
            return Observation(text.to_string());
        }

        // Binary-search the largest character prefix/suffix (on char
        // boundaries) whose token count fits the allotted share of the cap.
        let chars: Vec<char> = text.chars().collect();
        let head_budget = ((cap_tokens as f64) * TRUNCATE_HEAD_PCT).floor() as usize;
        let tail_budget = ((cap_tokens as f64) * TRUNCATE_TAIL_PCT).floor() as usize;

        let head_len = largest_prefix_within(&chars, head_budget, tok);
        let tail_len = largest_suffix_within(&chars, tail_budget, tok);

        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        let truncated_chars = chars.len().saturating_sub(head_len + tail_len);

        let marker = format!("\n…[truncated {truncated_chars} chars]…\n");
        Observation(format!("{head}{marker}{tail}"))
    }

    /// A harness-authored rejection message (e.g. "output rejected: ...").
    #[allow(dead_code)]
    pub(crate) fn rejection(reason: &str) -> Self {
        Observation(format!("output rejected: {reason}"))
    }

    /// The tool named `name` was aborted mid-execution by a cancellation.
    #[allow(dead_code)]
    pub(crate) fn tool_aborted(name: &str) -> Self {
        Observation(format!("tool `{name}` aborted: cancelled"))
    }

    /// The session parked waiting for the user.
    #[allow(dead_code)]
    pub(crate) fn user_parked() -> Self {
        Observation("session parked: waiting for user input".to_string())
    }

    /// A plain harness-authored message (e.g. from a user-facing note).
    #[allow(dead_code)]
    pub(crate) fn user_message(text: &str) -> Self {
        Observation(text.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[allow(dead_code)]
fn largest_prefix_within(chars: &[char], budget: usize, tok: &dyn Tokenizer) -> usize {
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let s: String = chars[..mid].iter().collect();
        if tok.count(&s) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[allow(dead_code)]
fn largest_suffix_within(chars: &[char], budget: usize, tok: &dyn Tokenizer) -> usize {
    let n = chars.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let s: String = chars[n - mid..].iter().collect();
        if tok.count(&s) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

impl sealed::Sealed for Observation {}
impl Promptable for Observation {
    fn render(&self) -> String {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::HeuristicTokenizer;

    #[test]
    fn short_text_unchanged() {
        let full = ToolOutputFull::new("hello world".to_string());
        let tok = HeuristicTokenizer;
        let obs = Observation::truncate(&full, 100, &tok);
        assert_eq!(obs.as_str(), "hello world");
    }

    #[test]
    fn long_text_is_truncated_with_marker_and_preserves_head_and_tail() {
        let head = "A".repeat(2000);
        let middle = "B".repeat(2000);
        let tail = "C".repeat(2000);
        let full_text = format!("{head}{middle}{tail}");
        let full = ToolOutputFull::new(full_text.clone());
        let tok = HeuristicTokenizer;
        let cap = 200; // tokens, i.e. ~800 chars via HeuristicTokenizer

        let obs = Observation::truncate(&full, cap, &tok);
        let rendered = obs.as_str();

        // At most the cap (allow the marker itself some room, so just check
        // it's meaningfully smaller than the input and doesn't blow past a
        // generous multiple of the cap in characters).
        assert!(tok.count(rendered) <= cap + 50, "token count = {}", tok.count(rendered));

        assert!(rendered.contains("[truncated"));
        assert!(rendered.starts_with('A'));
        assert!(rendered.ends_with('C'));
        assert!(full_text.starts_with(rendered.chars().take(10).collect::<String>().as_str()));
        assert!(
            full_text.ends_with(
                rendered
                    .chars()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
                    .as_str()
            )
        );
    }
}
