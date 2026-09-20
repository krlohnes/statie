# Unit 49: cap the implement-review round trip

## Background (read, do not restate)
Build 32 gemma `rename_across_files` seed 2: 188 steps, 148 activations. `review` handed back
findings (a duplicated doc-comment line) 75 times; `implement` handed straight back to
`review` every time with an empty `files` patch and no tool call. Why it is frictionless:
re-activation keeps `harness.green_step` and zeroes `harness.tool_calls`
(`core/src/session.rs`, `activate`'s `Some((cur, _))` branch), so `green_done_note`
(`skills/src/coding/mod.rs`) accepts implement's immediate `done` and injects `next_skill:
review`. `implement`'s spec never mentions `findings`. Nothing counts handoffs.

## Spec
1. `implement`: a new validate step `implement_findings_open_note(cur, out) -> Option<String>`
   that runs BEFORE `green_done_note` in `ImplementSkill::validate`'s done chain. Fires when
   the action is `done` (any `next_skill`, or none), `cur.agent.findings` is non-empty, and
   `cur.harness.tool_calls == 0` (no tool call since this activation). Effect: the action
   becomes `Action::Continue { note }` with `pub const FINDINGS_OPEN_NOTE_PREFIX: &str =
   "review sent this work back and you have not touched it since: "` followed by the
   findings joined with "; ", then " Fix them with `edit_file` and `test`, or `ask_user` why
   they are wrong. Your patch was kept." The patch is left as sent. This is a harness
   `Continue`, so item 47's `refusal_thinks` covers the second strike and the caps the third;
   do not add another think trigger. A `done` after at least one tool call in this activation
   is NOT touched by this function (falls through to the existing chain).
2. Round-trip cap. `core/src/guards.rs` `Guards` gains `pub review_rounds_cap: u32` (serde
   default 3, doc comment with the evidence above); it reaches `ReviewSkill` the way
   `grounded_findings` does (find `CodingRegistry::with_guards`, `ReviewSkill`'s fields and
   builder). `CodingWorkspace` gains `pub review_rounds: u32` (serde default 0), harness-owned
   (add it to `common_harness_owned_fields` so the model cannot patch it; the model's spec
   text for the workspace should say it is kept by the harness). `ReviewSkill::on_activate`
   returns a patch setting `review_rounds` to `ws.review_rounds + 1` (merged with the existing
   `common_on_activate` `next_skill: null` patch when both apply). `ReviewSkill::validate`:
   when the step would be a send-back (after `review_sendback_note` and the existing chain
   have had their say: action `done` with merged `next_skill == "implement"`, or merged
   `findings` non-empty and `done == false`) and `cur.agent.review_rounds > cap`, the action
   becomes `Action::AskUser { message }` (a park) with `pub const REVIEW_ROUNDS_CAP_QUESTION:
   &str = "review has sent this work back {n} times and implement did not resolve it. The
   open findings: {findings}. Decide: accept the work as is, or say which finding to fix
   first."` filled in (`{n}` = `review_rounds - 1`, the number of send-backs so far;
   `{findings}` joined with "; "), and the note `"review rounds cap ({cap}) reached: parked
   with the findings"`. `review_rounds` counts activations of `review`; the first activation
   is round 1, so cap 3 allows three send-backs and parks on the fourth review's send-back.
3. `statie.example.json`: document `guards.review_rounds_cap` (one line, default 3) if the
   guards section is there (check first; if `guards` is not documented there, report it).

## Acceptance (tests assert exact strings and values; never loosen an existing assertion)
- `skills/src/coding/tests.rs` unit tests: (a) `implement_findings_open_note` fires on `done`
  with findings and `tool_calls == 0`, exact note, action `Continue`, patch untouched; (b) does
  not fire with `tool_calls == 1`; (c) does not fire with empty findings; (d) the review cap:
  hand-built `CommittedState` with `review_rounds == 4`, cap 3, a send-back shaped output
  becomes `AskUser` with the exact question; with `review_rounds == 3` it stays a send-back.
- `skills/tests/it/` end-to-end (find the existing implement/review fixture, e.g. the one
  `review_sendback_note` tests use): a scripted run where review sends back findings and
  implement answers `done` at once: step N is a `continued` with the exact
  `FINDINGS_OPEN_NOTE_PREFIX` note; then an `edit_file` and a `done` in the same activation
  hand off normally. And: four review activations each sending back → the fourth parks
  `AskUser` with the exact question; `review_rounds` reads 4.
- `core` guard tests (find where `Guards` is parsed/defaulted in tests): `review_rounds_cap`
  defaults to 3 and parses.
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie-core --test it`, `cargo test -p statie --bin statie`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs` (exports),
`skills/tests/it/*.rs`, `core/src/guards.rs`, `statie.example.json`, and any file the compiler
names for the `Guards` field (the cli config parses it: `cli/src/config.rs` tests may need the
new field's default). No `core/src/session.rs` changes. No bench/ or .md edits.
Report: files changed, exact constants, every existing test whose expectations changed and why,
commands run with exit codes.
