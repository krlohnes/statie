# Unit 51: a permission ask is not a question

## Background (read, do not restate)
Three Qwen runs parked on builds 32 and 33 with an `ask_user` that proposed the next edit and
asked leave to make it: "Would you like me to proceed with this change now?", "Would you like me
to proceed with adding these missing arms?", "Should I create it now with the required
functions, or do you want me to first check ...". Each came right after one tool error. The
message contains `?`, so `ask_user_statement_note` (`skills/src/coding/mod.rs`) lets it through
and the run parks on a human who already asked for the work. The existing chain:
`ask_user_empty_note` (empty message) then `ask_user_statement_note` (no `?`), both converting
the action to a harness `Continue` with the patch kept and `next_skill` stripped.

## Spec
1. A third sibling, `ask_user_permission_note(out) -> Option<String>`, called from
   `ask_user_empty_note` after `ask_user_statement_note` returns `None` (so every caller of the
   chain gets it: `implement` via `premature_done_note`'s fallthrough, `plan` and `review`
   directly). Fires when the action is `AskUser` and the message is a permission ask: split
   the message into question sentences (the text between one sentence end `.`/`!`/`?`/start
   and the next `?`), trim and lowercase each; the message is a permission ask if any such
   sentence starts with one of `pub const PERMISSION_ASK_PREFIXES: &[&str] = &["would you
   like me to", "do you want me to", "want me to", "shall i ", "may i ", "can i ", "should i
   proceed", "should i go ahead", "is it ok if i", "is it okay if i", "is it okay to", "is it
   ok to"]` or contains one of `pub const PERMISSION_ASK_MARKERS: &[&str] = &[" proceed", "go
   ahead"]`. Effect, same mechanics as the two siblings: `next_skill` removed from the raw
   patch, action set to `Continue { note }` with `pub const PERMISSION_ASK_NOTE: &str =
   "`ask_user` asked permission for what the task already asks of you; nothing was asked and
   your patch was kept. You do not need permission: make the call you proposed. Ask the user
   only for a decision you cannot make yourself."`.
2. A question that is not a permission ask is untouched: "Which of the two configs is the
   production one?" or "Should the default be 3 or 5?" stay `AskUser` (neither matches a
   prefix or marker).
3. The second permission ask in a row is item 47's business (`refusal_thinks` sees a harness
   `Continue`); add no think trigger.

## Acceptance (exact strings; never loosen an existing assertion)
- `skills/src/coding/tests.rs` unit tests: (a) each of the three live messages above converts,
  exact note, `next_skill` stripped, patch otherwise kept; (b) "Which of the two configs is the
  production one?" and "Should the default be 3 or 5?" untouched; (c) a message with no `?`
  still gets `ask_user_statement_note`'s note, not this one; (d) the prefix match is
  case-insensitive and tolerates leading whitespace ("  Would You Like Me To add it?").
- `skills/tests/it/` end-to-end (use the fixture the existing ask_user tests use, e.g. in
  `early_park.rs` or `premature_done.rs`): implement, one tool error, then `ask_user` "Would you
  like me to proceed with this change now?": the step commits as `continued` with the exact
  note and the run does not park; a following `edit_file` runs.
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs` (export the three
constants), `skills/tests/it/*.rs`. No core, tools, cli, bench/ or .md edits.
Report: files changed, exact constants, every existing test whose expectations changed and why,
commands run with exit codes.
