# Unit 30: a question asked after the plan is written is the hand-off

## Spec
In `skills/src/coding/mod.rs`, `PlanSkill::validate`, immediately after the
`ask_user_empty_note` handling and before the `is_done` block:
- If `out.action()` is `Action::AskUser { message, .. }` and the merged plan is non-empty
  (compute it the same way the `is_done` block computes `merged_plan_nonempty`: apply this
  patch to `cur.agent` and read `plan`; factor that computation into a helper used by both),
  then:
  - if the patch has no `next_skill`, set `patch.next_skill = "implement"` (leave an existing
    `next_skill` alone);
  - set the action to `Action::Done`;
  - add the note, exactly:
    `pub const PLAN_ASK_HANDOFF_NOTE_PREFIX: &str = "The plan is written, so the harness handed off to `implement` instead of asking. The plan answers the question; `implement` has the editing tools. The question was: "`
    followed by the first 200 characters of the message (chars, not bytes) and, if truncated, `…`.
  Re-export the constant from `skills/src/lib.rs`.
- An `ask_user` with an empty merged plan is not touched.
- Do not change any other trigger's order or text.

## Acceptance
- `skills/tests/it/plan_ask_handoff.rs` (new module in `skills/tests/it/main.rs`): copy the
  session setup from `skills/tests/it/bare_done.rs` or `router.rs`.
  (a) With `plan` active and a patch carrying a 4-step plan plus an `ask_user` "Should I hand off
  to the implement skill now?": the step's outcome is the hand-off (assert exactly what the
  existing hand-off tests assert, e.g. `Outcome::Handoff("implement")` or the equivalent shape the
  core produces; read `bare_done.rs`), the committed state has `next_skill == Some("implement")`
  and the 4-step plan, and the step's notes contain the exact note text with the question.
  (b) Same, but the patch already carries `next_skill: "review"`: `next_skill` stays "review".
  (c) With an empty plan in state and no plan in the patch, an `ask_user` "Which file?" parks as
  `Park::AskUser` with that message, unchanged.
  (d) A question of 250 characters is quoted as its first 200 characters plus `…`.
- Existing green: `skills/tests/it` full run, `cargo test -p statie-skills --lib`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/lib.rs`, the new test module and its `main.rs` line.
No other files. No .md edits.
Allowed cargo: `cargo build -p statie-skills`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`.
