# Unit 41: the coding skills say "patch what changed" where the model reads it

## Background (read, do not restate)
The merge strategy paragraph in the system block (`patch/src/merge.rs` `describe()`) already
says "send only the fields you want to change" and that keyed arrays merge entry by entry. The
model does not act on it: on `short_fixer_b23` (gemma) the unchanged `task` was resent on 115 of
308 steps and the whole plan on 40; `plan` is 50% of patch characters, `recent_summary` 23%,
`task` 13%. On `qwen_json_short_b24` `recent_summary` is 53% of patch characters. Median
completion is 220 tokens a step and generation is 1.1 s of the 3.25 s step (DECISIONS.md D25
addendum). The skill specs (`skills/src/coding/mod.rs`, the `spec()` of `PlanSkill`,
`ImplementSkill`, `ReviewSkill`, `RouterSkill`) tell the model what to put in the workspace but
never how little to send per step; `plan`'s "Keep `recent_summary` current" invites a rewrite
every step.

## Spec
1. Add one shared paragraph, `PATCH_WHAT_CHANGED` (a `pub const &str` in `skills/src/coding/mod.rs`),
   appended to the instructions of all four coding skills (after `head`, before any tail),
   with exactly this text:
   "Patch only what changed this step. Never resend `task`. To move a plan step, send only
   that entry with its `id` and the changed field, e.g. {\"plan\": [{\"id\": 3, \"status\":
   \"done\"}]}; do not resend the other steps. Rewrite `recent_summary` only when what you know
   changed, in at most two sentences. A step whose state did not change sends an empty patch:
   {\"patch\": {}, \"action\": ...}."
2. In `PlanSkill`'s `head`, replace the bullet "Keep `recent_summary` current as you explore."
   with "Update `recent_summary` when you learn something that changes the plan." In
   `ImplementSkill`'s spec, find any sentence that asks for `recent_summary` on every step and
   make it conditional the same way; report each sentence changed.
3. No harness behaviour change: no guard, no note, no merge change. The example patch in the
   paragraph is a shape, not an answer (D23): it names no task and no real plan text.

## Acceptance
- `skills/tests/it/` (the module that snapshots or asserts spec text; find by "Keep
  `recent_summary` current"): each of the four skills' rendered instructions contains
  `PATCH_WHAT_CHANGED` exactly once (assert with the imported const, exact), and the plan
  skill's instructions contain the new bullet and not the old one (exact strings).
- Every existing test that snapshots spec text is updated to the exact new text; none is
  loosened or deleted.
- Existing green: `cargo test -p statie-skills --test it`, `cargo test -p statie-skills --lib`,
  `cargo test -p statie --bin statie`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/lib.rs` (export the const), `skills/src/coding/tests.rs`,
`skills/tests/it/*.rs`, and any test in `cli/` the compiler or a failing assertion names
(updated to exact new text). No core, tools, or patch source changes. No .md edits.
Allowed cargo: `cargo build -p statie-skills`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie --bin statie`.
