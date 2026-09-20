# Unit 22: `try_fixes` reads the harness-owned failure; identical tool errors are capped

## Part (a)
`core/src/session.rs`, `execute_try_fixes`: the "no failing test on record" check
currently reads `self.session.last_test_failure` (session-local, reset in the
activation path) together with `cur.harness.last_test_cmd`. Change it to read
`cur.harness.last_failure_summary` (the persisted mirror, `core/src/version.rs`)
together with `cur.harness.last_test_cmd`. The fixer input's `failure` text is the
summary from the harness state. Keep the error path and its message for the case
where the harness state has no failure. Do not remove `last_test_failure`; other
logic (the repeat counter) still uses it. If the harness mirror is not written in
some path where the session field is (check every assignment to
`last_test_failure` against every assignment to `harness.last_failure_summary`),
make the mirror complete so the two never disagree, and say so in the report.

## Part (b)
Same file, the non-progress guard (`last_skip_key` / skip repeat count /
`non_progress_count` / `non_progress_shapes`, `CONTINUE_REPEAT_CAP`): add the
same treatment for consecutive identical `tool_error` steps. Identity is
(tool name, reason string). Keep a `last_tool_error_key: Option<(String, String)>`
and a count with the same reset rule as `last_skip_key` (any step that is not an
identical tool error breaks the streak). At the second identical error, the
observation gets a prefix line "[harness] This is the same `{tool}` error as the
previous step; repeating the call will not change it." At the cap (same constant
as the skip cap), the step parks through the existing non-progress park path with
a shape string like `"repeated tool error: {tool}"` added to `non_progress_shapes`.

## Acceptance
- `core/tests/try_fixes_source.rs` (new): a session with a `MockFixer` and a
  `MockCandidateRunner` (see `core/tests/` for the existing try_fixes tests and
  copy their setup): run a failing `test`, then force a re-activation of the same
  skill (whatever the existing tests do to trigger `activate`; if the only way is
  a skill handoff, hand off to a skill and back), then call `try_fixes {}`: the
  fixer IS invoked (assert its recorded input's `failure` equals the harness
  `last_failure_summary`), no `tool_error`. Second test: with no test ever run,
  `try_fixes {}` still yields the "no failing test to fix" tool error.
- `core/tests/non_progress.rs` gets two tests: three consecutive identical tool
  errors (use a tool whose args are rejected, e.g. `read_file` with a missing
  path, or a mock tool that always errors) park at the third with
  `non_progress_cap` logged and the shape naming the tool; two identical errors
  separated by a successful call do not park.
- Existing tests green: `core/tests/non_progress.rs`, `core/tests/read_paging.rs`,
  `core/tests/evicted_reread.rs`, every existing try_fixes test in `core/tests/`
  and `skills/tests/` (`cargo test -p statie-skills --test try_fixes_gate --test try_fixes_trigger`).

## Boundaries
`core/src/session.rs`, `core/tests/try_fixes_source.rs` (new), `core/tests/non_progress.rs`.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test <name>`, `cargo test -p statie-skills --test <name>`.
