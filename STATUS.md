# Status, 2026-09-12 morning

## Where the code is
25 commits on `main` since the last push point (`58dff8f`), all
unpushed. The latest code commits: `896fb14` (item 26, situational
rules behind `guards.situational_rules`) and `4307eb8` (item 27, the
no-green cap). The tree is clean; no worktrees or agent branches remain.
The gate is 16 test binaries in 12 to 19 minutes (D18).

## Backlog: through
Every item is committed and measured, or parked with a written reason.
- P0 and P1: all committed (items 1 to 8, 15 to 18).
- P2: audit (13), grounded review (12), `think` listing (14), options
  mode (11), maintainer sweep (10), situational rules (26): committed and
  measured. Context blocks (9): parked on the arm evidence (D15). Budget
  lines (8b): deferred.
- Found while measuring, all fixed, committed, measured: 19 to 25, 27.

## What the last three builds said (README field notes, D20 to D22)
Every paired comparison this session ran at two seeds a task. That
picks the cheaper default when the arms are close; it does not show an
effect (D22). Read the landing gaps below with that in mind.
- Build 13, same binary, same seeds: situational rules off 22 of 26 in 72
  minutes; on 22 of 26 in 92 minutes, no prompt saving at the peak. The
  switch stays off (D20). The paper's localisation gain does not carry to
  a model whose misses are stalls.
- Build 14, the no-green cap on the three hard tasks: both
  `refactor_signature` seeds parked at exactly the sixteenth failing test
  after four questions, at 1265 s and 1387 s of 1500 s. Correct, and a
  small saving: a failing test cycle costs 80 s on that task. Kept; no
  more batteries aimed at that task alone (D21).
- `multi_file_feature` is 4 of 6 across the last three batteries of the
  same code; `trap_rounding` 3 of 6 then 2 of 2. Both are variance at
  two seeds, not a trend.
- Best full battery remains build 11's fixer arm at 23 of 26; builds 13
  and 14 carry the same code plus two switched-off or never-firing
  additions, and landed 22 of 26.

## Running now
Nothing. No cargo, bench, or agent process is running.

## Open threads
- Push is yours; nothing has been pushed this session.
- `refactor_signature` is a capability miss for a 26B model with a four-window,
  6k-token working set in a 16k context; the harness now names it (`no green in 16 test runs`)
  instead of timing out on it.
- 8b (budget lines in the harness view) is the one deferred item with a
  spec; it needs a hypothesis about what the model would do with the
  number before it earns a battery.
- Nine crates for 44k lines is more than the project needs; three would
  do. Not worth the churn now that the gate is minutes.
