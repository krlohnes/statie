# Status, 2026-09-15 evening

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 35 and 37 to
39, the Qwen and OpenCode bench tooling, D23 to D27, and builds 15 to
24. Item 36 was committed and reverted (D26). Tree clean. Gate: 16
binaries, 12 to 19 minutes.

## What today's measurements said
- The step is output, not cache (D25 addendum): prompt evaluation is
  0.5 s of a 3.25 s step; generation 1.1 s (220 tokens against
  OpenCode's 49); the maintainer's call 1.4 s. The maintainer-off arm
  priced it: 1.3 s a step and three runs of landing (D17 addendum).
  It stays on; a cheaper maintainer is the lever.
- Item 37 (a refused `done` cannot mark plan steps done): gemma 16 of
  16 on build 23, zero plan walks against three on build 22.
- Item 38 (the thinker sends the configured `think` flag): Qwen 15 of
  16 on build 23 against 11 to 13 before; every `think` answered where
  none ever had. One ahead of OpenCode's 14 on landing, nine times its
  wall clock.
- Item 39 (calls that name no tool are one streak): committed,
  Milestone 45, from the one build-23 Qwen miss (107 alternating
  rejections, no park). Measured next on `qwen_json_short_b24`.
- The long-task gap at equal room (D27): OpenCode with Qwen at a 16k
  limit lands 6 of 10 at 53 s; at 32k it landed 10 of 10; statie
  build 22 landed 3 of 10 at 277 s. Room was worth four runs to the
  generic loop; the rest is the harness.

## Running now
`qwen_json_short_b24` (item 39 binary) on the host. Next: statie with
Qwen on the long tasks on that binary, against D27's 6 of 10.

## Open threads
- Push is yours.
- Item 33's sweep: six combinations unrun, paused by you. D27 says the
  long-task work is inside 16k, so the 24k rows matter less; the
  8-window rows at 16k still ask a live question.
- The maintainer is 1.3 s a step. Nothing proposed yet for running it
  on fewer steps; it needs a design, not a knob.
- OpenCode's shell tool is not confined to its working directory.
