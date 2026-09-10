# Backlog

Prioritised from RELATED-WORK.md and the bench results in README.md's
field notes. Each item says what to build or run, which evidence puts it
here, how it is measured, and what it depends on. "Battery" means the
bench task set, two seeds each, against the current build as baseline;
a change earns its place when it does not lower the landing rate and
moves the metric it was built for. Ordered within each tier.

## P0: measure the directive, and the best-supported thinking pattern

### 1. Long-horizon bench tasks
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

### 8. Budget and recency in the harness view, and the park-rate study
- **What.** Two lines in the harness view: tokens used of budget, and
  the working set's oldest step. Separately, an analysis over existing
  logs: `ask_user` park rate against step count and prompt size.
- **Why.** VISTA's dashboard lifted a small model from 22.7 to 50.7%
  with gains growing under context pressure. The context-rot follow-up
  says the failure at length is premature termination, which is our
  park.
- **Measure.** Park rate before and after on the long tasks; the log
  study is a README field note.
- **Depends on.** Item 1 for the long tasks; the log study needs
  nothing.

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
