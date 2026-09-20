# Unit 28: the router cannot ask, and no example carries a copyable message

## Spec
1. `skills/src/coding/mod.rs`: `RouterSkill` overrides `fn allows_ask_user(&self) -> bool { false }`
   (next to its `allows_yield`). Its spec text gains one sentence at the end of the
   "When to hand off" paragraph: "The router cannot ask the user anything; if the task is
   unclear, hand off to `plan`, which can."
2. `core/src/prompt.rs`: in the two `ask_user` example lines, the message
   `"Which file?"` becomes `"<your question for the user>"`. Nothing else in the examples changes.
3. Update every existing test that snapshots those example lines (`core/tests/it/prompt.rs`,
   `skills/src/coding/tests.rs`, anything else the compiler or a failing test names) to the new
   exact text. Do not loosen any assertion.

## Acceptance
- `core/tests/it/prompt.rs`: a new test that builds the harness prompt with a schema whose
  action.type enum is `["tool","done"]` and asserts the prompt contains no `ask_user` example
  line and does contain the `tool` and `done` lines (exact strings).
- `skills/tests/it/router.rs` (new module in `skills/tests/it/main.rs`, or add to an existing
  router test module if one exists): with the router active, a model output whose action is
  `ask_user` with message "Which file?" is not parked as ask_user; assert the exact outcome and
  the exact rejection note the core produces for a forbidden action type (find it in
  `core/src/session.rs` where `allows_ask_user` is consulted; quote it, do not paraphrase).
  Assert `allows_ask_user()` is false for `RouterSkill` and true for `PlanSkill` and `ImplementSkill`.
- Existing green: `core/tests/it prompt::`, `skills/tests/it` full run.

## Boundaries
`skills/src/coding/mod.rs`, `core/src/prompt.rs`, the test files named above. No other files.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test it prompt::`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`.
