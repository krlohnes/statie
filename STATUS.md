# Status, 2026-09-11 morning

## Where the code is
Fifteen commits on `main` since the last push point (`58dff8f`), all
unpushed. The latest: `b42f23a` (one integration test binary per crate,
D18: gate from 103 binaries and about three hours to 16 binaries and 12 to
19 minutes), `9ba0690` (item 23), `3024891` (item 25). The tree is clean.

## Backlog: every item is now committed, measured, or parked with a reason
- P0: long tasks (1) built and calibrated; arms (2) measured on build 9;
  worktree (3) and `try_fixes` (4) built.
- P1: `tests` record (5), chunked `think` (6), `probe` (7), early-park think
  (8), `write_file` after read (15), shell reads (16), `edit_file` checks (17),
  failure-shaped question (18): all committed.
- P2: audit (13), grounded review (12), `think` listing (14), options mode
  (11): committed and measured. Sweep (10): measured. Context blocks (9):
  parked on the arm evidence. Budget lines (8b): still deferred.
- Found while measuring, all fixed and committed: evicted re-reads refused
  (19), `try_fixes` offered without a failure (20), refused re-read parks
  (21), `try_fixes` reading a stale session field and uncapped identical
  errors (22), detected command not recorded and error cap without a think
  (23), one test binary per crate (24), error streaks keyed on shape and a
  stalled-path streak (25).

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

## Latest numbers (README "field notes", DECISIONS D12 to D19)
- Fixer arm, build 11: 23 of 26, the best battery; `try_fixes` errors 0;
  `multi_file_feature` landed on both seeds for the first time.
- Eight original short tasks: build 8 16/16, build 10 13/16 (the regression
  the caps caused), build 11 15/16 on both the fixer and the current arm.
- Still missing on every build: `refactor_signature` (0 of 2). Item 25
  shortens how long a run spends on it.

## Running now
- `refactor_b8`: refactor_signature, trap_rounding, multi_file_feature on the
  item 25 binary, to measure steps-to-park.

## Open threads after that
- Push is yours; nothing has been pushed this session.
- `refactor_signature` is a capability miss; the harness's remaining job is
  to stop spending steps on it, which item 25 measures.
- Nine crates for 44k lines is more than the project needs; three would do.
  Not worth the churn now that the gate is minutes.
