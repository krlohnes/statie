# Unit 55: the router's bare `done` is routed by state

## Background (read, do not restate)
Qwen's first step in nearly every run is `{"patch": {"task": "..."}, "action": {"type": "done"}}`
in the `router` skill: no `next_skill`. `validate_default`'s handoff rule rejects it ("action
`done` is only valid when the workspace is terminal or hands off"), the model retries with
`next_skill: plan`, and the run goes on. Count: 15 of 16 Qwen short runs on each of builds 33,
34 and 35, 9 of 10 Qwen long runs on build 34; gemma 0. One burnt retry and one wasted model
call per run, and the retry cap is three. `RouterSkill::validate` (`skills/src/coding/mod.rs`)
runs `strip_harness_owned`, `strip_malformed_next`, `router_done_not_ours_note`, then
`validate_default`. `RouterSkill::allowed_transitions` is `plan`, `implement`, `review`.

## Spec
1. New `router_bare_done_routes(cur, out) -> Option<String>`, called in `RouterSkill::validate`
   after `router_done_not_ours_note` and before `validate_default`. Fires when the action is
   `Done`, the merged workspace's `done` is false, and the merged `next_skill` is `None` or
   `Some("router")`. Effect: insert `next_skill` into the raw patch by the merged state, in
   this order: `plan` empty -> `"plan"`; any plan step `pending`/`in_progress` -> `"implement"`;
   otherwise `findings` non-empty -> `"implement"`; otherwise -> `"review"`. Action stays
   `Done` (a real handoff). Note: `pub const ROUTER_ROUTED_NOTE_PREFIX: &str = "`done` named
   no skill; the harness routed it by state to "` followed by the skill in backticks, a colon
   and space, and the reason: one of `"the plan is empty."`, `"plan step {id} is open."` (the
   first open step's id), `"findings are open."`, `"the plan is done and unreviewed."`.
   A `done` that already names a valid `next_skill`, or one whose merged `done` is true, is
   untouched (the existing rules apply).
2. Nothing else changes; the router's own spec text may gain one sentence saying a bare
   `done` is routed by state (find the router's spec string; keep it to one sentence).

## Acceptance (exact strings; never loosen an existing assertion)
- `skills/src/coding/tests.rs` unit tests, each asserting the exact note and the exact
  `next_skill` in the validated patch, action still `Done`: (a) empty plan (the live shape:
  patch `{"task": "..."}`, action `done`) -> `plan`; (b) a plan with step 2 open -> `implement`,
  note names step 2; (c) all steps done, findings non-empty -> `implement`; (d) all done, no
  findings -> `review`; (e) `done` with `next_skill: "implement"` already set -> untouched, no
  note; (f) `done` with merged `done: true` -> `router_done_not_ours_note`'s existing
  behaviour, unchanged (assert its note).
- `skills/tests/it/` end-to-end (find the router fixture, e.g. where the router's first step
  is exercised): the live shape commits as a handoff to `plan` on step 1 with the exact note
  in the log and NO `rejected` entry; the second activation is `plan`.
- Existing green: `cargo test -p statie-skills --lib`, `cargo test -p statie-skills --test it`,
  `cargo clippy -p statie-skills --all-targets -- -D warnings`, `cargo fmt -p statie-skills`.
  Any existing test that asserted the rejection for this shape is updated to the new
  contract; name it and say why.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/coding/tests.rs`, `skills/src/lib.rs` (export the
constant), `skills/tests/it/*.rs`. No core, tools, cli, bench/ or .md edits.
Report: files changed, exact constant, every existing test whose expectations changed and why,
commands run with exit codes.
