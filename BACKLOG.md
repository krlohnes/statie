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
- **Status.** Switches built (c9fb618, plus `tests_field` with item 5); configs in `bench/configs/`. Battery not yet run.
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
- **Status.** Built (proposer, runner, session, trigger); in the second gate.
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
- **Status.** Built; in the second gate. Field note: the model tried to patch `tests` 3 times per Python run and got the harness-owned note.
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
- **Status.** Built as the general `probe` tool (D11; patch in .claude/item7.diff, on top of item 15). Second trap task `trap_interest` built (a six-month amortisation whose interest truncates twice; the decoy is a `.max(0)` guard pinned by its own test). Measurement after the third gate.
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
- **Status.** Built in a worktree, merged; in the second gate.
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
- **Status.** Built (patch in .claude/item15.diff, on top of item 16). First cut rejected at review: its rule ("no edits recorded") still refused the motivating case; corrected with `read_step` on `seen`.
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
- **Status.** Built in a worktree (patch in .claude/item16.diff, on top of item 18); applies after the second batch commits.
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
- **Status.** Built; in the second gate.
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
- **Status.** Built in a worktree (patch in .claude/item18.diff); applies after the second batch commits.
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

## P2: worth doing, weaker or narrower evidence

### 9. Typed context blocks with keep or archive
- **What.** Working-set entries become addressable blocks the model can
  pin or drop, archived blocks recoverable through `recall`; harness
  eviction remains the default.
- **Why.** VISTA. It is also the cleanest test of "does the model manage
  its state better when it can see it", the flip side of item 2.
- **Measure.** Same arms as item 2, plus this one.
- **Depends on.** Items 1, 2, 8.

### 10. Maintainer sweep
- **Status.** First data point (maintainer off, thinker on, old
  unchunked binary): Python long task 1 of 2 (14 steps and 45 s on the
  landing seed, against 29 steps with the maintainer; the other seed
  parked in the early-ask shape at call 9); trap 1 of 2 (the miss timed
  out on three consecutive 284-second thinks that exhausted the 32k
  window, two of them the model's own calls). Too noisy to call; the
  sweep proper waits for the gated build with chunked thinking.
- **What.** Maintainer on versus off, and the notes budget at 200, 400,
  800, on the long tasks.
- **Why.** The trained systems say the updater matters most; the
  maintainer is our prompted updater and has never been swept.
- **Depends on.** Item 1.

### 11. Options and verdict at plan time
- **What.** A `think` mode for design forks: list approaches with
  trade-offs, pick one, store the verdict; the plan is written from it.
- **Why.** Tree of Thoughts where no tool can judge; the deliberation
  paper says structure helps on non-routine decisions at 62 times the
  tokens and hurts on routine ones. Rare by construction.
- **Measure.** Needs a task with a real fork; none in the battery.

### 12. Grounded review critique
- **What.** Review findings must cite a line and, where testable, a
  probe the harness runs; a finding with neither is dropped with a note.
- **Why.** Adversarial framing showed nothing with this model; grounding
  is what made the judge trustworthy in the propose-then-verify runs.
- **Measure.** False send-backs on the battery.

### 13. Automated paired-failure audit
- **What.** `statie audit` compares a landed and a failed run of the
  same task and reports where their contexts first diverge.
- **Why.** ACON optimises compression guidelines from exactly such pairs
  and reports 26 to 54% less context; we do the same by hand in the
  field notes.
- **Depends on.** Two seeds disagreeing on a task, which the long tasks
  will provide.

### 14. Does listing `think` cost anything
- **What.** Battery with `think` present in the tool list but never
  triggered, against absent. The model has never called it unprompted
  in 48 runs.
- **Why.** Build 5 showed higher rejections and tool errors with the
  tool listed; build 6 did not. Cheap to settle.

## Not doing

- Multi-agent debate as a thinking pattern: 0 of 6 here, and the
  literature only supports it with asymmetric evidence.
- Training the state updater: out of scope for a prompted local model.
- Longer native thinking as a lever: no budget knob on this model, and
  the Mirage result says length past a point costs accuracy.
