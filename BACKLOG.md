# Backlog

Prioritised from RELATED-WORK.md and the bench results in README.md's
field notes. Each item says what to build or run, which evidence puts it
here, how it is measured, and what it depends on. "Battery" means the
bench task set, two seeds each, against the current build as baseline;
a change earns its place when it does not lower the landing rate and
moves the metric it was built for. Ordered within each tier.

## P0: measure the directive, and the best-supported thinking pattern

### 1. Long-horizon bench tasks
- **Status.** Built (d03e6c9); calibrated (D3 revised): python 2/2 landed past eviction, multi_file 0/2 early parks, refactor 0/2 (late park, think-timeout).
- **What.** Two or three tasks that take 40 to 80 steps on a clean run:
  a multi-file feature with a test suite, a refactor that touches five
  or more files, a bug whose reproduction needs reading three modules.
  Rust and Python, like the existing seven.
- **Why.** Every state-mutation question (eviction, notes, seen,
  decision) is invisible on tasks that land in twelve steps. Nothing in
  P0 item 2 can be measured without this. The context-rot papers put
  the failure at length; our battery never reaches it.
- **Measure.** Self-check passes (fixture fails, solution passes); a
  clean-build run lands at least one seed of each in the expected step
  range.
- **Depends on.** Nothing. Do first.

### 2. Harness-owned versus model-owned state, measured
- **Status.** Measured on build 9 (README ninth build; D15): landing equal across arms, harness-owned arms at two-thirds the steps and a third of the wall clock. Re-run after items 19 and 20. Switches built (c9fb618, plus `tests_field` with item 5). Arms pinned as configs (D12): `bench/configs/arm_current.json`, `arm_model_owned.json`, `arm_harness_max.json`; `bench/run_arms.sh` runs them back to back. First run of arm (b) interrupted after five tasks; the second run (`arm_current_b3`) had two runs killed by a gate restart (D8 addendum) and is re-run as `arm_current_b3r` after the chain; arms (a) and (c) run in the chain.
- **What.** Config toggles so the battery can run three arms with the
  model, budgets, and window fixed: (a) model-owned, maintainer off,
  `seen` and `decision` off, notes model-written; (b) current build;
  (c) harness-owned maximum, everything on plus the `tests` field from
  P1 item 5 once it exists. `seen` needs an off switch (it has none).
- **Why.** The 2026 memory survey names this as the unmeasured
  comparison; IterResearch, MEM1, and Memory-as-Action all say the state
  updater is the bottleneck, and they train it. We cannot train, so the
  question is how far tool-written state gets a prompted model. It is
  the prime directive as an experiment.
- **Measure.** Landing rate, median steps, peak prompt, park rate, on
  the full battery including the long tasks. Report the cross-arm table
  in the README.
- **Depends on.** Item 1.

### 3. Worktree snapshot primitive
- **Status.** Built (c571ab8).
- **What.** In the tools crate: snapshot the sandbox's working tree to a
  commit without touching the user's index or HEAD (temporary
  `GIT_INDEX_FILE`, `add -A`, `write-tree`, `commit-tree` with HEAD as
  parent), create a detached worktree at it, apply one edit, run a
  command with a shared `CARGO_TARGET_DIR`, collect the result, remove
  the worktree. `git init` on the fly for a sandbox with no repo.
- **Why.** The verification primitive under item 4 and P1 item 7. A copy
  that honours `.gitignore` for free, and candidates become commits.
- **Measure.** Tests prove HEAD, index, and working tree are byte-
  identical after a run; untracked files are in the snapshot; a
  candidate that passes in the worktree passes when its diff is applied
  to the sandbox.
- **Depends on.** Nothing. Can run in parallel with items 1 and 2.

### 4. Hypotheses and verify (`try_fixes`)
- **Status.** Built (8e21bbf). Not yet measured: needs a config with a `fixer` section (arm_harness_max.json has one).
- **What.** On the repeated-failure trigger: sample k (start at 3)
  thinking-off candidate fixes, each a concrete `edit_file`; apply each
  in its own worktree; run the test command; commit the passing one to
  the sandbox; write winner and losers (with their first error line)
  into `decision` so losers are not retried. Falls back to `think` when
  no candidate passes, with the losers in its question.
- **Why.** Strongest evidence of any pattern: Agentless (sample, filter
  by tests, vote), self-consistency, and the Mirage result that several
  short samples beat one long trace. Our manual version was 5 of 5 on
  the trap where the debate judge was 0 of 6. Reflexion for keeping the
  losers.
- **Measure.** Trap task landing (currently 0 of 2 without `think`, 2
  of 2 with it), steps to land, wall time against `think`, and no
  change on the ordinary tasks. Also on the rename stall shape, where
  candidates contained a fix 4 of 6 times.
- **Depends on.** Item 3.

## P1: tools write more state, and the next two thinking patterns

### 5. `test` writes durable state
- **Status.** Built (8e21bbf). Field note: the model tried to patch `tests` 3 times per Python run and got the harness-owned note.
- **What.** A harness-owned `tests` field: `{cmd, pass, failing:
  [names], first_error, step}` written by `after_tool` on every `test`
  call. The model reads it instead of re-reading the observation after
  eviction.
- **Why.** StateAct's chain-of-states, and our own `seen` result (build
  2, steps down, rejections gone). The dogfood logs show the model
  re-running tests to see the failure again; a field it cannot patch
  removes the reason.
- **Measure.** Count of `test` calls with no edit in between, across
  the battery; landing unchanged.
- **Depends on.** Nothing.

### 6. Bounded-window thinking for `think`
- **Status.** Built (42a5206). Measured on refactor_signature seed 1:
  128 s per think against 263 s unchunked; seeds 1 and 2 together 0 of
  4 answered (four chunks exhausted), 0 of 2 landed either way. On
  the trap: 2 of 2 landed, the one think answered in 46 s. On the
  original eight (build 8): 16 of 16 landed, 3 of 3 thinks answered
  in 22 to 97 s. Chunked is now the default; 6b built. Follow-up 6b: when the last chunk ends without an
  answer, make one more thinking-off call from the carry-over that
  must produce `ANSWER:`; a `think` should never return nothing.
- **What.** When the reasoning call reaches a chunk budget (say 6k
  tokens) or `done_reason: length`, stop, ask for a short carry-over,
  and continue in a fresh context with the question plus the
  carry-over, up to N chunks. Lets `think` run at the session's
  `num_ctx` with no model reload.
- **Why.** The Markovian Thinker: 8k chunks with carry-over matched or
  beat long chain-of-thought at the same budget. Our traces ran 8k to
  11k tokens and needed a separate 32k window and a reload each way.
- **Measure.** `think` success rate and wall time at `num_ctx` 16384
  chunked versus 32768 unchunked, on the trap.
- **Depends on.** Nothing.

### 7. Trace by execution
- **Status.** Built as the general `probe` tool (D11; patch in .claude/item7.diff, on top of item 15). Second trap task `trap_interest` built (a six-month amortisation whose interest truncates twice; the decoy is a `.max(0)` guard pinned by its own test). Committed (939d3d0).
- **What.** For an assertion failure with numbers: the model names the
  expressions to watch; the harness writes a temporary test (or
  instruments the functions on the call path) in a worktree, runs it,
  and returns the values; the model compares against the expected
  values and names the first divergence. Needs one more trap task where
  the arithmetic is the whole problem.
- **Why.** PAL and Program of Thoughts: models decompose well and
  compute badly. LDB: show block-level values, ask for verdicts, up to
  9.8 points. Our model computed `4600 / 100 * 90` as zero in every
  trace run.
- **Measure.** Diagnosis accuracy against `think` on the arithmetic
  traps, at what token cost.
- **Depends on.** Item 3; a second arithmetic trap task.

### 8. The early-park stall becomes a `think`
- **Status.** Built (8e21bbf).
- **What.** When the model sends a second consecutive non-question
  `ask_user` (the shape the continue-repeat cap parks on the third),
  and no `think` has run since the last tool call, substitute `think`
  with a question built from the state: "You had nothing to ask twice.
  Given the task, the plan, and the last tool output, what is the next
  concrete tool call and why?" Same once-per-streak rule as the
  repeated-failure trigger.
- **Why.** The park-rate study (DECISIONS.md D4): 6 of 9 parks in 96
  runs happen before model call 15, most of them this shape, with no
  test failure open, so the existing trigger never fires. Cuadron et
  al.'s "premature disengagement" is the same failure named.
- **Measure.** Park count on the battery (9 of 96 today); landing rate
  unchanged or better.
- **Depends on.** Nothing.

### 8b. Budget and recency in the harness view (deferred)
- **What.** Two lines in the harness view: tokens used of budget, and
  the working set's oldest step, per VISTA.
- **Why deferred.** No late parks in 96 runs; peak prompt at park is
  within 300 tokens of landed runs. Re-open if the long tasks show
  parks that rise with length.

### 15. `write_file` allowed after a full, recent read
- **Status.** Committed (939d3d0). First cut rejected at review: its rule ("no edits recorded") still refused the motivating case; corrected with `read_step` on `seen`.
- **What.** The whole-file rewrite guard (`write_file` refuses an
  existing file longer than a fixed line cap) consults `seen` instead:
  a rewrite is allowed when the harness has recorded a `read_file` of
  the whole file since the last edit of it, and refused with the
  current note otherwise. The guard stops being a proxy for "you would
  be writing it from memory" and checks the fact.
- **Why.** Calibration run `refactor_signature` seed 1 parked at model
  call 80 after 18 edits: the model had broken `tests/callers.rs` with
  an anchored edit, could not repair it with `edit_file`, and had
  `write_file` refused twice (62 lines, cap 40) although it had just
  printed the whole file. It then looped `cat` and empty `ask_user`
  until the cap parked it. `revert_file` was no way out: the original
  is also over the cap. This is the directive applied to a guard: the
  state knows what the model has read, so the guard should use it.
- **Measure.** Late parks on the long tasks; `write_file` rejections;
  landing on `refactor_signature`.
- **Depends on.** Item 16 for reads made through `shell`.

### 16. Shell reads become `read_file`
- **Status.** Committed (939d3d0).
- **What.** A `shell` command that is a pure read of one file (`cat`,
  `head`, `tail`, `sed -n`) is substituted with the equivalent
  `read_file` call, the way the think triggers substitute actions, with
  a note. The read then pages, lands in `seen`, and counts for the
  redundant-read and identical-skip guards.
- **Why.** Both long Python runs and the refactor run read files with
  `shell cat` (2, 7, and 8 shell calls), which bypasses `seen`, the
  paging cap, and the guards built on them; the refactor run ended in a
  `cat` loop no guard could see.
- **Measure.** Shell calls that are reads, across the battery, before
  and after; no change in landing.
- **Depends on.** Nothing.

### 17. `edit_file` stops silent corruption
- **Status.** Built (6bc03ca).
- **What.** Two checks in the edit tool. (a) An `after`/`before` insert
  whose text contains a line that already exists in the file and looks
  like a definition (an import, `fn`/`def`, `const`/`static`/`let` at
  column 0, a struct field) is refused: "line N already defines this;
  use old/new to replace it". (b) After any successful edit, if the
  file's brace, bracket, or parenthesis balance changed, the tool output
  ends with "this edit left braces unbalanced (+1 `}`)" so the model
  sees it before the next `test`.
- **Why.** Calibration runs: 37 `edit_file` calls, 9 refused (anchor
  not found, text already present, missing `path`), 28 applied. Of the
  applied ones, the insert form (23 of 37 calls) is used when the model
  means replace, leaving the old code in place: both Python runs ended
  with `pricing.py` holding a duplicate import, a duplicate `TAX_RATE`,
  and two overlapping copies of the fixed function, which the tests
  tolerated and the review skill had to send back for; the refactor run's
  eighteen edits all applied and one left `tests/callers.rs` with an
  unbalanced brace the model never repaired (the park in item 15).
  Refusals with a reason are the shape this model recovers from;
  silent success is the one it does not.
- **Measure.** Duplicate-definition count in landed sandboxes; review
  send-backs; edits per landed run; landing unchanged or better.
- **Depends on.** Nothing. Do before the state-ownership arms, or every
  arm pays the same tax.

### 18. The repeated-failure question fits the failure
- **Status.** Committed (939d3d0).
- **What.** `REPEAT_FAILURE_QUESTION` becomes one of several, picked
  from the normalised failure summary: a compile error ("read the
  first error: name the file, the line, and the one edit that fixes
  it; do not trace values"), an assertion with numbers (the current
  arithmetic trace), a panic or exception without numbers ("name the
  line that panics and the input that reaches it"), a timeout or hang.
  Each caps the expected answer shape so the trace stops.
- **Why.** DECISIONS.md D9: four thinks at 263 seconds each on a
  compile-error refactor, three of them empty after exhausting a 32k
  window, because the only question asks for an arithmetic trace.
- **Measure.** `think` reasoning length, empty rate, and repeat count
  after the think, per failure kind, on the battery.
- **Depends on.** Nothing. Do with item 6's default switch.

### 19. A read of an evicted window runs again
- **Status.** Committed (939d3d0; Milestone 27). Measured: `python_three_modules` 0/2 to 2/2 on the current arm (`arm_current_b4`, 18/26 landed against 16/24); `reread_evicted` never fired, the model read once and kept the window. Found in the `arm_current` battery: `python_three_modules` 0/2 (was 2/2), `refactor_signature` seed 2, `trap_rounding` seed 1 all parked after three refused re-reads of a file read many steps earlier.
- **What.** The redundant-read guard (`core/src/session.rs`, keyed on
  `reads_since_mutation`) refuses any repeat read since the last mutation
  and says "that window is in your working set above" whenever windows
  are on. With four windows, a read from ten steps back has been evicted,
  the claim is false, and the model, which correctly wants the content
  again, gets refused until the third-strike park. The guard must skip
  only when the earlier read's window is still rendered; otherwise the
  read runs again and the log records it as a re-read of evicted state.
- **Why.** This is the prime directive in one guard: re-reading evicted
  state is how limited context gets managed, and the harness was
  punishing it. Item 16 made it worse by turning `shell cat` into the
  same `read_file` key.
- **Measure.** `python_three_modules` and `refactor_signature` on the
  current arm before and after; count of `reread_evicted` events per run.

### 20. `try_fixes` is offered only when there is a failure to fix
- **Status.** Committed (69a93a8; Milestone 28). Measured (`arm_harness_max_b5`): 20/26 landed against 15/23; `try_fixes` errors 176 to 145, but all 145 in one run where the skill gate passed the call (the `tests` record showed a failure) and core refused it from a stale session field; item 22 finishes the job. Found in the `arm_harness_max` battery: 176 of its 195 tool errors were `try_fixes` called with no failing test ("no failing test to fix; run test first"), 113 of them in `design_fork` and 68 in `trap_interest`, the model repeating the call after each error.
- **What.** `implement` lists `try_fixes` only when the workspace's
  `tests` record shows a failure (`pass == false`) and a fixer is
  configured. A `try_fixes` call that arrives with no failing test is not
  an error: the harness substitutes `test {}` (the step the error message
  was asking for) with a note saying so, the same mechanism as the
  `think` substitutions.
- **Why.** A listed tool is an invitation; the model took it before it
  had anything to fix, and the tool error did not break the loop. Gating
  the listing on harness-owned state is the prime directive again: the
  `tests` record already knows whether there is a failure.
- **Measure.** `try_fixes` tool errors per run on the harness-max arm
  before and after; `design_fork` and `trap_interest` landing.

### 21. A second refused re-read becomes a `think`
- **Status.** Committed (69a93a8; Milestone 29); measured by the sweep batteries on `refactor_signature`. Found in `arm_current_b4`: `refactor_signature` 0/2, both seeds parked by the repeated-skip cap after three refused `read_file src/config.rs` calls whose window was still rendered.
- **What.** The harness records consecutive identical `skipped_redundant`
  steps in `HarnessState` (count and call key). When `implement` sees a
  read-only call whose key was skipped on the previous step, it
  substitutes `think` with a question that names the file and asks for
  the exact next edit (path, old text, new text) or, if the file is not
  what is needed, which other file to read. The core cap stays as the
  backstop for the third strike.
- **Why.** The model that re-reads what it can see has lost the thread,
  not the file. Refusing it twice more and parking spends the run on a
  message the model has already ignored once; the early-park and
  repeated-failure thinks exist for exactly this kind of stall, and this
  is the one shape they do not cover.
- **Measure.** `refactor_signature` on the current arm; parks by the
  repeated-skip cap across the battery.

### 22. `try_fixes` reads the harness-owned failure; identical tool errors are capped
- **Status.** Committed (a39f147; Milestone 30). Measured (`arm_harness_max_b6`): `try_fixes` errors 145 to 9, fixer invoked 5 times; the identical-error cap parked 8 runs (landing 17/26 against 20/26), see item 23. First cut rejected: it cleared the mirror on re-activation and re-ran `test` in its own test, the very loop it was meant to end. Found in `arm_harness_max_b5`: `rename_across_files` seed 2 called `try_fixes` 145 times after a failing `test`, each refused with "no failing test to fix", no park.
- **What.** Two parts. (a) `execute_try_fixes` decides whether there is a
  failure from `HarnessState.last_failure_summary` and `last_test_cmd`
  (persisted, mirrored from the session field) instead of the
  session-local `last_test_failure`, which resets on activation and can
  be `None` while the harness state says the last test failed. (b) The
  non-progress guard counts consecutive identical `tool_error` steps
  (same tool, same reason) the way it counts identical skips: the second
  identical error is reported with the streak, the third parks with the
  same `non_progress_cap` message, so no run can repeat one refused call
  145 times.
- **Why.** D14 again: the harness held two records of "did the last test
  fail" and acted on the stale one. And a tool error the model repeats
  is non-progress by any definition; only skips and continues were
  counted.
- **Measure.** `try_fixes` errors on the fixer arm; longest identical
  tool-error streak across the battery.

### 23. The detected test command is recorded; a repeated failing call becomes a `think`
- **Status.** Committed (9ba0690; Milestone 31). Measured (`arm_harness_max_b7`): 23/26 landed against 17/26, `try_fixes` errors 0, cap parks 2 against 8, short tasks 15/16. First cut let a `think` step reset the streak, so error, think, error, think could cycle; corrected so a think keeps the count and the cap bounds it. Found in `arm_harness_max_b6`: three runs refused `try_fixes` after `test {}` because the auto-detected command was never recorded; five runs parked at the third identical `edit_file` error.
- **What.** (a) When `test` runs with no `cmd`, the harness records the
  command the tool detected (the tool prints it as its first line) in
  `HarnessState.last_test_cmd` and in the `tests` record, so a failure
  after `test {}` is a failure `try_fixes` can work from. (b) The
  harness records the last tool error (tool, args, reason, count, step)
  in `HarnessState`, and `implement` substitutes `think` when the model
  re-issues the same tool call with the same arguments after it errored,
  with a question that quotes the error and asks for a different exact
  action. The core cap at three stays as the backstop.
- **Why.** The same two shapes as items 19 to 22: a harness record that
  was incomplete, and a repeat the harness answered with a refusal
  where a question would have moved the run. Eight of build 10's nine
  fixer-arm misses were these.
- **Measure.** Fixer arm: `try_fixes` refusals after `test {}`; parks by
  the identical-error cap; landing.

### 24. One integration test binary per crate
- **Status.** Done (D18): all seven crates, 16 binaries, gate 12 green in 12 minutes against 93.
- **What.** `tests/it/main.rs` per crate declaring the former files as
  modules; the same tests, one binary. Targeted runs become
  `cargo test -p <crate> --test it <module>::`.
- **Why.** The gate launched about 103 binaries for five minutes of
  test CPU; the per-binary link and Gatekeeper wait were the whole cost.
- **Measure.** Gate wall clock and `Running` lines before and after.

### 25. Repeat guards key on the shape of an error, not its text
- **Status.** Committed (3024891; Milestone 32). Measured (`refactor_b8`): the stall guard fired four times per seed but `refactor_signature` still ran 132 steps to the 900 s timeout on both, with 25 to 30 applied edits and 18 to 21 test runs cycling through 8 to 10 distinct compile errors. Every guard saw progress; the test saw none. Item 27 is the cap for that shape. First cut rejected: its error trigger fired on any call of the tool after one error, which would have blocked the corrected edit; now identical call or repeated shape. Found in `arm_harness_max_b7`: `refactor_signature` ran 116 and 153 steps before the mixed-shape cap parked it.
- **What.** The identical-error streak keys on (tool, reason text), so
  nine `edit_file` failures in a row with different line numbers in the
  reason count as nine different errors, and a run that alternates a
  refused read with a failed edit breaks both streaks every step. Key
  the error streak on (tool, reason with digits and paths stripped),
  and count a refused read followed by a failed edit on the same path
  as one streak, so the second-strike `think` and the third-strike park
  arrive at three, not at a hundred.
- **Why.** `refactor_signature` is the one task no build has landed;
  the model cannot do the refactor, and the harness's job is then to
  stop spending steps on it. Seed 1 alternated skip, error, skip, error
  for forty steps; seed 2 issued nine consecutive failing edits.
- **Measure.** Steps to park on `refactor_signature`; no change on the
  landing rows.

### 27. A run without a green test in N test runs stops
- **Status.** Committed (4307eb8; Milestone 34). Measured next by `refactor_b10` on the three hard tasks against `refactor_b8`. Evidence across 79 landed runs (builds 10 and 11): the most failing `test` runs before the first pass was 13 (one `multi_file_feature` seed), the 90th percentile 6, the median 2. The five `refactor_signature` misses ran 29, 24, 22, 14, and 11 failing tests.
- **What.** The harness counts failing `test` runs since the last pass in
  this activation (`HarnessState.fails_since_green`). At 12, `implement`
  substitutes `think` with a question that says how many tests have
  failed and asks whether the plan itself is wrong and what the smallest
  change that could pass is. At 16 the core parks through
  `park_non_progress_cap` with the shape "no green in 16 test runs". A
  pass resets the count.
- **Why.** The refactor runs cycle through distinct compile errors,
  each edit applies, no guard keyed on repeats can see it, and the run
  spends its whole budget. The cap sits above every landed run in the
  record, so it costs nothing on the evidence and ends the worst
  misses at half their length.
- **Measure.** `refactor_signature` steps; landing on the full battery
  unchanged.

## P2: worth doing, weaker or narrower evidence

### 9. Typed context blocks with keep or archive
- **Status.** Parked (D15): the model-owned arm landed the same 16/24 as the current arm at 20 median steps against 13. No signal yet that model-managed context helps; revisit if the re-run after items 19 and 20 changes that.
- **What.** Working-set entries become addressable blocks the model can
  pin or drop, archived blocks recoverable through `recall`; harness
  eviction remains the default.
- **Why.** VISTA. It is also the cleanest test of "does the model manage
  its state better when it can see it", the flip side of item 2.
- **Measure.** Same arms as item 2, plus this one.
- **Depends on.** Items 1, 2, 8.

### 10. Maintainer sweep
- **Status.** Measured (`sweep_*_b5` against `arm_current_b4`, five tasks, two seeds; README "Maintainer sweep"; D17): budget 200 landed 6/10, 400 5/10, 800 5/10, off 4/10. Within noise on landing; off costs a third more steps; the wall-clock and tool-error spread is uncapped loops, not the maintainer. Budget stays at 400.
- **What.** Maintainer on versus off, and the notes budget at 200, 400,
  800, on the long tasks.
- **Why.** The trained systems say the updater matters most; the
  maintainer is our prompted updater and has never been swept.
- **Depends on.** Item 1.

### 11. Options and verdict at plan time
- **Status.** Committed (939d3d0; Milestone 26, `think {mode: "options"}`); `design_fork` bench task added. Measured on `arm_current_b4`: 1/2 landed, and neither seed called `think` with `mode: "options"` despite the plan rule; the mode is reachable only by harness trigger now that `think` is unlisted (D16), so it needs a plan-time trigger before it can be measured again. Measurement: `design_fork` with and without the plan-rules sentence.
- **What.** A `think` mode for design forks: list approaches with
  trade-offs, pick one, store the verdict; the plan is written from it.
- **Why.** Tree of Thoughts where no tool can judge; the deliberation
  paper says structure helps on non-routine decisions at 62 times the
  tokens and hurts on routine ones. Rare by construction.
- **Measure.** Needs a task with a real fork; none in the battery.

### 12. Grounded review critique
- **Status.** Committed (939d3d0; Milestone 24, `guards.grounded_findings`). Measured (`ungrounded_review_b4` against `arm_current_b4`, 26 runs each): 21/26 against 18/26, but the review sent work back in only 2 of 52 runs and dropped no finding in either arm, so the landing gap is run noise and the false send-back rate is unmeasurable on this battery; the review approves almost everything it sees. Measurement: false send-backs on the battery, current arm against the switch off.
- **What.** Review findings must cite a line and, where testable, a
  probe the harness runs; a finding with neither is dropped with a note.
- **Why.** Adversarial framing showed nothing with this model; grounding
  is what made the judge trustworthy in the propose-then-verify runs.
- **Measure.** False send-backs on the battery.

### 13. Automated paired-failure audit
- **Status.** Committed (939d3d0; Milestone 23). Window list not compared (derived state, never persisted).
- **What.** `statie audit` compares a landed and a failed run of the
  same task and reports where their contexts first diverge.
- **Why.** ACON optimises compression guidelines from exactly such pairs
  and reports 26 to 54% less context; we do the same by hand in the
  field notes.
- **Depends on.** Two seeds disagreeing on a task, which the long tasks
  will provide.

### 14. Does listing `think` cost anything
- **Status.** Measured (`think_unlisted_b4` against `arm_current_b4`, eight short tasks, two seeds): listed 13/16, unlisted 12/16; unlisted had 0 rejections against 2, 10 tool errors against 19, mean peak prompt 6423 against 6823, median 15 steps against 17. The eight `think` calls in the unlisted battery were all harness substitutions. Listing costs about 400 prompt tokens and a few tool errors and buys nothing measurable; `thinker.listed` now defaults to false (D16): the plan skill did not use options mode on either `design_fork` run. Measurement: current arm with `listed: false` on the eight short tasks.
- **What.** Battery with `think` present in the tool list but never
  triggered, against absent. The model has never called it unprompted
  in 48 runs.
- **Why.** Build 5 showed higher rejections and tool errors with the
  tool listed; build 6 did not. Cheap to settle.

### 26. Situational rules: the rules text follows the last action
- **What.** The coding skills' rules block is rendered whole on every
  step. Replace it with a small static procedure graph (nodes: read,
  edit, test pass, test fail, think, review) whose edges carry
  condition, guidance, and pitfalls; each step renders only the 2-hop
  neighbourhood of the last action, with the full block as the fallback
  when nothing matches. No extra model call.
- **Why.** Procedural Graphs (RELATED-WORK.md): localised guidance beat
  full-graph injection by 9 points and 71% fewer tokens on frontier
  models; full injection hurt. Our rules block is a full injection.
- **Measure.** Mean and peak prompt tokens, landing on the full battery,
  the eight-short-task row.
- **Status.** Committed (896fb14; Milestone 33). Measured (`situational_b9` against `arm_harness_max_b9`, same binary): 22 of 26 both ways, three more median steps and no prompt saving at the peak with the switch on. Stays off (D20).
- **Depends on.** Nothing; P2 because the evidence is from other models.

## Not doing

- Multi-agent debate as a thinking pattern: 0 of 6 here, and the
  literature only supports it with asymmetric evidence.
- Training the state updater: out of scope for a prompted local model.
- Longer native thinking as a lever: no budget knob on this model, and
  the Mirage result says length past a point costs accuracy.
