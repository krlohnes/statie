# Unit 20: `try_fixes` is offered only when there is a failure to fix

## Spec
1. In `skills/src/coding/mod.rs`, `ImplementSkill::tools()` lists `try_fixes`
   only when the current workspace's `tests` record exists and has `pass == false`
   (`CodingWorkspace::tests: Option<TestRecord>`). Find how `tools()` gets access
   to the current workspace (if it takes none today, add the smallest hook the
   session already offers for state-dependent tool lists; look at how `think`'s
   listing and `HarnessState.try_fixes_available` are handled and follow that).
   If the tool list is genuinely static in the skill trait, implement the gate in
   `validate` instead: a `try_fixes` call with no failing `tests` record is
   substituted (point 2) and the prompt's rules text says "`try_fixes` is only
   useful after a failing `test`". Say in the report which of the two you did.
2. In `ImplementSkill::validate`, a `try_fixes` action arriving when the
   workspace has no failing `tests` record is substituted with
   `Action::Tool { name: "test", args: {} }` using the existing `substitute_action`
   helper, and the step's note reads exactly:
   "`try_fixes` needs a failing test to work from, so the harness ran `test` instead."
   This substitution must not count as a tool error.
3. Nothing changes when a failing `tests` record exists.

## Acceptance
- `skills/tests/try_fixes_gate.rs` (new): (a) with `tests: None`, a `try_fixes`
  call becomes a `test` call at that same step, the note text above is logged,
  and no `tool_error` entry exists; (b) with `tests: Some(pass: false)`, `try_fixes`
  passes through unchanged (assert the action name at that step is `try_fixes`);
  (c) with `tests: Some(pass: true)`, it is substituted like (a). If you built the
  listing gate, also assert the rendered prompt's tool list omits `try_fixes` in
  (a) and includes it in (b) (see how `skills/tests/think_listed.rs` checks the
  prompt via `MockModel::prompts()`).
- Existing `try_fixes` tests in `core/tests/` and `skills/tests/` unchanged and green.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/tests/try_fixes_gate.rs`.
Allowed cargo: `cargo build -p statie-skills`, `cargo test -p statie-skills --test try_fixes_gate`,
`cargo test -p statie-skills --test think_trigger`, and `cargo test -p statie-skills --test <name>`
for any skills test you believe you affected.
