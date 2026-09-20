# Unit 21: a second refused re-read becomes a `think`

## Spec
1. `core/src/session.rs`: the repeated-skip guard (`last_skip_key`,
   `last_skip_count`, the `skipped_redundant` path in `Active::execute`) also
   writes to the harness-owned state: add `HarnessState.last_skip: Option<SkippedRead { tool: String, key: String, count: u32, step: u64 }>`
   (a new small struct in `core/src/version.rs` beside `HarnessState`, serde
   like its neighbours, `#[serde(default)]`), set on every `skipped_redundant`
   step to that step's key and the consecutive count, and cleared (None) on any
   step that is not a skip, mirroring `last_skip_key`'s own reset rule. Update
   the `HarnessState` literals the compiler points at (tests, `store/src/test_support.rs`, `models/tests/common/mod.rs`).
2. `skills/src/coding/mod.rs`, `ImplementSkill::validate`: a new
   `implement_repeated_skip_thinks` runs after `implement_try_fixes_needs_failing_test`
   and before `implement_repeat_failure_thinks`. It fires when: the action is a
   read-only tool call (`read_file`, `search`, `list_files`, or a `shell` read the
   harness would convert; reuse `shell_read_as_read_file` and the read tool
   names already used in the file), `cur.harness.last_skip` is `Some` with
   `count >= 1` and the same tool and the same `read_call_key`-style key (compute
   it the same way core does: if `read_call_key` is private, make it `pub` in core
   and call it), and no `think` has run since that skip (`last_think_step` is
   None or before `last_skip.step`). It substitutes, via `substitute_think`, the
   question `pub const REPEATED_SKIP_QUESTION: &str` =
   "You asked to read {path} again, but its contents are already in your working set above and nothing has changed. Either state the exact next edit (path, old text, new text) that moves the plan forward, or name a different file or range you actually need and why."
   with `{path}` filled from the call's path argument (or the tool name when
   there is none). The note text follows the other substitutions' pattern.
   The `think` substitution runs the thinker as usual; `common_after_tool`
   writes `decision`.
3. Nothing changes for the first skip (the core's `skipped_redundant` message
   still goes to the model) or when a `think` already ran since the skip.

## Acceptance
- `skills/tests/skip_think.rs` (new), a real session with a `MockThinker` and
  file tools, working set on: read `a.txt`; read `a.txt` again (skipped,
  `skipped_redundant` logged, `harness.last_skip` is `Some(count 1)`); read
  `a.txt` a third time: the action at that step is `think` with exactly
  `REPEATED_SKIP_QUESTION` for `a.txt`, the thinker was called once, `decision`
  carries the answer, and NO park happened (the run continues). Then an edit
  clears `last_skip` (`None`). Second test: with `max_windows: 0` the same shape
  still fires (the skip happens with the "in the log" wording too). Third:
  a different read after a skip does not fire.
- `core/tests/` gets one test that `last_skip` is set on a skip and cleared on
  the next executed call (put it beside the existing skip tests in `core/tests/read_paging.rs` or a new file).
- Existing tests green: `skills/tests/think_trigger.rs`, `early_park*.rs` if present,
  `core/tests/read_paging.rs`, `core/tests/non_progress.rs`, `core/tests/evicted_reread.rs`.

## Boundaries
`core/src/session.rs`, `core/src/version.rs`, `core/src/lib.rs` (re-export), `store/src/test_support.rs`,
`models/tests/common/mod.rs`, `skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`,
`skills/src/lib.rs` (re-export the question), the new tests.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`, `cargo build -p statie`,
`cargo test -p statie-core --test <name>`, `cargo test -p statie-skills --test <name>`,
`cargo test -p statie-models --test mock_model`.
