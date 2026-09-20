# Unit 13: `statie audit` paired-run comparison

## Spec
A new CLI subcommand `statie audit <run_a_dir> <run_b_dir>` in `cli/src/audit.rs`,
wired in `cli/src/main.rs` beside `bench`. Each run dir is a bench output
directory `bench-runs/<label>/<task>/seed<N>/` containing `session.jsonl`
(the per-step record written by `cli/src/bench.rs`, see line ~615) and `stdout.txt`.

Output (plain text to stdout):
1. One header line per run: task, seed, outcome, steps (from the record).
2. The first step where the two runs' *actions* diverge (tool name + primary
   argument), with the action each took.
3. The first step where the two runs' *harness state* diverges: compare
   `plan`, `files`, `decision`, `tests`, and the working-set window list, field
   by field, and print the field name and both values (truncated to 200 chars).
4. Then a per-step table for both runs up to 10 steps past the first divergence:
   `step | action_a | action_b | prompt_tokens_a | prompt_tokens_b`.
If a field or record is missing the line says `n/a`, never panics.

Read `session.jsonl`'s schema from the writer in `bench.rs`; do not invent
fields. If the record does not carry enough to compute a section, print
`section unavailable: <reason>` and keep going.

## Acceptance
- `cli/tests/audit.rs`: two hand-written fixture records (small, inline
  strings) with a known divergence at step 3; assert the exact header lines,
  the divergence line, and the table row for step 3. Missing-file case prints
  `n/a` and exits 0.
- `cargo build -p statie` clean.

## Boundaries
`cli/src/audit.rs` (new), `cli/src/main.rs` (dispatch only), `cli/tests/audit.rs`
(new). No changes to `bench.rs` or the record format.
