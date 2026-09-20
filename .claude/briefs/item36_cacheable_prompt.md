# Unit 36: a cacheable prompt layout

## Spec
1. `core/src/prompt.rs`, `PromptBuilder::build`: the user block is joined in this order:
   `IDX_AGENT, IDX_WORKING_SET, IDX_NOTES, IDX_SITUATIONAL, IDX_AUGMENTATION, IDX_HARNESS, IDX_OBSERVATION`.
   The system block (`IDX_SYSTEM, IDX_SPEC, IDX_TOOLS`) is unchanged. Section headings and the
   text inside each section are unchanged except as stated below. Any note text elsewhere that
   says "your working set above" stays true (the working set is still above the observation).
2. `core/src/working_set.rs`, the rendered `text`: the distinct windows are rendered OLDEST first
   (ascending step) instead of newest first; the one-line recent-calls summaries are rendered
   AFTER the windows instead of before. Selection of which windows are kept (`max_windows`
   newest, plus the edit-pairing rule) is unchanged; only render order changes. `window_steps()`
   keeps returning the steps of the rendered windows (order: ascending).
3. Nothing else in the prompt gains a per-step counter. Audit the sections that now precede the
   harness view (Agent State, working set, notes, Situational, Augmentation) for any text that
   embeds the current step number or a per-step counter; report every such place. Do not change
   them in this unit unless it is a formatting artefact (e.g. a "step N" label in the working set's
   window headers is intentional and stays).

## Acceptance
- `core/tests/it/prompt.rs`: (a) a test that builds two consecutive prompts for the same state
  where only the observation and `harness.step` differ, and asserts the common byte prefix of the
  two user blocks extends through the end of the working-set section (compute the prefix length
  and compare it to the offset of the notes heading in the second prompt; assert exact). (b) the
  section order test updated to the new order, exact. (c) existing snapshot tests updated to the
  new order and the working set's new render order, exact strings.
- `core/tests/it/working_set.rs` (or wherever the working-set render tests live): windows render
  oldest first, recent calls after windows; existing assertions updated to the new order, exact.
- `skills/tests/it` tests that snapshot the prompt (e.g. `situational.rs`, `edit_lines.rs` prompt
  checks) updated for the order, exact strings only.
- Existing green: `cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
  `cargo test -p statie --bin statie`.

## Boundaries
`core/src/prompt.rs`, `core/src/working_set.rs`, the test modules named (and any test the
compiler or a failing assertion names, updated to exact new text). No other source files. No .md edits.
Allowed cargo: `cargo build -p statie-core`, `cargo build -p statie-skills`,
`cargo test -p statie-core --test it`, `cargo test -p statie-skills --test it`,
`cargo test -p statie --bin statie`.
