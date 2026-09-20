# Unit 45: notes over budget are cut to the budget, not dropped

## Background (read, do not restate)
`core/src/session.rs` `drain_pending_maintainer`: when the maintainer's `notes` count more tokens
than `budget` (`HarnessState.budgets.notes`, 400 in the bench configs), the whole result is
discarded: a `maintainer_error` system entry with reason `notes over budget: {n} > {budget}`,
`sink.maintainer_failed`, and return; the previous notes stand and `next` is lost. Bench
`qwen_json_short_b26`: 51 such discards; `qwen_json_long_b24`: 170; the sweep rows: 55 in 32
runs; most overshoots are 3 to 60 tokens over 400.

## Spec
1. Replace the discard with a cut. If `notes_tokens > budget`: cut `out.notes` to at most
   `budget` tokens, preferring the last sentence end (`.`, `!`, `?`, or a newline) at or before
   the cut point that leaves at least half the budget; otherwise cut at the token boundary the
   tokenizer gives (find the existing truncate-to-tokens helper used for observations and
   reuse it). Append nothing. Log a system entry `{"event": "maintainer_notes_cut", "from":
   <tokens>, "to": <tokens>}` at the maintainer's step, then continue with the cut notes exactly
   as an in-budget result would (the same `maintainer` event, the same `next` checks).
2. `sink.maintainer_failed` is not called for a cut. No other maintainer behaviour changes.
3. Export the cut as a `pub fn cut_notes_to_budget(notes: &str, budget: usize, tokenizer: &dyn
   Tokenizer) -> (String, usize /*tokens after*/)` in `core/src/maintainer.rs` so it is unit-testable.

## Acceptance
- `core/tests/it/maintainer.rs`: (a) a scripted maintainer returning notes of exactly
  `budget + 30` tokens (by the test tokenizer) with a sentence end inside the budget: the
  committed notes equal the text up to that sentence end (exact string), a
  `maintainer_notes_cut` entry carries the exact `from`/`to`, the `maintainer` event is logged
  as for an in-budget result, and no `maintainer_error` entry exists; (b) notes with no sentence
  end in the second half: cut at the token boundary (exact string from the helper); (c) notes
  within budget: unchanged, no cut entry (the existing test). (d) unit tests for
  `cut_notes_to_budget` on three exact inputs.
- The existing test asserting the discard (`notes over budget`) is rewritten to assert the cut
  (exact new values); report the old and new assertions. Nothing else is loosened or deleted.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`.

## Boundaries
`core/src/session.rs`, `core/src/maintainer.rs`, `core/src/lib.rs` (export), the core test
modules named and any test the compiler or a failing assertion names. No prompt, skills, or
tools changes. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo test -p statie-core --test it`,
`cargo test -p statie-skills --test it`, `cargo test -p statie --bin statie`.
