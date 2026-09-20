# Unit 43: the maintainer runs off the critical path; notes are one step stale

## Background (read, do not restate)
`core/src/session.rs`: after a step commits (`Outcome::Continue`, or a park on `ask_user`/`yield`),
the loop calls `maintainer.maintain(input).await` (search `run_maintainer` and `.maintain(`), and
commits the resulting notes (and `next`, when the maintainer proposes one) before the next prompt
is built. That call is 1.3 s of a 3.25 s step (DECISIONS.md D17 addendum). On `short_fixer_b23`,
42 of 235 maintainer calls followed a harness `Continue`, a `tool_rejected`, or a
`skipped_redundant` read, where the observation carried nothing new.

## Spec
1. After step N commits, the maintainer's call for step N is started as a task
   (`tokio::spawn` or a `JoinHandle`-like future held on the session) and NOT awaited before
   step N+1's prompt is built. Step N+1's prompt uses the notes already committed (from step
   N-1's maintainer, or earlier). Before step N+2's prompt is built, the session awaits step N's
   maintainer result and commits it exactly as today (same log entries, same `maintainer` /
   `maintainer_error` system events, same `next` handling), then starts step N+1's maintainer.
   Net: at most one maintainer call in flight; every prompt sees notes at most one step stale.
2. The maintainer is not started after a step whose committed action was `Action::Continue`,
   or whose entries carry a `tool_rejected` or `skipped_redundant` system event; the previous
   notes stand and a system entry `{"event": "maintainer_skipped", "reason": "<continue|rejected|
   skipped_read>"}` is logged.
3. The prompt's notes section header says which step the notes summarise: find where the
   notes section is rendered (`core/src/prompt.rs`, `IDX_NOTES`) and append ` (as of step {n})`
   to its heading, where `{n}` is the step whose maintainer produced them; the harness state
   records that step (`HarnessState.notes_step: Option<u64>`, serde default, doc comment).
4. Parks, `Done`, handoffs, cancellation, and session end: any maintainer task in flight is
   awaited (bounded by the maintainer's own timeout) and its result committed (or its error
   logged) before the outcome is returned, so no commit races the store and no task outlives
   the session. A maintainer error leaves the previous notes, as today.
5. No change to what the maintainer is given or produces; no change to the acting model's
   prompt other than the header in 3.

## Acceptance
- `core/tests/it/` (the module that tests the maintainer wiring; find by `"event": "maintainer"`
  or `MaintainInput`): with a scripted maintainer that records the step it was called for and
  returns distinct notes per call, a five-step scripted session asserts, per step, exactly which
  notes text the prompt carried (step 1: none; step 2: none or step-0 notes per your design;
  step 3: step 1's notes; ...) and the exact `notes_step` in the committed harness state; the
  session log's `maintainer` events appear in step order with the same payload shape as today.
- The skip rule: a step whose action was a harness `Continue` (use an existing fixture that
  produces one, e.g. the open-plan-step refusal) logs `maintainer_skipped` with the exact reason
  and calls the maintainer zero times for that step (assert the recorder's call count).
- The end-of-session rule: a scripted maintainer that sleeps 200 ms on its last call; the
  session's final outcome is returned only after that call's notes are committed (assert the
  notes are in the final state).
- The prompt header: `core/tests/it/prompt.rs` asserts the exact notes heading with a step.
- Every existing test that asserts maintainer timing or notes content is updated to the new
  (one-step-stale) truth with exact values; none is loosened or deleted. Report each such test.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`.

## Boundaries
`core/src/session.rs`, `core/src/version.rs`, `core/src/prompt.rs`, `core/src/maintainer.rs`
(only if the trait needs a `Send` bound for spawning), the core and skills test modules the
compiler or a failing assertion names, `store/src/test_support.rs` and `models/tests/it/common`
only for a `HarnessState` literal. No skills or tools source changes. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie --bin statie`.
