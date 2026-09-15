# Unit 39: calls that name no tool are one streak, whatever the name

## Background (read, do not restate)
`core/src/session.rs` keys the repeated-tool-error streak (`last_tool_error_key`,
`tool_error_repeat_count`, `CONTINUE_REPEAT_CAP = 3`) on `(tool name, error_shape(reason))`, for
both `tool_error` and `tool_rejected` entries. At count 2 the observation gets the "same error as
the previous step" prefix and `implement_repeated_error_thinks` may substitute a `think`; at
count 3 `park_non_progress_cap` parks with the shape `repeated tool error: {tool}`. Bench
`qwen_json_short_b23/rename_across_files/seed1`: the model sent 107 rejected calls in a row,
alternating `name: ""` ("action.name is empty: ...") and `name: "src/config.rs"` ("tool
`src/config.rs` is not available in skill `implement`; ..."). The two keys alternated, the count
never reached 3, and the run timed out at 126 steps. The same shape (a path or nothing in
`name`) appears in `qwen_json_long_b22` (`tests/load.rs`, `tests/cli_and_diagnostics.rs`).

## Spec
1. In `core/src/session.rs`, a `tool_rejected` entry whose reason says the call named no usable
   tool is keyed as the class `("<not a tool>", "not a tool")` instead of its own name and shape.
   "Named no usable tool" means the rejection reason is one of the two produced today for
   (a) an empty `name` ("action.name is empty: ...") and (b) a `name` that is not an active tool
   of the current skill ("tool `X` is not available in skill `S`..." in either of its two forms:
   with "; `T` has it. Finish this skill's job, then hand off" and without). Find where those
   reasons are produced and export a `pub fn is_not_a_tool_rejection(reason: &str) -> bool` in
   core beside `error_shape` that recognises them (match on the fixed prefix/phrase, not the
   whole text). Every other `tool_rejected` and every `tool_error` keeps today's key.
2. The `RepeatedToolError` written to `HarnessState.last_tool_error` for such a streak keeps
   `tool` = the LATEST rejected name (possibly empty) and `reason` = the latest full reason, as
   today (the `think` question quotes them); `shape` = "not a tool"; `count` = the class streak
   count. Document on the struct that for this class the streak spans differing names.
3. The park shape string pushed to `non_progress_shapes` for this class is exactly
   `repeated tool error: <not a tool>`. The observation prefix at count 2 (the second such call
   in a row) reads, for this class, exactly:
   "[harness] This is the second call in a row that named no tool (`{latest name}` is not one);
   a third parks the session. Put one of the active tools in `name`."
   with `{latest name}` the latest `name` verbatim (an empty name renders as two backticks with
   nothing between).
4. Nothing else changes: the identical-call streak for real tool errors, the `think`
   substitution trigger, `CONTINUE_REPEAT_CAP`, and `infer_tool_name` are untouched.

## Acceptance
- `core/tests/it/` (the module that tests the repeated tool error streak and its park; find it
  by `repeated tool error:`): (a) a scripted session in `implement` whose model sends tool calls
  named `""`, `"src/config.rs"`, `""` in that order (each rejected) is parked on the third
  with `non_progress_shapes` containing exactly `repeated tool error: <not a tool>`, and the
  second step's observation starts with the exact prefix from spec 3 with `src/config.rs` as the
  latest name. (b) the same with `"src/config.rs"`, `"edit_file"` (in a skill that lacks it),
  `""` parks on the third. (c) `""`, then a real `read_file` call, then `""`: no park, count
  restarts (assert `last_tool_error.count == 1` after the third). (d) two different REAL tool
  errors (e.g. `read_file` on two missing paths with different shapes) still do not share a
  streak (existing behaviour; assert count 1 after the second).
- `core/tests/it/` unit test for `is_not_a_tool_rejection`: true for the three reason forms
  (exact strings copied from the producers), false for a `read_file` "no such file" reason and
  for an `edit_file` anchor-miss reason.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`.

## Boundaries
`core/src/session.rs`, `core/src/version.rs` (doc only), the core test modules named (and any
test the compiler or a failing assertion names, updated to exact new text). No skills source
changes unless the rejection reasons are produced there and need a shared constant, in which
case the constant is added where the text lives and imported; report it. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie --bin statie`.
