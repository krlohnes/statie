# Unit 46: the maintainer is not skipped after a refusal

## Background (read, do not restate)
Item 43 (`core/src/session.rs`, search `maintainer_skipped`) made the maintainer run off the
critical path (`spawn_maintainer`/`drain_pending_maintainer`) and added a skip rule: no
maintainer after a step whose committed action was a harness `Continue`, or whose entries
carried `tool_rejected` or `skipped_redundant`. Build 27 showed the skip starving the notes
exactly during gemma's refusal walks (DECISIONS.md D29). The overlap stays; the skip goes.

## Spec
1. Remove the skip rule: whenever `run_maintainer` is true (the same outcomes as before item
   43: `Continue`, `Parked(AskUser)`, `Parked(Yield)`), the session drains any pending
   maintainer and spawns the new one, regardless of the step's action or entries. Delete the
   `maintainer_skipped` event and the `skip_reason` computation and any helper only they used.
2. Nothing else from item 43 changes: `PendingMaintainer`, the drain before every terminal
   return, `notes_step`, the heading.

## Acceptance
- `core/tests/it/maintainer.rs`: the test `maintainer_skipped_and_not_called_after_a_harness_
  continue_step` is rewritten to assert the opposite with exact values: after a harness
  `Continue` step the maintainer IS called (recorder count exactly one more than before) and no
  `maintainer_skipped` entry exists anywhere in the log. `skills/tests/it/recovery.rs`'s
  `the_maintainer_is_skipped_but_drains_after_a_skipped_step` is rewritten back to the
  maintainer running after a skipped read (calls exactly 2, as before item 43, allowing for the
  one-step drain), with its pre-item-43 name. Report both old and new assertions.
- `grep -rn maintainer_skipped core/ skills/` returns nothing.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`; `cargo clippy -p statie-core --all-targets -- -D warnings`
  clean.

## Boundaries
`core/src/session.rs`, `core/tests/it/maintainer.rs`, `skills/tests/it/recovery.rs`, and any
test the compiler or a failing assertion names. No other changes. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo test -p statie-core --test it`,
`cargo test -p statie-skills --test it`, `cargo test -p statie --bin statie`,
`cargo clippy -p statie-core --all-targets -- -D warnings`.
