# Status, 2026-09-11 morning

## Where the code is
Nine commits on `main` since the last push point (`58dff8f`), all unpushed:
`939d3d0` (third batch: items 7, 11, 12, 13, 14, 15, 16, 18, 19), `69a93a8`
(items 20, 21, `thinker.listed` default false), `a39f147` (item 22), plus
bench configs and docs. Item 23 (detected test command recorded; a repeated
failing call becomes a `think`; a `think` no longer resets the identical-error
or identical-skip streak) is applied to the tree and in gate 11, which has
passed fmt and clippy and is in its test phase. It commits on green.

## Backlog: every item is now committed, measured, or parked with a reason
- P0: long tasks (1) built and calibrated; arms (2) measured on build 9;
  worktree (3) and `try_fixes` (4) built.
- P1: `tests` record (5), chunked `think` (6), `probe` (7), early-park think
  (8), `write_file` after read (15), shell reads (16), `edit_file` checks (17),
  failure-shaped question (18): all committed.
- P2: audit (13), grounded review (12), `think` listing (14), options mode
  (11): committed and measured. Sweep (10): measured. Context blocks (9):
  parked on the arm evidence. Budget lines (8b): still deferred.
- Found while measuring, all fixed: evicted re-reads refused (19),
  `try_fixes` offered without a failure (20), refused re-read parks (21),
  `try_fixes` reading a stale session field and uncapped identical errors
  (22), detected command not recorded and error cap without a think (23).

## What the measurements say (README "field notes", DECISIONS D12 to D17)
- Arms: model-owned and current both landed 16 of 24; current took 13 median
  steps against 20 and a third of the wall clock. Harness-owned state stays.
- Every miss shared by the arms was a harness defect, not a state-ownership
  effect: the guards claimed the model could see things it could not (D14).
- Current arm after item 19: python long task back to 2 of 2, 18 of 26 overall.
- Fixer arm: 15/23 on build 9, 20/26 on build 10 (best so far), 17/26 on
  build 10 with the error cap (the cap parked 8 runs at the third strike; item
  23 puts a `think` at the second). Build 11 runs next.
- `think` listing: no landing effect, small prompt cost; unlisted by default.
- Grounded review: nothing to act on, the review sends back almost nothing.
- Maintainer budget 200/400/800/off: within one run; off costs a third more
  steps. Stays at 400.

## Running now
- Gate 11 (item 23). Then commit and remove the last agent worktrees.
- `arm_harness_max_b7`, the fixer arm on the item 23 binary, starts on its
  own when the gate has rebuilt the binary. Its numbers close items 20, 22, 23.

## Open threads after that
- `refactor_signature` and `multi_file_feature` still miss on every arm
  (0 of 2 each); the traces point at the model re-reading and re-editing the
  same file, now answered by `think` twice before a park.
- Push is yours; nothing has been pushed this session.
- D7 still applies: a gate is about three hours, dominated by macOS vetting
  each fresh test binary.
