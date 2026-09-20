# Unit 48: the maintainer's overlap is a per-model config knob

## Background (read, do not restate)
Item 43 (`core/src/session.rs`, `spawn_maintainer`/`drain_pending_maintainer`) runs the
maintainer off the critical path, notes one step stale. Qwen gained a third of its wall clock
at unchanged landing (builds 27, 29, 30); gemma lost two to four runs on every such build (16,
15 → 12, 13), all refusal walks where guidance one step late lets the walk reach the cap.
DECISIONS.md D24: per-model behaviour is a config choice, not a code default.

## Spec
1. `cli/src/config.rs` `MaintainerConfig` gains `pub overlap: bool` (serde default `true`, doc
   comment with the evidence above), threaded to the session as `Session::with_maintainer_
   overlap(bool)` (or a field on the existing maintainer wiring; find where `with_maintainer`
   is called).
2. In `core/src/session.rs`: when `overlap` is false, the session drains the maintainer
   immediately after spawning it (same spawn, same drain, same log entries, same `notes_step`),
   so the next prompt sees this step's notes: the synchronous behaviour of builds through 26,
   through the same code path. No second implementation.
3. `bench/configs/arm_harness_max.json` and `bench/configs/arm_harness_max_nomaint.json` are
   NOT edited by you (bench/ is out of bounds); report that the gemma configs need
   `"maintainer": {..., "overlap": false}` and the arbiter will set them.
4. `statie.example.json` documents the knob (one line, with the default).

## Acceptance
- `core/tests/it/maintainer.rs`: with overlap off, a three-step scripted session's prompt at
  step 2 carries step 1's notes (exact text) and `notes_step` is 1 after step 1 commits; with
  overlap on (the existing five-step test) unchanged.
- `cli` config tests (find where `MaintainerConfig` is parsed in tests): `overlap` defaults to
  true and parses `false` (exact).
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie --bin statie`,
  `cargo test -p statie-skills --test it`; `cargo clippy -p statie-core --all-targets -- -D warnings`.

## Boundaries
`core/src/session.rs`, `cli/src/config.rs`, `cli/src/*.rs` where the session is built,
`statie.example.json`, the test modules named and any test the compiler or a failing
assertion names. No skills or tools changes. No bench/ or .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie`, `cargo test -p statie-core --test it`,
`cargo test -p statie --bin statie`, `cargo test -p statie-skills --test it`,
`cargo clippy -p statie-core --all-targets -- -D warnings`.
