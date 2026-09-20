# Unit 23: the detected test command is recorded; a repeated failing call becomes a `think`

## Part (a): record the detected command
`core/src/session.rs` near `harness.last_test_cmd = extract_command(&args);`
(the `test` branch of `common` bookkeeping): when `extract_command` yields
`None` (no `cmd` argument), parse the command from the tool output's first
line, which `tools/src/test_tool.rs` always prints as `command: <cmd>`, and
store that. Put the parsing in a small `pub fn detected_test_command(output:
&str) -> Option<String>` beside `extract_command`. The `tests` record written by
the coding skill (`skills/src/coding/mod.rs`, look for where `TestRecord { cmd,
.. }` is built in `common_after_tool`) uses the same function so its `cmd` is
never empty after a run that printed a command. `execute_try_fixes`'s
precondition is unchanged; it now finds both values populated.

## Part (b): a repeated failing call becomes a `think`
1. `core/src/version.rs`: `HarnessState.last_tool_error: Option<RepeatedToolError
   { tool: String, args: serde_json::Value, reason: String, count: u32, step: u64 }>`
   (`#[serde(default)]`, beside `last_skip`). `core/src/session.rs` writes it
   where the identical-tool-error guard (item 22 part b: `last_tool_error_key`,
   `tool_error_repeat_count`) updates, with the same value of `count`, and
   clears it where that guard resets (any step that is not a tool error, and
   activation). Update the `HarnessState` literals the compiler flags
   (`store/src/test_support.rs`, `models/tests/common/mod.rs`, tests).
2. `skills/src/coding/mod.rs`: `pub const REPEATED_ERROR_QUESTION: &str` =
   "Your last `{tool}` call failed with: {reason}. Repeating the same call will fail the same way. Using the working set above, state a different exact next action: for an edit, the path, the old text as it appears in the file, and the new text; otherwise the file or range to read and why."
   and `implement_repeated_error_thinks`, run right after `implement_repeated_skip_thinks`:
   it fires when the action is `Action::Tool { name, args }` with `name` and
   `args` equal to `cur.harness.last_tool_error`'s (`count >= 1`) and no `think`
   has run since that error's step (same `last_think_step` rule as the skip
   trigger). It substitutes `think` via `substitute_think` with `{tool}` and
   `{reason}` filled (reason cut to 300 characters). Re-export the constant from
   `skills/src/lib.rs`.
3. The core cap (third identical error parks) is unchanged.

## Acceptance
- `core/tests/detected_cmd.rs` (new): a `test {}` whose tool output starts with
  `command: make test` and fails leaves `harness.last_test_cmd == Some("make test")`;
  then `try_fixes {}` with a `MockFixer` invokes the fixer (no "no failing test"
  error). Use the try_fixes test setup from `core/tests/try_fixes_source.rs`.
- `skills/tests/tests_field.rs` (existing) gets one test: after `test {}` with a
  detected command the `tests` record's `cmd` is that command.
- `skills/tests/error_think.rs` (new): an `edit_file` that errors (old text not
  found), then the same call again: the action at that step is `think` with
  exactly the filled `REPEATED_ERROR_QUESTION`, the thinker was called once,
  `decision` carries the answer, no park. Then a different call after an error
  does not fire. Then the same call three times with a `think` already run
  after the first error: no second `think` (the `last_think_step` rule), the
  core cap parks at the third.
- Existing green: `core/tests/non_progress.rs`, `core/tests/try_fixes_source.rs`,
  `skills/tests/skip_think.rs`, `skills/tests/think_trigger.rs`, `skills/tests/tests_field.rs`.

## Boundaries
`core/src/session.rs`, `core/src/version.rs`, `core/src/lib.rs`, `skills/src/coding/mod.rs`,
`skills/src/coding/tests.rs`, `skills/src/lib.rs`, `store/src/test_support.rs`,
`models/tests/common/mod.rs`, `models/tests/mock_model.rs`, the named tests.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-core --test <name>`, `cargo test -p statie-skills --test <name>`,
`cargo test -p statie-models --test mock_model`.
