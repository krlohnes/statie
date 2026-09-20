# Unit 25: repeat guards key on the shape of an error, not its text

## Spec
In `core/src/session.rs`:
1. Add `pub fn error_shape(reason: &str) -> String`: the reason with every run of
   ASCII digits replaced by `#`, every path-like token (contains `/` or ends in
   `.rs`/`.py`/`.toml`/`.txt`/`.md`) replaced by `<path>`, backtick-quoted spans
   replaced by `` `…` ``, and whitespace collapsed; cut to 120 characters. Put unit
   tests beside it: `"line 42 of src/lib.rs already has `pub fn new(...)`"` and
   `"line 53 of src/lib.rs already has `pub fn count(...)`"` map to the same
   shape; `"old text not found in src/config.rs"` and `"anchor line occurs 2
   times in src/main_logic.rs"` map to different shapes.
2. The identical-tool-error streak (`last_tool_error_key`, `tool_error_repeat_count`,
   `HarnessState.last_tool_error`, item 22 part b and item 23) keys on
   `(tool, error_shape(reason))` instead of `(tool, reason)`. The `RepeatedToolError`
   record keeps the full `reason` of the latest error (the `think` question quotes
   it) and gains `shape: String`. The skill trigger `implement_repeated_error_thinks`
   (`skills/src/coding/mod.rs`) matches on tool and shape of the *previous* error,
   not on identical args: a model that re-issues the same kind of failing edit with
   a different line number is the case this exists for. Keep the `last_think_step`
   rule.
3. One streak across a refused read and a failed edit on the same path: when a
   `skipped_redundant` read of path P is followed by a `tool_error` from an edit
   tool (`edit_file`, `write_file`, `revert_file`) on path P, or the reverse, the
   two count as one "stalled on P" streak: introduce `HarnessState.last_stall:
   Option<Stall { path: String, count: u32, step: u64 }>` written by core whenever
   a skip or an edit error lands on a path, incremented when the previous stall
   is the same path and no executed (non-error, non-skip, non-think) step has
   happened since, cleared otherwise. `implement` substitutes `think` at
   `count >= 2` with `pub const STALLED_PATH_QUESTION` =
   "You have been going back and forth on {path}: reading it again and editing it have both failed. Using the working set above, state the one exact edit (old text as it appears, new text) that moves the plan forward, or say which other file the change belongs in."
   (same `last_think_step` rule), and core parks at `count >= 6` through
   `park_non_progress_cap` with shape "stalled on {path}". A `think` step keeps
   the stall count (D19).

## Acceptance
- Unit tests for `error_shape` as above (in `core/src/session.rs` `#[cfg(test)]`).
- `core/tests/it/non_progress.rs`: three `edit_file` errors with the same shape but
  different line numbers park at the third (today they do not); prove the "today
  they do not" half by asserting the count goes 1, 2, 3 on `last_tool_error`.
- `skills/tests/it/error_think.rs`: after an `edit_file` error, a second `edit_file`
  on the same file with different arguments but the same error shape becomes
  `think` with `REPEATED_ERROR_QUESTION`.
- `skills/tests/it/stall_think.rs` (new): read P, read P (skipped), edit P (error),
  read P (skipped): the last is `think` with `STALLED_PATH_QUESTION` for P; then
  continuing to alternate parks at the sixth with `non_progress_cap` and the
  shape naming P; an executed edit on P in between resets the stall.
- Existing green: `core/tests/it` (`non_progress`, `read_paging`, `evicted_reread`,
  `try_fixes_source`, `detected_cmd`), `skills/tests/it` (`error_think`, `skip_think`,
  `think_trigger`, `try_fixes_gate`).

## Boundaries
`core/src/session.rs`, `core/src/version.rs`, `core/src/lib.rs`, `skills/src/coding/mod.rs`,
`skills/src/coding/tests.rs`, `skills/src/lib.rs`, `store/src/test_support.rs`,
`models/tests/it/common/mod.rs`, `models/tests/it/mock_model.rs`, the named tests, and any
`HarnessState` literal the compiler flags. Integration tests are modules under
`<crate>/tests/it/`; a new test file is a new `mod` line in `tests/it/main.rs`.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-core --lib session::`, `cargo test -p statie-core --test it <module>::`,
`cargo test -p statie-skills --test it <module>::`, `cargo test -p statie-models --test it`.
