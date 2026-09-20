# Unit 27: a run without a green test in N test runs stops

## Spec
1. `core/src/version.rs`: `HarnessState.fails_since_green: u32` (`#[serde(default)]`),
   the count of `test` tool runs whose result was a failure since the last passing
   `test` run in this activation. `core/src/session.rs`: increment where the test
   result is noted (`note_test_result` / the `tests` record write), reset to 0 on a
   pass and on activation. Update the `HarnessState` literals the compiler flags
   (`store/src/test_support.rs`, `models/tests/it/common/mod.rs`,
   `models/tests/it/mock_model.rs`, `core/tests/it/*.rs`, `skills/src/coding/tests.rs`).
2. Constants in core: `pub const NO_GREEN_THINK_AT: u32 = 12;` and
   `pub const NO_GREEN_PARK_AT: u32 = 16;`. At a failing `test` that brings the count
   to `NO_GREEN_PARK_AT`, the core parks through `park_non_progress_cap` with the
   shape `"no green in 16 test runs"` (format from the constant).
3. `skills/src/coding/mod.rs`: `implement_no_green_thinks`, run after the stall
   trigger: when `cur.harness.fails_since_green >= NO_GREEN_THINK_AT` and no `think`
   has run since the step of the last failing test (use the `tests` record's `step`
   and `last_think_step`), and the action is not itself `test` or `think`,
   substitute `think` with `pub const NO_GREEN_QUESTION: &str` =
   "{n} test runs have failed since the last pass. Is the plan itself wrong? Name the one smallest change that could make the failing test pass, or say which plan step should be dropped."
   with `{n}` filled. Re-export from `skills/src/lib.rs`.

## Acceptance
- `core/tests/it/no_green_cap.rs` (new module in `tests/it/main.rs`): sixteen failing
  `test` runs park at the sixteenth with `non_progress_cap` logged and the shape
  string; a pass at the fifth resets the count (assert `fails_since_green` goes
  4, 0, 1). Copy the session setup from `core/tests/it/non_progress.rs`.
- `skills/tests/it/no_green_think.rs` (new module): twelve failing tests then a
  `read_file` becomes `think` with the exact filled question; a `test` action at
  the same point is not substituted; after the think, the next non-test action runs.
- Existing green: `core/tests/it` `non_progress::`, `repeated_failure::`,
  `try_fixes_source::`; `skills/tests/it` `think_trigger::`, `stall_think::`,
  `error_think::`, `tests_field::`.

## Boundaries
`core/src/session.rs`, `core/src/version.rs`, `core/src/lib.rs`, `skills/src/coding/mod.rs`,
`skills/src/coding/tests.rs`, `skills/src/lib.rs`, the fixture files named above, the new tests.
Integration tests are modules under `tests/it/` (one binary per crate).
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-core --test it <module>::`, `cargo test -p statie-skills --test it <module>::`,
`cargo test -p statie-models --test it`.
