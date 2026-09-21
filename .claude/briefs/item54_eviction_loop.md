# Unit 54: the eviction loop cuts by what the working set actually gave up, and is bounded

## Background (read, do not restate)
Item 44's loop in `core/src/session.rs` (search `working_set_shrunk`): while the assembled
prompt is over budget, `ws_budget -= overage + 64`, rebuild `WorkingSet::from_log` with the
smaller budget, reassemble, log one `working_set_shrunk` entry per iteration. The working set is
made of whole windows, so a budget cut of `overage + 64` often drops nothing and the overage is
unchanged; the loop then iterates dozens of times per step. Build 34 Qwen long,
`refactor_signature` seed 1: 714 `working_set_shrunk` entries over 166 steps, 36 iterations on
one step (overage 44 each time, budget walked from 6000 to 2112 in steps of 108), 46 on another.
Each iteration is a full working-set rebuild and a log entry; the run took 1,093 s.

## Spec
1. Each iteration cuts the budget to what the working set can actually give up: after the
   rebuild, measure the working set's rendered token count (`tokenizer.count` of its render, or
   whatever `WorkingSet` exposes; find it). If the new working set is not smaller than the
   previous one by at least the overage, the next cut is at least half of the current budget
   (`new_budget = min(ws_budget - (overage + 64), ws_budget / 2)` when the previous cut freed
   less than the overage; the plain `overage + 64` cut when it freed enough). Bound the loop at
   `pub const WORKING_SET_EVICTION_ROUNDS: usize = 8`; after that, fall through to the existing
   observation clamp and the final `result?` as today.
2. One log entry per step, not per iteration: replace the per-iteration `working_set_shrunk`
   entries with a single `working_set_shrunk` entry after the loop carrying `from` (the
   configured budget), `to` (the final budget), `rounds` (iterations run), and `overage` (the
   first overage). Keep the event name. If no shrink happened, no entry (as today).
3. The `observation_shrunk` clamp is unchanged.

## Acceptance (exact values; never loosen an existing assertion except where the entry shape
## changed, and say so)
- `core/tests/it/` (find item 44's eviction tests, likely in the prompt/budget module): (a) a
  working set of whole windows whose first `overage + 64` cut frees nothing: the loop halves,
  and the test asserts the exact sequence of budgets and that `rounds` in the single entry is
  exact; (b) the bound: a case that cannot fit in 8 rounds ends with exactly 8 rounds, one
  entry, and then the observation clamp or the error as today (assert which); (c) item 44's
  existing tests updated to the single-entry shape with exact `from`/`to`/`rounds`/`overage`.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-core --lib` (if
  any), `cargo clippy -p statie-core --all-targets -- -D warnings`, `cargo fmt -p statie-core`,
  `cargo test -p statie-skills --test it` (unchanged), `cargo test -p statie --bin statie`.

## Boundaries
`core/src/session.rs`, `core/src/working_set.rs` (or wherever `WorkingSet` lives, only if a
token-count accessor is needed), `core/tests/it/*.rs`. No skills, tools, cli, bench/ or .md
edits.
Report: files changed, the constant, every existing test whose expectations changed and why,
commands run with exit codes.
