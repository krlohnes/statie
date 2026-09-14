# Status, 2026-09-14 evening

## Where the code is
Main is ahead of the last push (`c191e12`) by the items 28 to 32 work,
the Qwen configs, D23 and D24, and builds 15 to 19. Tree clean apart
from the item 33 backlog entry. Gate: 16 binaries, 12 to 19 minutes.

## What happened since the last status
- A second model (qwen3-coder, 30B mixture-of-experts) was run as a
  prompt lint (D23). It exposed five harness defects in a day, each
  fixed and gated: a copyable example message (28), rejections that
  did not say where the tool lives or count as repeats (29), a
  permission question after the plan is written (30), an edit contract
  that demands verbatim copying (31, line-addressed edits), nameless
  tool calls the arguments identify (32).
- Decoding format is a per-model config choice (D24): Qwen under
  `format: schema` 0 of 16; under `json` 11, 12, 11, 13, 13 of 16
  across builds 15 to 19. gemma on the same builds 13 to 15 of 16.
- gemma discovered the `lines` edit form unprompted (nine edits on
  build 18); Qwen used it once the anchor-miss error offered it.

## Running now
Nothing. The item 33 agent (knob sweep runner, bench scripts only) is
building; no cargo, bench, or model call is in flight.

## Next
- Item 33 lands, then the first knob sweep on the Qwen json config:
  `max_windows` 4 and 8, `budget.working_set` 6000 and 9000, `num_ctx`
  16384 and 24576, eight short tasks, two seeds. About six batteries.
- Push is yours.
