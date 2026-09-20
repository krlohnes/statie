# Unit 47: the second consecutive refusal becomes a `think`; the walk ends there

## Background (read, do not restate)
`implement`'s `validate` (`skills/src/coding/mod.rs`) turns several wrong actions into a
harness `Action::Continue { note }` that keeps the patch and tells the model what to do:
`premature_done_note` (a `done` on an open plan step; item 37 already varies the note on a
repeat using `cur.harness.consecutive_continues`), `implement_no_green_runs_test`'s
Continue fallback (a `done` after a failing test with nothing edited: "the most recent `test`
run failed ... handing off needs a passing run"), `ask_user_empty_note` (an `ask_user` with
no question), and the plan skill's empty-plan refusal. `HarnessState.consecutive_continues`
(item 37) counts committed harness `Continue` actions in a row. Models answer a refusal by
repeating the refused action: `short_fixer_b30` had five runs with three or more consecutive
`done`/`ask_user`/`continue` actions, `b27` five, `b26` three, up from one on `b23`; three of
`b30`'s four misses are such walks to the non-progress cap (six steps each). The harness
already substitutes a `think` on the second strike of other loops (`implement_repeated_error_
thinks`, the skip and stall thinks): D19, second strike a question, third a park.

## Spec
1. Add `pub const REFUSAL_QUESTION: &str` in `skills/src/coding/mod.rs`: "The harness refused
   your last two actions for the same reason: {note}. You must not send that action again.
   Which single tool call moves this step forward now: `test`, `read_file` on the step's file,
   `edit_file`, or, if you are truly blocked, `ask_user` with a real question? Answer with
   the exact call." ({note} is the refusal note text of the current refusal.)
2. In `implement`'s `validate`, after the existing note functions have decided on a harness
   `Continue { note }` (any of the refusals listed above) and when
   `cur.harness.consecutive_continues >= 1` (the previous committed action was already a
   harness `Continue`) and the skill is configured with a thinker (`think` available the way
   `implement_repeated_error_thinks` checks it), replace the action with `Action::Tool { name:
   "think", args: {"question": REFUSAL_QUESTION formatted} }` instead of the `Continue`, keep
   the patch as the refusal would have, and add a note `{REFUSAL_THINK_NOTE_PREFIX}` + the
   refusal note (a `pub const` prefix: "Second refusal in a row; the harness asked `think`: ").
   Once per streak: if the previous committed action was itself this substituted `think`, do
   not substitute again (fall through to the refusal as today, so the third strike reaches the
   existing cap). Follow the "once per streak" mechanics of `implement_repeated_error_thinks`
   (`HarnessState.last_think_step` or equivalent) exactly.
3. The plan skill's empty-plan refusal gets the same treatment in `plan`'s `validate`, with the
   same constants.
4. No change when there is no thinker configured (the refusal stands as today).

## Acceptance
- `skills/tests/it/` (the module testing item 37, find by `OPEN_STEP_NOTE_REPEAT`): (a) an
  `implement` session with a thinker: `done` on an open step (refused, note exact as today),
  then `done` again → the committed action is `think` with the exact `REFUSAL_QUESTION` text
  (assert the question string exactly) and the note carries the prefix + the refusal note
  exactly; then a third `done` → the refusal note again, not another `think` (once per
  streak); (b) the same for `done` after a failing test with nothing edited (the
  no-green Continue fallback) and for two empty `ask_user`s; (c) without a thinker, two
  refusals in a row produce two refusal notes and no `think`; (d) a tool call between two
  refusals resets: the second refusal is a plain note.
- `skills/src/coding/tests.rs`: unit tests for the formatted question on one refusal text
  (exact).
- Every existing test asserting the second refusal's note (item 37's repeat tests) is updated
  only where a thinker is configured in its fixture; where the fixture has no thinker the
  assertions stay. Report each change.
- Existing green: `cargo test -p statie-skills --test it`, `cargo test -p statie-skills --lib`,
  `cargo test -p statie --bin statie`; `cargo clippy -p statie-skills --all-targets -- -D warnings`.

## Boundaries
`skills/src/coding/mod.rs`, `skills/src/lib.rs` (exports), `skills/src/coding/tests.rs`,
`skills/tests/it/*.rs`, and any test the compiler or a failing assertion names. No core
changes. No .md edits.
Allowed cargo: `cargo build -p statie-skills`, `cargo test -p statie-skills --test it`,
`cargo test -p statie-skills --lib`, `cargo test -p statie --bin statie`,
`cargo clippy -p statie-skills --all-targets -- -D warnings`.
