# Unit 50: a redundant read of the open step's file finishes the step

## Background (read, do not restate)
Build 32 gemma `trap_rounding` seed 2 parked at the mixed-shape non-progress cap after six
alternations: `read_file src/tax.rs` skipped as redundant (the file is already in the working
set; `core/src/session.rs` 7.8 guard, `harness.last_skip = Some(SkippedRead{..})`), then
`done` on the open plan step "examine `src/tax.rs` to understand the rounding logic" refused
by `premature_done_note` (`skills/src/coding/mod.rs`, item 37: "plan step 2 is still open ...
A step is finished by a tool call"). The only tool call the step names is the one the harness
refuses; marking the step done is refused too. Each shape resets the other's streak, so
neither second-strike `think` fires.

## Spec
1. In `premature_done_note` (case (a), `done` with an open step), BEFORE it decides to refuse:
   if `cur.harness.last_skip` is `Some(skip)` whose `tool` is `read_file` (check the
   `SkippedRead` type in `core/src/version.rs` for its fields) and whose path appears verbatim
   in the FIRST open step's `text` (the same open step the note would name), then instead of
   refusing: rewrite the raw patch so that step's `status` is `done` (merge with the model's
   own entry for that id if it sent one; keep everything else), and return the note
   `pub const STEP_READ_ALREADY_NOTE_PREFIX: &str = "step {id} only asked to read {path},
   which is already in your working set; the harness marked it done: "` with `{id}` and
   `{path}` filled, followed by the step text. The action stays `done` and continues through
   the existing chain (`green_done_note` / `implement_bare_done` /
   `implement_no_green_runs_test` see the rewritten patch), so the `done` is handled by
   whichever rule applies to the plan with that step closed. If a later step is also open,
   the normal rules still apply to it on the next `done`.
2. Nothing else changes: no new think trigger, no change to the 7.8 guard, no change to
   `drop_done_plan_entries` for the other steps.

## Acceptance (exact strings and values; never loosen an existing assertion)
- `skills/src/coding/tests.rs`: (a) hand-built `CommittedState` with plan step 2 open, text
  "examine `src/tax.rs` to understand the rounding logic", `last_skip` a `read_file` of
  `src/tax.rs`, output `done` with an empty patch: the note is exact, the merged plan reads
  step 2 `done`, the action is still `Done`; (b) the same with `last_skip` for a different
  path: the ordinary `OPEN_STEP_NOTE_FIRST` refusal, unchanged; (c) the same with
  `last_skip == None`: unchanged refusal; (d) the path appears in step 3's text but step 2 is
  the first open step: unchanged refusal.
- `skills/tests/it/premature_done.rs` end-to-end: a scripted implement run where the model
  reads `src/tax.rs` twice (second skipped as redundant; use the existing redundant-read
  fixture in `skills/tests/it/`, find it) then sends `done` with step "examine `src/tax.rs`"
  open and no green run: the step is marked done, the note is exact, and the `done` is then
  handled by `implement_no_green_runs_test` (assert the action that commits and
  `consecutive_continues`).
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs` (export the
constant), `skills/tests/it/premature_done.rs` and any test the compiler or a failing assertion
names. No core, tools, cli, bench/ or .md edits.
Report: files changed, the exact constant, every existing test whose expectations changed and
why, commands run with exit codes.
