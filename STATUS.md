# Status, 2026-09-15, paused at 16:57

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 35 and 37 to
42, the Qwen and OpenCode bench tooling, D23 to D27, and builds 15 to
25. Item 36 was committed and reverted (D26). Tree clean. Gate: 16
binaries, 12 to 19 minutes. Nothing is running on the host or the Mac.

## Committed since the morning
- Item 37 (Milestone 43): a refused `done` cannot mark plan steps
  done. gemma 16 of 16 on build 23, zero plan walks.
- Item 38 (Milestone 44): the thinker sends the configured `think`
  flag. Qwen 15 of 16 on builds 23 and 24, every `think` answering
  where none ever had.
- Item 39 (Milestone 45): calls that name no tool are one streak. On
  the Qwen long tasks it turned 126-step timeouts into 25-step parks.
- Item 40 (Milestone 47): tool descriptions state their forms and
  failure texts. Unmeasured; build 26 is its battery.
- Item 41 (Milestone 46): the coding skills say "patch what changed".
  gemma build 25: output down a fifth, 24% faster, 13 of 16 against
  16. Decision open; see below.
- Item 42 (Milestone 48): a nameless call with `read_file`'s arguments
  is a `read_file` call. Unmeasured; the Qwen long battery is its test.

## What the measurements said
- The step is output, not cache (D25 addendum). The maintainer is
  1.3 s a step and three runs of landing (D17 addendum); it stays on.
- OpenCode at equal room (D27): Qwen long tasks 6 of 10 at 53 s with a
  16k limit, 10 of 10 at 32k; statie 3 of 10 at 236 to 277 s on builds
  22 and 24. Room was four runs; the rest is the harness.
- OpenCode's prompt is not model-tuned; its edge is "minimize output"
  said three ways and tool descriptions that carry their failure
  texts. Items 40 and 41 test each.

## Open decisions
- Item 41: keep, revise, or revert. gemma lost three runs on two
  seeds while writing less; two of the three misses are shapes the
  paragraph could have invited (a `done` on an empty patch; a skipped
  test). The Qwen short battery on build 25 is the other half of the
  evidence: Qwen build 25 landed 14 of 16 against 15, at 116
  tokens a step against 166 and 135 s a run against 112.
- Item 33's sweep: six combinations unrun, paused by you.
- The maintainer's 1.3 s: needs a design for running it on fewer
  steps, not a knob.

## Next batteries, one at a time
1. gemma and Qwen short on a build 26 binary (items 40 and 42 in).
2. Qwen long on build 26 (items 39 and 42 against D27's 6 of 10).

## Open threads
- Push is yours.
- OpenCode's shell tool is not confined to its working directory.
