# Unit 35: every step records how much of its prompt was evaluated

## Spec
1. `core/src/model.rs`: `Usage` gains `#[serde(default)] pub prompt_eval_secs: Option<f64>` and
   `#[serde(default)] pub eval_secs: Option<f64>` (keep `Copy`/derives compiling; if `Usage` is
   `Copy`, `Option<f64>` is fine). Every constructor site the compiler flags sets them (`None`
   where unknown).
2. `models/src/ollama.rs`: parse `prompt_eval_duration` and `eval_duration` (nanoseconds, u64,
   optional) from the final ndjson line and fill the two fields as seconds (`ns as f64 / 1e9`).
   `models/src/openai.rs` leaves them `None`. `models/src/mock.rs`: `with_usage` keeps its
   signature and sets `None`; add `with_usage_timing(prompt_eval_secs, eval_secs)` for tests.
3. Wherever the session logs a step's `usage` (the `patch` log entry payload's `usage` object,
   `core/src/session.rs`), the two fields appear when `Some` (serde skips `None`: use
   `#[serde(skip_serializing_if = "Option::is_none")]`).
4. `cli/src/bench.rs`: `RunResult` gains `mean_prompt_eval_secs: Option<f64>` computed from the
   session log's usage entries (mean over steps that have it); `format_run_line` appends
   ` peval=<x.xx>` (or ` peval=-` when none) after `secs=`; the task table gains a `mean peval`
   column after `mean srv_max` (mean of the per-run means that exist, `-` when none). Keep every
   existing column and width unchanged; add the new one only.
5. `bench/sweep_summary.py`: parse the optional `peval=` field and add a `mean peval` column
   (`-` when absent). `bench/opencode_events.py` and `bench/run_opencode.sh` write `peval=-`.

## Acceptance
- `models/tests/it/ollama.rs` (or the existing ollama parser test module): a fixture final
  line with `"prompt_eval_count": 6136, "prompt_eval_duration": 2400000000, "eval_count": 80,
  "eval_duration": 2000000000` yields `Usage { prompt_tokens: 6136, completion_tokens: 80,
  prompt_eval_secs: Some(2.4), eval_secs: Some(2.0) }` (assert with exact floats); a line without
  the durations yields `None`s.
- `cli` unit tests (`cargo test -p statie --bin statie`): `format_run_line` with
  `mean_prompt_eval_secs: Some(1.234)` ends with ` secs=12.3 peval=1.23`; with `None` ends with
  ` peval=-`. The table test (if one exists) updated for the new column, exact.
- `core/tests/it`: a step whose mock usage carries timing logs `prompt_eval_secs` in the patch
  entry's `usage`; a step without it has no such key (assert the JSON exactly).
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-models --test it`,
  `cargo test -p statie --bin statie`, `cargo test -p statie-skills --test it`.

## Boundaries
`core/src/model.rs`, `core/src/session.rs`, `models/src/{ollama,openai,mock}.rs`, `cli/src/bench.rs`,
`bench/sweep_summary.py`, `bench/opencode_events.py`, `bench/run_opencode.sh`, the tests named,
fixture literals the compiler flags. No other files. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-models`, `cargo build -p statie`,
`cargo test -p statie-core --test it`, `cargo test -p statie-models --test it`,
`cargo test -p statie --bin statie`, `cargo test -p statie-skills --test it`.
