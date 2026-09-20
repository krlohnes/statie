# Unit 44: the prompt fits by evicting from the working set before it fails

## Background (read, do not restate)
`core/src/session.rs` renders the working set to `cur.harness.budgets.working_set` tokens
(`WorkingSet::from_log(...)`), then `PromptBuilder::build()` (`core/src/prompt.rs`) fails with
`OverBudget` when the whole prompt exceeds `budgets.total`, naming the largest section. The
working set is the one elastic section, but it is sized before the others are known, so when
the fixed sections grow (item 40 added about 800 tokens of tool descriptions to the system
block; long tasks carry large `plan`/`files` state) the total overflows and the step FAILS:
`qwen_json_long_b26`, five of ten runs ended with "step failed: prompt section System over
budget: 14005..14986 > 14000" and the session parked on an error. A harness that owns the
working set must never fail a step on a budget it can meet by evicting.

## Spec
1. In `core/src/session.rs`, where the prompt is built: if `build()` returns `OverBudget` and
   the working set was rendered with a non-zero budget, re-render the working set with its
   budget reduced by the overage plus a margin of 64 tokens (`budget = budget.saturating_sub
   (tokens - limit + 64)`), rebuild, and repeat while it still overflows and the working set
   budget is above zero. A working set at budget zero renders no windows (the recent-calls
   lines may stay if they fit; follow `WorkingSet::from_log`'s existing behaviour at a tiny
   budget). Each shrink is logged once per step as a system entry
   `{"event": "working_set_shrunk", "from": <tokens>, "to": <tokens>, "overage": <tokens>}`.
2. If it still overflows with the working set at zero, truncate the observation section to
   fit (the observation already has `truncate_with_report`-style handling; reuse it) with the
   same 64-token margin, logging `{"event": "observation_shrunk", ...}` the same way.
3. If it still overflows after both, return the error as today (it now means the fixed
   sections alone exceed the total, which is a configuration error worth failing on).
4. `last_window_steps` reflects the working set actually rendered into the prompt.
5. No change to `PromptBuilder`'s own checks, budgets, or the per-section caps.

## Acceptance
- `core/tests/it/prompt.rs` or the session module that drives prompts (find the test named in
  `core/src/prompt.rs` line ~767, "prompt section Observation over budget", for the fixture
  style): (a) a scripted session whose fixed sections plus a full working set exceed `total`
  by N tokens builds a prompt (no error) whose working set section is smaller, with exactly
  one `working_set_shrunk` entry whose `overage` equals the measured overage (assert the exact
  numbers with the test tokenizer), and the prompt's token count is at or under `total`;
  (b) fixed sections that exceed `total` on their own still fail with the exact `OverBudget`
  error text naming the largest section; (c) the observation path: a working set already at
  zero and an observation that overflows by N is truncated with one `observation_shrunk`
  entry and the prompt fits (exact counts).
- Every existing test asserting an `OverBudget` on a prompt that a working-set eviction
  would now fit is updated to the new exact behaviour; none is loosened or deleted. Report
  each.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`.

## Boundaries
`core/src/session.rs`, `core/src/working_set.rs` (only if a helper is needed to re-render at a
budget), the core test modules named and any test the compiler or a failing assertion names.
No prompt.rs, skills, or tools changes. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo test -p statie-core --test it`,
`cargo test -p statie-skills --test it`, `cargo test -p statie --bin statie`.
