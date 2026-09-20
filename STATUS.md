# Status, 2026-09-20, paused mid-morning

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 35, 37 to
40, and 42 to 48, the Qwen and OpenCode bench tooling, D23 to D30,
and builds 15 to 32 (short). Items 36 and 41 were committed and reverted
(D26, D28). Tree clean. Gate: 16 binaries, 12 to 19 minutes.

## Committed since the pause
- Item 41 reverted (D28): both models wrote less and landed less.
- Item 43 (Milestone 49): the maintainer runs off the critical path,
  notes one step stale. Build 27: Qwen 15 of 16 at 66 s against 104;
  gemma 12 of 16, its losses refusal walks where the skip rule starved
  the notes. D29 keeps the overlap, drops the skip.
- Item 44 (Milestone 50): the prompt fits by eviction before it
  fails. Build 28, Qwen long: 6 of 10 against 2 of 10, zero over-budget
  failures, 509 evictions. D27's landing check is met; time is not
  (261 s a run against OpenCode's 53).
- Item 45 (Milestone 51): notes over budget are cut, not dropped.
  Build 29, Qwen short: 16 of 16 at 55 s, the first full Qwen
  battery; the cut never fired, so the sixteenth run is not its doing.
- Item 46 (Milestone 52): the skip rule removed. Build 30: gemma 13 of
  16, Qwen 14 of 16; gemma's losses are refusal walks with the notes
  one step late.
- Item 48 (Milestone 53): `maintainer.overlap` is a per-model knob.
  Build 31, gemma with it off: 16 of 16 at 67 s a run, zero parks.
  D30: off for gemma, on for Qwen.
- Item 47 (Milestone 54): the second refusal in a row becomes a
  `think`; a committed `think` is neutral for the refusal counters.
  Build 32 short: gemma 15 of 16, Qwen 14 of 16, not separable from
  builds 30 and 31; the trigger fired four times.
- Item 33's 8-window rows at 16k: landing unmoved; default stays
  4 windows at 6,000; 24k rows not run.

## Where statie stands against OpenCode
- Short tasks, 16k: gemma 16 of 16 (OpenCode 16); Qwen 16 of 16
  (OpenCode 14). Time: gemma 67 s a run against 21; Qwen 55 to 62 s
  against 15.
- Long tasks, Qwen, 16k: 6 of 10 (OpenCode 6 of 10 at a 16k limit,
  10 of 10 at 32k). Time: 261 s against 53. `refactor_signature` is
  unlanded by statie with any model.

## Running now
Nothing on the host: the build 32 Qwen long battery was stopped at the
pause after 5 of 10 runs (all landed; kept as
`bench-runs/qwen_json_long_b32_partial`), and the chained second gemma
short battery never started. Two agents may still be finishing in
worktrees `wt_49` (cap the implement-review round trip) and `wt_50` (a
redundant read of the open step's file finishes the step); their work
is unreviewed and not in main.

## Resume with
1. Review items 49 and 50 (briefs in `.claude/briefs/`), apply, gate,
   commit.
2. Build 33 on both: gemma short (overlap off), Qwen short, Qwen long;
   a second gemma short battery to settle item 47 (15 vs 16 on one
   battery each does not separate).
3. Check: activations per run at most 6; alternating walks at zero;
   gemma at 16 of 16.

## Open threads
- Push is yours.
- The empty-plan refusal in `plan` has no repeat rule (item 37's
  shape on a different refusal); one run on build 26.
- `recent_summary` over its cap four times in a row parked one Qwen
  run; noted, not an item.
- OpenCode's shell tool is not confined to its working directory.
