# Unit 37: a plan step is marked done by work, not by a refused `done`

## Background (read, do not restate)
`skills/src/coding/mod.rs` `premature_done_note` case (a): `implement` sends `done` while the
merged plan has an open step; the patch is kept and the action becomes `Action::Continue { note }`
with the note "plan step N is still open: ... If that step's work is already done, set its status
to done in your patch and run `test`; if not, make the change with edit_file/write_file, then
`test`. `done` is for a finished plan; your patch was kept." In seven bench runs the model answered
by marking N done and sending `done` again, three to twelve times in a row with no tool call,
until the non-progress cap parked it. The note invites that walk.

## Spec
1. `core/src/version.rs` `HarnessState` gains `pub consecutive_continues: u32` (serde default,
   doc comment in the style of `consecutive_non_question_asks`): the number of consecutive
   committed steps whose final action was the harness-internal `Action::Continue`. In
   `core/src/session.rs`, next to where `consecutive_non_question_asks` is set, compute it from
   the FINAL action being committed (after `validate`): `Continue` extends the previous value by
   one, any other action resets to 0. Initialise to 0 wherever `HarnessState` is built.
2. `premature_done_note` case (a) changes as follows (case (b), the empty `ask_user`, unchanged):
   - First refusal (`cur.harness.consecutive_continues == 0`): note text becomes exactly
     "plan step {id} is still open: {text}. A step is finished by a tool call, not by marking it
     done; make the change with edit_file/write_file and run `test`, or if that step's work is
     already done, run `test` to show it. `done` is for a finished plan; your patch was kept."
     `next_skill` is stripped as today. The patch is kept in full, including plan status changes.
   - Repeat refusal (`cur.harness.consecutive_continues >= 1`, i.e. the previous committed action
     was also a harness `Continue` and no tool has run since): any `plan` entry in the raw patch
     whose `status` is `done` is dropped from the patch (other fields of that entry and all other
     patch fields are kept; if the `plan` array becomes empty, remove the `plan` key). The note
     is exactly "plan step {id} is still open: {text}. No tool has run since your last `done`,
     and plan steps are not marked done without one; the status changes in this patch were
     dropped. Your next action must be a tool call: {call}. `done` is for a finished plan."
     where `{call}` is "`test`" when `cur.harness.edits_since_test > 0` or
     `cur.harness.green_step.is_none()`, otherwise "`read_file` or `edit_file` on the file that
     step names" (exact strings). `{id}`/`{text}` are the first open step of the MERGED plan
     after the drop (recompute the merge after dropping).
   - Put the two note templates behind `pub const` strings in `skills/src/coding/mod.rs`
     (`OPEN_STEP_NOTE_FIRST`, `OPEN_STEP_NOTE_REPEAT`, with `{}` placeholders and a `format!`
     helper), so tests import them (D13).
3. No other guard changes. `implement_no_green_runs_test`'s priority over this note is unchanged.

## Acceptance
- `core/tests/it/session.rs` (or the module that tests `HarnessState` counters; a new `mod` in
  `tests/it/main.rs` if none fits): a scripted session where the committed actions are
  `Continue, Continue, tool, Continue` asserts `consecutive_continues` reads 1, 2, 0, 1 after each
  commit (exact), and a fresh session reads 0.
- `skills/tests/it/` (the module where `premature_done_note` is tested today; find it by the
  string "is still open"): (a) first refusal: the note equals `OPEN_STEP_NOTE_FIRST` formatted
  with the open step, the patch's plan status changes are applied (assert the merged plan
  statuses exactly). (b) repeat refusal with `consecutive_continues: 1` and a patch marking step 2
  done: the merged plan still has step 2 `in_progress`/`pending` (exact), the note equals
  `OPEN_STEP_NOTE_REPEAT` with `{call}` = "`test`" when `edits_since_test: 1`, and a second case
  with `edits_since_test: 0` and `green_step: Some(3)` gives the `read_file`/`edit_file` form
  (exact). (c) a repeat refusal whose patch has only a plan status change results in a patch with
  no `plan` key (assert the raw patch exactly). (d) a scripted three-step run: `done`(open step)
  → `done` marking it done → `test` shows the third step is a real tool call and
  `consecutive_continues` is 0 after it.
- Every existing test that asserts the old note text is updated to the new exact text; none is
  loosened or deleted.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie-skills --lib`, `cargo test -p statie --bin statie`.

## Boundaries
`core/src/version.rs`, `core/src/session.rs`, `skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`,
the test modules named, and any test the compiler or a failing assertion names (updated to exact
new text). No other source files. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie --bin statie`.
