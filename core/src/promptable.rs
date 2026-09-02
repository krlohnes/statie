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

/// Extra token headroom subtracted from the marker's measured cost, to
/// absorb the fact that head+tail concatenated can tokenize to slightly
/// more tokens than the sum of their standalone counts (BPE merges across
/// the boundary). See `truncate`.
const MARKER_SAFETY_TOKENS: usize = 4;

/// What happened when a tool output overflowed the observation budget and
/// had to be truncated: the numbers `truncate_with_report`'s caller can use
/// to tell the model (via `HarnessState::last_overflow`) what was cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow {
    pub total_chars: usize,
    pub total_tokens: usize,
    pub head_tokens: usize,
    pub tail_tokens: usize,
}

// The constructors below (besides `as_str`) are the harness's only ways to
// produce an `Observation`. `truncate` is exercised directly by this
// module's unit tests; the rest are forward API surface for the loop unit
// (A7: observations are the only history channel into the prompt), so they
// are unused by a plain, non-test build until that unit lands.
impl Observation {
    /// Head/tail truncation: keep ~60% of the cap from the front and ~40%
    /// from the back, measured in tokens via `tok`, joined by a
    /// `[harness] output overflowed: ...` instruction marker. `full` under
    /// the cap is returned unchanged.
    ///
    /// The result is verified against `cap_tokens` after construction and
    /// shrunk further if it still overshoots (tokenization of head+tail
    /// concatenated is not guaranteed to equal the sum of their standalone
    /// token counts). This function never returns an `Observation` whose
    /// rendered token count, per `tok`, exceeds `cap_tokens`.
    #[allow(dead_code)]
    pub(crate) fn truncate(full: &ToolOutputFull, cap_tokens: usize, tok: &dyn Tokenizer) -> Self {
        Self::truncate_with_report(full, cap_tokens, tok).0
    }

    /// Same as `truncate`, but also reports what happened: `Some(Overflow)`
    /// when the output had to be cut, `None` when it fit under `cap_tokens`
    /// unchanged.
    pub(crate) fn truncate_with_report(
        full: &ToolOutputFull,
        cap_tokens: usize,
        tok: &dyn Tokenizer,
    ) -> (Self, Option<Overflow>) {
        let text = full.as_ref();
        let total_tokens = tok.count(text);
        if total_tokens <= cap_tokens {
            return (Observation(text.to_string()), None);
        }

        let total_chars = text.chars().count();
        let chars: Vec<char> = text.chars().collect();

        // Measure the marker using the worst-case (largest possible)
        // numbers as an upper bound, since the real values aren't known
        // until after cutting -- they only ever shrink from here, and
        // digit count is the only thing that could grow the marker's own
        // token count, which this upper bound also covers.
        let worst_marker = harness_overflow_marker(
            total_chars,
            total_tokens,
            cap_tokens,
            cap_tokens,
            cap_tokens,
        );
        let marker_cost = tok.count(&worst_marker);

        // If the cap can't even hold the marker plus safety margin, there's
        // no room for head/tail content at all: fall back to a bare head
        // cut to fit cap_tokens exactly.
        if cap_tokens <= marker_cost + MARKER_SAFETY_TOKENS {
            let head_len = largest_prefix_within(&chars, cap_tokens, tok);
            let head: String = chars[..head_len].iter().collect();
            let head_tokens = tok.count(&head);
            return (
                Observation(head),
                Some(Overflow {
                    total_chars,
                    total_tokens,
                    head_tokens,
                    tail_tokens: 0,
                }),
            );
        }

        let usable = cap_tokens - marker_cost - MARKER_SAFETY_TOKENS;
        let mut head_budget = ((usable as f64) * TRUNCATE_HEAD_PCT).floor() as usize;
        let mut tail_budget = usable - head_budget;

        loop {
            let head_len = largest_prefix_within(&chars, head_budget, tok);
            let tail_len = largest_suffix_within(&chars, tail_budget, tok);

            let head: String = chars[..head_len].iter().collect();
            let tail: String = chars[chars.len() - tail_len..].iter().collect();

            let head_tokens = tok.count(&head);
            let tail_tokens = tok.count(&tail);
            let marker = harness_overflow_marker(
                total_chars,
                total_tokens,
                cap_tokens,
                head_tokens,
                tail_tokens,
            );
            let result = format!("{head}{marker}{tail}");

            if tok.count(&result) <= cap_tokens {
                return (
                    Observation(result),
                    Some(Overflow {
                        total_chars,
                        total_tokens,
                        head_tokens,
                        tail_tokens,
                    }),
                );
            }

            // Shrink the tail by 10% of the original usable budget first;
            // once the tail is exhausted, shrink the head the same way.
            // `usable` is > 0 here (checked above), so this step always
            // makes progress and the loop terminates.
            let step = (usable / 10).max(1);
            if tail_budget > 0 {
                tail_budget = tail_budget.saturating_sub(step);
            } else if head_budget > 0 {
                head_budget = head_budget.saturating_sub(step);
            } else {
                // Both budgets exhausted: nothing left to cut but the
                // marker itself no longer fits either. Fall back to a bare
                // head cut to fit cap_tokens exactly.
                let head_len = largest_prefix_within(&chars, cap_tokens, tok);
                let head: String = chars[..head_len].iter().collect();
                let head_tokens = tok.count(&head);
                return (
                    Observation(head),
                    Some(Overflow {
                        total_chars,
                        total_tokens,
                        head_tokens,
                        tail_tokens: 0,
                    }),
                );
            }
        }
    }

    /// Re-renders `self` truncated to fit `cap_tokens`, if it doesn't
    /// already. A no-op (returns `self` unchanged) when already within
    /// budget.
    pub(crate) fn clamp(self, cap_tokens: usize, tok: &dyn Tokenizer) -> Observation {
        if tok.count(&self.0) <= cap_tokens {
            return self;
        }
        Observation::truncate(&ToolOutputFull::new(self.0), cap_tokens, tok)
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

/// Builds the `[harness] output overflowed: ...` instruction marker that
/// replaces the cut middle of an oversized tool output. `head_tokens` and
/// `tail_tokens` are the token counts of the head/tail strings actually
/// kept; `cap` is the observation budget itself.
fn harness_overflow_marker(
    total_chars: usize,
    total_tokens: usize,
    cap: usize,
    head_tokens: usize,
    tail_tokens: usize,
) -> String {
    format!(
        "\n[harness] output overflowed: {total_chars} chars (~{total_tokens} tokens) against a \
         {cap}-token observation budget; showing the first {head_tokens} and last {tail_tokens} \
         tokens. Do not re-run the same command: narrow it (pipe shell output through \
         grep/head, use list_files with a path, search with a pattern, read_file with \
         offset/limit) or call `filter_last_tool_call` with a regex to pull specific lines from \
         this output.\n"
    )
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

        assert!(
            tok.count(rendered) <= cap,
            "token count = {}",
            tok.count(rendered)
        );

        assert!(rendered.contains("[harness] output overflowed"));
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

    /// `Observation::truncate` must never render over `cap_tokens`,
    /// regardless of input size, character mix, or how small the cap is,
    /// using the real tokenizer (the one that surfaced the live bug: BPE
    /// merges across the head/tail/marker boundary can make the whole cost
    /// more than the sum of its parts).
    #[test]
    fn truncate_never_exceeds_cap_for_many_caps_and_inputs() {
        use crate::tokenizer::TiktokenTokenizer;

        let tok = TiktokenTokenizer;
        let caps = [8usize, 12, 20, 33, 50, 100, 257, 1000, 2000];
        let sizes = [0usize, 1, 50, 5_000, 60_000];

        let ascii = "The quick brown fox jumps over the lazy dog. ";
        let punct = "!@#$%^&*()_+-=[]{}|;:'\",.<>/?`~\\ ";
        let code = "fn main() { let x = vec![1, 2, 3]; println!(\"{:?}\", x); }\n";
        let multibyte = "é€…漢字";

        let mut inputs: Vec<(&str, String)> = Vec::new();
        for &size in &sizes {
            let mixed: String = [ascii, punct, code, multibyte]
                .into_iter()
                .cycle()
                .flat_map(|s| s.chars())
                .take(size)
                .collect();
            inputs.push(("mixed", mixed));

            let mb: String = multibyte.chars().cycle().take(size).collect();
            inputs.push(("multibyte", mb));
        }

        for cap in caps {
            for (label, text) in &inputs {
                let full = ToolOutputFull::new(text.clone());
                let obs = Observation::truncate(&full, cap, &tok);
                let rendered = obs.render();
                let count = tok.count(&rendered);
                assert!(
                    count <= cap,
                    "cap={cap} input={label} len={} produced {count} tokens (rendered len {})",
                    text.chars().count(),
                    rendered.chars().count(),
                );
            }
        }
    }

    #[test]
    fn truncate_with_report_returns_numbers() {
        use crate::tokenizer::TiktokenTokenizer;

        let tok = TiktokenTokenizer;
        let text = "x".repeat(10_000);
        let full = ToolOutputFull::new(text.clone());
        let cap = 200;

        let (obs, report) = Observation::truncate_with_report(&full, cap, &tok);
        let report = report.expect("expected an overflow report for oversized input");

        assert_eq!(report.total_chars, text.chars().count());
        assert_eq!(report.total_tokens, tok.count(&text));
        assert!(
            report.head_tokens + report.tail_tokens <= cap,
            "head_tokens={} tail_tokens={} cap={cap}",
            report.head_tokens,
            report.tail_tokens
        );
        assert!(tok.count(obs.render().as_str()) <= cap);
    }

    #[test]
    fn truncate_with_report_returns_none_when_within_budget() {
        let tok = HeuristicTokenizer;
        let full = ToolOutputFull::new("hello world".to_string());
        let (obs, report) = Observation::truncate_with_report(&full, 100, &tok);
        assert!(report.is_none());
        assert_eq!(obs.as_str(), "hello world");
    }

    #[test]
    fn clamp_shortens_oversized_rejection() {
        let tok = HeuristicTokenizer;
        let reason = "x".repeat(10_000);
        let obs = Observation::rejection(&reason);
        assert!(tok.count(&obs.render()) > 200);

        // A cap generous enough for the (much longer, instructional) marker
        // to still fit under `HeuristicTokenizer`'s coarse char/4 estimate;
        // `clamp_shortens_oversized_rejection` at a tiny cap is covered
        // separately by `clamp_falls_back_to_bare_head_when_marker_cannot_fit`.
        let clamped = obs.clamp(200, &tok);
        assert!(tok.count(&clamped.render()) <= 200);
        assert!(clamped.render().contains("[harness] output overflowed"));
    }

    #[test]
    fn clamp_falls_back_to_bare_head_when_marker_cannot_fit() {
        // At a cap too small to hold the marker at all, `clamp` still never
        // exceeds the cap -- it just can't include the instructional text.
        let tok = HeuristicTokenizer;
        let reason = "x".repeat(10_000);
        let obs = Observation::rejection(&reason);
        let clamped = obs.clamp(10, &tok);
        assert!(tok.count(&clamped.render()) <= 10);
    }

    #[test]
    fn clamp_is_noop_when_already_within_budget() {
        let tok = HeuristicTokenizer;
        let obs = Observation::rejection("short reason");
        let clamped = obs.clone().clamp(1000, &tok);
        assert_eq!(clamped.render(), obs.render());
    }
}
