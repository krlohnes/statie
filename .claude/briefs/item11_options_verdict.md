# Unit 11: options-and-verdict mode for `think`

## Spec
`think` gains an optional `mode` argument: `"trace"` (today's behaviour, default) or
`"options"`. In options mode the harness wraps the question so the thinker must
(1) list two or three approaches, each with one line of trade-off, (2) pick one,
(3) state the verdict in one sentence. The instruction text is a `pub const
OPTIONS_MODE_INSTRUCTION` in `core/src/thinker.rs` (or wherever
`CARRY_OVER_INSTRUCTION` lives) and is prepended to the question sent to the
thinker. The `decision` field written by `common_after_tool` stores the full
answer as today; nothing else in the workspace changes. The plan skill's rules
text tells the model: "For a design fork with more than one reasonable approach,
call `think` with `mode: "options"` before writing the plan." Only the plan skill
gets that sentence; implement and review do not.

Also add a bench task `bench/tasks/design_fork/` shaped like the existing Rust
tasks (look at `bench/tasks/add_struct_field/` for the layout: `task.json`,
`sandbox/`, the expected-tests convention). The task: a `RateLimiter` with
`new(capacity: u32, refill_per_sec: u32)` and `allow(&mut self, now_ms: u64) -> bool`;
the sandbox ships behavioural tests (burst up to capacity, refill over time, no
negative drift) that a token bucket or a sliding window both satisfy. Register it
in `EXPECTED_TASKS` in `cli/tests/bench_tasks.rs`.

## Acceptance
- `core/tests/think_modes.rs` (new): with a `MockThinker`, `think {question, mode:"options"}`
  sends `OPTIONS_MODE_INSTRUCTION + question` and `think {question}` sends the bare
  question; an unknown mode is a tool error whose text names the two modes.
- `skills/tests/plan_options.rs` (new): the plan skill's rules text contains the
  sentence above and the implement skill's does not.
- The bench task's own tests pass against a reference token-bucket solution you
  write and then delete (the shipped sandbox has the stub that fails, as the other
  tasks do). `cargo test -p statie --test bench_tasks` green.

## Boundaries
`core/src/session.rs` (think arg parsing only), `core/src/thinker.rs`, `skills/src/coding/mod.rs`
(plan rules text only), the two new test files, `bench/tasks/design_fork/`, `cli/tests/bench_tasks.rs`.
