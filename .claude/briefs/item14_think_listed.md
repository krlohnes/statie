# Unit 14: switch to hide `think` from the tool list

## Spec
Config `thinker.listed` (bool, default true) in `cli/src/config.rs`. When
false, `think` is not in the tool list the model sees (`skills/src/coding/mod.rs`
around the tool-list constructions at ~3321 and ~3650), but the harness can
still substitute `think` itself (repeated-failure and early-park triggers keep
working, since they construct `Action::Tool { name: "think" }` directly).
Thread the flag the same way as `thinker.chunked`.

## Acceptance
- `skills/tests/think_listed.rs`: with `listed=false` the prompt's tool list
  does not contain `think` and the trigger still produces a `think` action at
  the same step it does today (copy the step assertion from
  `skills/tests/think_trigger.rs`, do not weaken it). With `listed=true`
  (default) the list contains it.
- `cli` config test: `thinker.listed` absent → true.

## Boundaries
`cli/src/config.rs`, `cli/src/main.rs` (wiring), the skill config struct in
`skills/src/coding/mod.rs`, new `skills/tests/think_listed.rs`.
