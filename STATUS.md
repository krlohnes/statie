# Status, 2026-09-21, paused mid-morning

## Where the code is
Pushed through `099c130` (items 28 to 48, builds through 32 short).
Unpushed since: items 49 to 56 (Milestones 55 to 62), the item 49
`next_skill` fix, builds 33 to 36. Items 36 and 41 were committed and reverted
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
  `think`. Build 33, gemma short twice: 32 of 32, no walk.
- Items 49, 50 (Milestones 55, 56): the implement-review cap and the
  read-only step rule, from build 32's 188-step ping-pong and
  alternating walk. Never fired on build 33.
- Item 51 (Milestone 57): a permission ask ("Would you like me to
  proceed?") is a refusal, not a park. Item 52 (Milestone 58): a call
  named by its file path with only the path is a read. Both from
  build 33's Qwen parks. Build 34: Qwen short 16 of 16, gemma 16 of
  16, Qwen long 8 of 10 (best yet; not-a-tool parks 0 against 2).
- Item 53 (Milestone 59): all-uncited review findings are a refusal,
  not a rejection; review runs the refusal think. From build 34's two
  Qwen `failure` parks with the crate green. Measured on build 35.
- Item 33's 8-window rows at 16k: landing unmoved; default stays
  4 windows at 6,000; 24k rows not run.

## Where statie stands against OpenCode
- Short tasks, 16k: gemma 16 of 16 on four batteries running
  (OpenCode 16); Qwen 16 of 16 on build 34 (OpenCode 14). Time: gemma
  66 to 88 s a run against 21; Qwen 54 to 99 s against 15. The host's
  prompt eval varies by half across batteries at unchanged prompt
  size, so time is read with `peval`.
- Long tasks, Qwen, 16k: 8 of 10 on build 34 (OpenCode 6 of 10 at
  16k, 10 of 10 at 32k). `refactor_signature` is the only unlanded
  task, at the no-green and step caps.
- Long tasks, Qwen, 16k: 6 of 10 (OpenCode 6 of 10 at a 16k limit,
  10 of 10 at 32k). Time: 261 s against 53. `refactor_signature` is
  unlanded by statie with any model.

## Running now
Nothing on the host. Build 37 (item 56, the anchored `old` match) was
stopped at the pause after 9 of its 10 Qwen long runs: 6 landed of 9
(the tenth, `trap_interest` seed 2, never ran), kept as
`bench-runs/qwen_json_long_b37_partial`. "old text not found" errors
7 on those 9 runs against 26 on build 36's 10, the anchored match
fired 3 time(s); parks: no green in 16 test runs. Item 56 is not yet written up in
README.md.

## Resume with
1. Write build 37's partial into README.md and BACKLOG.md item 56;
   decide whether to rerun the long battery whole.
2. Build 37 short batteries (Qwen, gemma) for item 56 on the short
   tasks, `$S/statie-b37` is the binary.
3. Push is yours: main is ahead of `099c130` by items 49 to 56. Gates must not run during a
battery when time is the question: the sandboxes' cargo shares this
machine.

## Open threads
- Push is yours.
- The empty-plan refusal in `plan` has no repeat rule (item 37's
  shape on a different refusal); one run on build 26.
- `recent_summary` over its cap four times in a row parked one Qwen
  run; noted, not an item.
- OpenCode's shell tool is not confined to its working directory.
