# Unit 19: a read of an evicted window runs again

## Spec
In `core/src/session.rs`, the redundant-read guard (search for the string
"that window is in your working set above"; it keys on `reads_since_mutation`
and `read_call_key`) currently refuses every repeat of a read-only call since
the last mutation. Change it so the refusal happens only when the earlier
call's output is still rendered in the working set:

1. `core/src/working_set.rs`: `WorkingSet::from_log` must also report which
   steps' windows it rendered. Add `pub fn window_steps(&self) -> &[u64]` (or a
   `BTreeSet<u64>`) on `WorkingSet`, filled from the same selection that
   renders the windows (the newest `max_windows` distinct windows, with edit
   windows replacing earlier windows on the same path). Keep `WorkingSet`
   sealed as it is; only `from_log` fills it.
2. The session builds the working set when it renders the prompt each step.
   Keep the rendered set's `window_steps` on the session (e.g.
   `self.session.last_window_steps`) so the guard can consult it.
3. In the guard: if windows are on (`max_windows > 0`) and `at_step` is NOT in
   the last rendered window steps, do not skip: let the read run normally, and
   push a `System` log entry `{"event":"reread_evicted","name":<tool>,"at_step":<at_step>}`
   at the current step. The paged-read branch (`previous_read_file_header`,
   `PagedRead`) is unchanged: it fires only when the earlier window IS still
   rendered. When windows are off (`max_windows == 0`) behaviour is unchanged
   ("that output is in the log").
4. `reads_since_mutation` bookkeeping is unchanged; the re-run read updates it
   as any read does.

## Acceptance
- `core/tests/evicted_reread.rs` (new), using the existing session test
  helpers (look at `core/tests/repeated_failure.rs` and `core/tests/green_filtered.rs`
  for setup with `MockModel`, file tools, and a working-set config): with
  `max_windows: 2`, read `a.txt`, then read `b.txt` and `c.txt` (two newer
  distinct windows evict `a.txt`), then read `a.txt` again: the read RUNS
  (a `tool_output` entry for that step with the file content), the log has
  one `reread_evicted` entry naming `at_step` of the first read, and no
  `skipped_redundant` entry. Second test: with `max_windows: 4` the same
  sequence is refused as today (one `skipped_redundant`, no `tool_output`,
  no `reread_evicted`). Third test: `max_windows: 0` still refuses with
  "that output is in the log".
- Existing tests that assert `skipped_redundant` keep passing unchanged;
  if one breaks because its earlier read was genuinely evicted under the
  test's window config, say so in the report rather than changing the test.

## Boundaries
`core/src/session.rs`, `core/src/working_set.rs`, `core/tests/evicted_reread.rs`.
Allowed cargo: `cargo build -p statie-core`, `cargo test -p statie-core --test evicted_reread`,
and `cargo test -p statie-core --test <name>` for any core test you believe you affected.
