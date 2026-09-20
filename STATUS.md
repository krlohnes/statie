# Status, 2026-09-20

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 35 and 37 to
44, the Qwen and OpenCode bench tooling, D23 to D28, and builds 15 to
26. Items 36 and 41 were committed and reverted (D26, D28). Tree
clean. Gate: 16 binaries, 12 to 19 minutes.

## Committed since the pause
- Item 41 reverted (D28): output fell a fifth to a third on both
  models and landing fell on both.
- Item 43 (Milestone 49): the maintainer runs off the critical path;
  notes are one step stale; steps that produced nothing new skip it.
  Unmeasured; build 27 is its battery.
- Item 44 (Milestone 50): the prompt fits by evicting from the
  working set before it fails. From build 26's Qwen long battery,
  where five of ten runs failed a step on the total after item 40
  added 800 tokens of descriptions. Unmeasured; build 28 is its
  battery.

## Build 26 (items 40 and 42)
- gemma short: 15 of 16 at 76 s a run, the fastest gemma battery;
  anchor misses now recovered with a think and a corrected edit.
- Qwen short: 15 of 16 at 104 s, unchanged landing, tool errors
  doubled across kinds. Item 40 kept.
- Qwen long: 2 of 10, five of them the over-budget failure (item 44).
  Item 42's inference never fired: the nameless calls were bare
  `{path}` or empty, the shapes it leaves alone.

## Running now, one at a time
1. Item 33's two 8-window rows at 16k (Qwen short): row 1 done, row 2
   at 13 of 16.
2. Build 27: gemma and Qwen short on the off-critical-path maintainer
   (item 43), against builds 26.
3. Build 28: Qwen long on items 43 and 44, against build 24's 3 of 10
   and D27's 6 of 10.

## Open threads
- Push is yours.
- The empty-plan refusal in `plan` has no repeat rule (item 37's
  shape on a different refusal); one run on build 26.
- `recent_summary` over its cap four times in a row parked one Qwen
  run; noted, not an item.
- OpenCode's shell tool is not confined to its working directory.
