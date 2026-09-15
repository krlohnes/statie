# Decisions

A running log of decisions made while driving BACKLOG.md, newest last.
Each entry: the decision, the evidence or reason, what it replaced or
ruled out, and how to tell later whether it was right. ARCHITECTURE.md
holds the resulting design; this file holds why.

## D1. Commit per completed backlog item
- **Decision.** While driving the backlog autonomously, commit each
  backlog item when its gate is green and its measurement is recorded,
  one commit per item (plus a docs commit when the docs change
  separately). Push is left to the user.
- **Why.** The standing rule was "commit only when asked"; an
  autonomous multi-item drive would otherwise leave hours of work
  uncommitted. Per-item commits keep the history reviewable and
  bisectable.
- **Check.** The user can revert any single item without touching the
  others.

## D2. Order of P0: long tasks and the worktree primitive first, in parallel
- **Decision.** Build the long-horizon bench tasks (backlog 1) and the
  worktree snapshot primitive (backlog 3) at the same time; then the
  state-ownership experiment (backlog 2) and `try_fixes` (backlog 4).
- **Why.** 1 and 3 share no files and nothing else can be measured or
  built without them. 2 needs the long tasks to show anything; 4 needs
  the worktree.
- **Check.** Both land without a merge conflict and 2 and 4 start the
  same day.

## D3. Long tasks are calibrated by running, and "long" means evicted
- **Decision (revised).** A long-horizon task is accepted when its
  landed runs make more tool calls than the working set holds (more
  than `working_set.max_windows` windows, so real eviction happens) and
  the model then edits or tests against files it read before the
  eviction. Step count is reported but is not the criterion: the first
  version of this decision asked for 40 to 80 steps, and the user
  rightly asked why more steps would be a target. They are not; the
  target is a run long enough that state management is exercised,
  because every comparison in backlog item 2 is invisible until
  something has been evicted. A task that fails both seeds is kept only
  when the failure is a harness stall rather than an impossible task.
- **Evidence.** `python_three_modules` seed 1 landed in 29 steps with
  12 tool calls against 4 windows, editing `pricing.py` from memory
  after its read had been evicted: long enough. `multi_file_feature`
  parked both seeds at call 7 and 8 in the early-park shape (D4),
  before any eviction: a stall, so the task stays and the early-park
  trigger is what gets measured on it.
- **Check.** The accepted tasks' tool-call counts and eviction counts
  in the README table, beside the step counts.

## D4. The dashboard lines wait; the early-park shape moves up
- **Decision.** Backlog item 8's harness-view budget and recency lines
  are deferred until a long-horizon task shows a late park. A new item
  takes their place in P1: convert the early empty-`ask_user` stall into
  a `think` call before the continue-repeat cap parks the run.
- **Evidence.** Park-rate study over the 96 runs of builds 5 to 7
  (bench outcome lines): 9 parks, 6 of them before model call 15 and
  the median at call 11; peak prompt at park 6327 tokens against 6033
  for landed runs. The context-rot literature's premature termination
  rises with length; ours does not, because the tasks are short. What
  parks us is the model sending an empty `ask_user` three times in a
  row (continue-repeat cap) or re-reading one file until the identical
  skip cap fires, with no test failure open, so the repeated-failure
  trigger never sees it.
- **Ruled out.** Treating the park rate as a context-pressure signal on
  the current battery.
- **Check.** Re-run the study once the long tasks exist; if late parks
  appear, item 8 comes back.

## D5. Agents write; only the arbiter runs cargo
- **Decision.** Sub-agents no longer run workspace-wide `cargo test`,
  `cargo clippy`, or `cargo fmt --check`. Their brief ends at `cargo
  build` of the crates they touched plus one foreground `cargo test -p
  <crate> --test <their new test file>`. The one workspace gate (test,
  clippy, fmt) runs once per batch, by me, after every agent in the
  batch has stopped. Agents whose units touch the same file work in
  their own git worktree and I merge.
- **Evidence.** Four agents in parallel on the first P0 batch each ran
  the workspace tests, one with a fresh `CARGO_TARGET_DIR`: 35 cargo
  and rustc processes on one machine, every run serialised on the build
  lock, none finishing in 45 minutes, and each agent burning its budget
  polling its own stalled run. The single gate over the same four units
  takes 20 to 30 minutes on an idle machine.
- **Ruled out.** Per-agent isolated target directories (a from-scratch
  build per agent, worse), and staggering the agents (loses the
  parallelism that makes the pattern worth using).
- **Check.** Wall time per batch, and no agent report that mentions
  waiting on cargo.

## D6. Battery configs and the runner live in the repo
- **Decision.** The battery's configs are files under `bench/configs/`
  (`base.json`, the dogfood config; `thinker.json`, the same plus a
  `thinker` section; one file per experiment arm) and `bench/run.sh`
  starts a detached run into the gitignored `bench-runs/<label>/`.
- **Evidence.** The session scratchpad was emptied by the system's
  temp-file cleaner at midnight: every config, run script, and battery
  artifact from builds 4 to 7 was gone, and the configs had to be
  rebuilt from the transcript and the dogfood sandbox. The README
  tables survive because they were written from the artifacts at once.
- **Ruled out.** Keeping run artifacts in the repo (a battery is
  hundreds of megabytes of sandboxes and logs).
- **Check.** A battery from a previous day can be re-run from its
  config file alone.

## D7. The gate's cost is macOS, not cargo
- **Observation, not a decision.** Every freshly linked test binary
  waits on `syspolicyd` (Gatekeeper's notarization check, which goes
  to the network and logs "Error checking with notarization daemon")
  before its first instruction runs: one to two minutes per binary on
  a quiet machine, sixteen minutes for one binary while three builds
  ran at once. The workspace gate has 85 test binaries, so it takes
  about three hours wall-clock while the tests themselves take under
  five minutes of CPU. The fix is outside this repo: granting the
  terminal the "Developer Tools" privacy permission (or `spctl
  developer-mode enable-terminal`) exempts it from the check. Until
  then, batches are gated together, never one unit at a time. The
  early-park agent, building in its own worktree at the same time as
  two builds in the main checkout, also saw the machine run out of
  memory (under a gigabyte free of 32) with rustc blocked in dyld
  loading a proc-macro dylib for twenty minutes at a stretch, so
  concurrent from-scratch builds are limited to one worktree at a time.
  (This entry was lost once to a sub-agent's `git stash` conflict and
  restored; agents are told not to touch git, and D5 now says why.)

## D8. One battery at a time on the model host
- **Decision.** The model host is a single local GPU (one 4090). Only
  one battery runs at a time, tasks sequential within it, and nothing
  else in the drive calls the model: sub-agents build and run local
  tests only, and every measurement (calibration, chunked thinking,
  the state-ownership arms) is queued behind the previous one.
- **Why.** Concurrent requests would share the KV cache and reload the
  model, corrupting wall-time and peak-prompt numbers and, as the
  unpinned `num_ctx` experiments showed, crashing the server.
- **Check.** Every README battery row was produced by a run that had
  the server to itself.

- **Addendum (build 9).** Killing cargo by name (`pkill -f "cargo test"`)
  while a battery runs also kills the sandbox `cargo test` of whichever
  bench run is in flight: two runs of the first `arm_current` battery
  (`rename_across_files` seed 2, `trap_rounding` seed 2) ended as
  `crashed` at exactly the two moments a gate was restarted. A gate is
  stopped by killing its own script and children by pid, never cargo by
  name, and a battery whose runs were killed is re-run whole.

## D9. Unbounded thinking is the wall-clock cost; the question must fit the failure
- **Decision.** Chunked thinking becomes the default for the `think`
  tool as soon as its measurement shows it answers at least as often
  as the unchunked call, and the repeated-failure question is shaped by
  the kind of failure (compile error, assertion, panic, timeout)
  rather than one arithmetic-trace question for all.
- **Evidence.** Calibration run `refactor_signature` seed 2 timed out
  at 1500 seconds with 63 model calls. Four `think` calls took 263
  seconds each, 1051 of the run's 1318 seconds; three of the four
  exhausted the 32768-token window before answering (nothing came
  back), and the one that answered used 29k characters of reasoning.
  The question was the arithmetic-trace one written for the rounding
  trap, asked about a Rust compile error in a signature refactor; the
  model traced arithmetic that did not exist. The rest of the run's
  steps averaged 4 to 7 seconds. Every other Rust-task step, including
  `cargo test`, was cheap.
- **Ruled out.** Raising `thinker.num_ctx` again (a 65k window would
  reload the model and still not bound the trace) and raising the task
  timeout to hide the cost.
- **Result so far.** Chunked on the same task, seed 1: three `think`
  calls at 128 seconds each (half the unchunked cost, no reload), and
  all three empty: "exhausted 4 chunks of 6000 tokens without
  answering". Chunking bounds the cost; it does not make a wrong
  question answerable. So two follow-ups: the chunk loop ends with a
  forced, thinking-off answer from the last carry-over so a `think`
  never returns nothing (item 6b), and the question is shaped by the
  failure (item 18).
- **Result, trap.** `trap_rounding` chunked: 2 of 2 landed; the one
  `think` that fired answered in 46 seconds (14k characters of
  reasoning, one chunk) against 100 to 250 seconds unchunked with the
  reload. Chunked is the default from here (`thinker.chunked: true`);
  the empty-answer problem is the question's, handled by 6b and 18.
- **Check.** Empty-answer rate on the battery after 6b and 18; no
  loss on the trap.

## D10. Units that share files commit together
- **Decision.** D1 (one commit per backlog item) holds when a unit's
  files are its own. When concurrent units edit the same files
  (`core/src/session.rs`, `skills/src/coding/mod.rs`, the
  `HarnessState` literals), they commit as one batch commit whose
  message lists the items, with the separable units (the edit_file
  checks, the forced answer) still committed on their own.
- **Why.** Splitting a shared file's hunks by item after the fact is
  slow and error-prone, and the second gate covers the batch as a
  whole; per-item bisectability is worth less than a commit that
  matches what was gated.
- **Check.** Every commit compiles and passes the gate on its own.

## D11. Trace by execution is a general `probe`, not harness instrumentation
- **Decision.** Backlog item 7 is built as a `probe` tool: the model
  writes a short snippet (a Rust test body or a Python module), the
  harness runs it against the real code in an isolated worktree and
  returns what it printed. The harness does not try to instrument the
  call path itself or pick expressions from the model's hints.
- **Why.** PAL and LDB's result is that the model decomposes well and
  computes badly; the cheapest way to honour it is to let the model
  decompose into prints and let the machine compute. Harness-generated
  instrumentation is language- and framework-specific and would need
  the test's setup reproduced; a snippet needs neither. The worktree
  primitive already isolates it, and `try_fixes` showed the shape.
- **Ruled out.** A `trace {exprs}` tool that rewrites the failing test
  with prints (fragile across test frameworks).
- **Check.** Diagnosis on the arithmetic traps with `probe` available
  against `think` alone, and how often the model reaches for it
  unprompted.

## D12. The three state-ownership arms are pinned configs, run back to back
- **Decision.** Backlog item 2's arms are files under `bench/configs/`:
  `arm_model_owned.json` (maintainer off; `seen`, `decision`, `tests`
  fields off; thinker on), `arm_current.json` (the thinker config: every
  field on, maintainer on, no fixer), and `arm_harness_max.json` (the
  current arm plus a `fixer` section, so `try_fixes` and `probe` are
  available). `bench/run_arms.sh <label>` runs the three on the full
  battery in that order, one at a time (D8), and appends to
  `bench-runs/arms_<label>.done` as each finishes.
- **Why.** The arms are the experiment the prime directive asks for; a
  pinned file per arm keeps the comparison reproducible after the
  binary changes, and a chained runner keeps the GPU busy across a
  gate without anyone watching. The fixer belongs to the maximum arm,
  not the current one, so the current arm measures the batch-2 and
  batch-3 items alone and the maximum arm measures what `try_fixes`
  adds on top.
- **Interrupted.** The first run of arm (b) was stopped after five
  tasks at the user's request (laptop needed for a call), as was the
  third gate. Both restart from scratch; partial arm output is not
  reported.
- **Check.** The cross-arm table in the README: landing rate, median
  steps, peak prompt, park rate, per arm, on all eleven tasks plus
  `trap_interest`.

## D13. Tests import shared strings; the gate covers the whole tree it runs on
- **Decision.** A test that checks a harness-authored string imports the
  constant (re-exported from the crate) instead of carrying a copy. The
  gate runs on whatever the tree holds when it starts, so units accepted
  while a gate is running are folded into the next gate rather than
  copied in mid-run: gate 3 was killed and replaced by gate 4 over batch
  3 plus items 12, 13, and 14.
- **Why.** `think_trigger.rs` carried its own copy of
  `REPEAT_FAILURE_QUESTION`; item 7 appended a sentence to the constant
  in a different worktree and the test broke on the merged tree without
  either agent seeing it. The item 14 agent found it by running the base
  tree's test. Copying accepted units into the tree under a running
  gate produced a gate that tested a mixture of trees, which is worth
  nothing as evidence.
- **Check.** Every gate log names the tree it ran on (the commit plus the
  patches applied), and the commit that follows matches it.

## D14. Guards defer to what the model can actually see
- **Decision.** A guard that refuses an action because "you already have
  that" must check the rendered prompt, not a bookkeeping map. The
  redundant-read guard now skips a repeat read only when the earlier
  window is still in the working set; a read of an evicted window runs
  again and is logged as `reread_evicted` (backlog item 19). The same
  rule applies to any future guard that reasons about the model's
  context: the working set's own record of what it rendered is the
  source of truth.
- **Why.** The first `arm_current` battery parked `python_three_modules`
  twice, `refactor_signature` once, and `trap_rounding` once, each after
  three refused re-reads of a file the model had read many steps before.
  The guard's message claimed the window was above; with four windows it
  had been evicted. Re-reading evicted state is exactly the behaviour the
  prime directive wants, and the harness was converting it into a park.
  Item 16 (shell reads become `read_file`) widened the blast radius by
  giving `cat file` the same key. This is the arm experiment's first
  real result: harness-owned state is only as good as the harness's
  honesty about what it has shown.
- **Check.** `python_three_modules` returns to 2/2 on the current arm
  and `reread_evicted` counts stay small (a run that re-reads the same
  file five times is a different problem).

## D15. The arms say: keep harness-owned state, and fix the harness's honesty first
- **Decision.** The prime directive stands as the default: harness-owned
  state (maintainer, `seen`, `decision`, `tests`) is kept on. The next
  work is not more state but making the harness truthful about the state
  it shows (D14) and about the tools it offers (backlog item 20), then
  re-running the arms.
- **Why.** Ninth build, twelve tasks, two seeds: model-owned and current
  arms both landed 16 of 24, but model-owned took a median of 20 steps
  against 13 and 94 minutes against 31. The misses in both arms were the
  same tasks and the same shape: refused re-reads of evicted windows, a
  harness defect that no amount of model-owned note-taking routes
  around. The fixer arm added nothing to landing and 176 tool errors
  from `try_fixes` being offered before a failure existed. So the
  evidence is that state ownership is not the bottleneck; the harness's
  claims about context are.
- **Ruled out for now.** Backlog item 9 (model-pinned context blocks)
  stays parked: with landing equal and steps worse in the model-owned
  arm, there is no signal that the model manages its context better
  when given the controls.
- **Check.** The re-run after items 19 and 20: `python_three_modules`
  and `refactor_signature` on the current arm, and `try_fixes` errors
  on the fixer arm.

## D16. `think` stays listed only because the plan skill's options mode needs it
- **Decision.** `thinker.listed` keeps its default of true. The
  measurement (backlog item 14) could not separate the two on landing
  (13 of 16 listed, 12 of 16 unlisted: one run apart at two seeds a
  task, which is noise) and found a small cost to listing: about 400 prompt tokens at peak and a few more
  tool errors on sixteen runs. Every useful `think` so far has been a
  harness substitution. Listing survives because `think {mode:
  "options"}` (item 11) is a model-initiated call at plan time; if the
  `design_fork` measurement shows the model never uses it, the default
  flips to false and `think` becomes a harness-only tool.
- **Why.** Sixty-four runs across builds 5 to 9 without one unprompted
  `think` that changed an outcome is the evidence; the triggers are
  where the value has been (D9). Keeping a tool the model does not use
  in the list is a cost with no return except the options mode, whose
  own measurement is pending.
- **Check.** `design_fork` on the current arm: does the plan skill call
  `think` with `mode: "options"` at all, and does the verdict survive
  into the plan.
- **Applied (build 9).** `design_fork` on the current arm: neither seed
  called `think` with `mode: "options"`. Default flipped to false;
  `think` is now harness-triggered only. Options mode stays in the code
  behind a future plan-time trigger.
- **Addendum (gate 9).** A timing test (`run_in_timeout_kills_process_and_reports_promptly`)
  failed once under a gate running alongside a bench battery and passed
  alone. Its margins (2 s outer limit on a 1 s timeout, one 200 ms wait
  for the reap) measured scheduler latency, not the behaviour; they are
  now 10 s and a 3 s poll, with the same assertions. A test that fails
  only under load is fixed by asserting the behaviour with margins the
  machine can meet, never by running it twice.
- **Second instance (build 10).** `try_fixes` refused 145 calls in one
  run because its precondition read a session-local copy of "the last
  test failed" that resets on activation, while the harness-owned
  mirror said otherwise. The precondition now reads the harness record
  (Milestone 30). Rule restated: when the harness owns a fact, every
  guard reads the harness's copy; session-local bookkeeping is for
  counters, never for decisions the model sees.

## D17. The maintainer stays on at 400; the sweep measures loops, not notes
- **Decision.** `maintainer.budget` stays at 400 and the maintainer stays
  on. No further sweep of the budget until the guards stop the loops
  that dominate the wall clock.
- **Why.** On the five long and trap tasks, budgets 200, 400, and 800
  landed 6, 5, and 5 of 10 and maintainer-off landed 4 of 10: one run
  apart, which two seeds cannot separate. Off costs a third more steps
  (median 22 against 16), the same shape the model-owned arm showed
  (D15): the model spends steps rewriting what the maintainer would
  have kept. The 117 and 95 minute rows are single runs looping on a
  refused `edit_file`, which item 22's identical-error cap now ends at
  three; the sweep would need re-running to say anything about the
  budget itself, and the landing rows say it would not change the
  decision.
- **Check.** If a later battery shows the maintainer's notes crowding
  out the working set at peak (mean peak prompt rises with the budget,
  which the 800 row hints at: 8627 against 7857), revisit 200.

- **Addendum (build 23, the fixer arm with the maintainer off).** Same
  binary, same tasks and seeds, `bench/configs/arm_harness_max_nomaint.json`:
  13 of 16 against 16 of 16, 68 s a run against 113, median step gap
  2.24 s against 3.54. The maintainer's call costs 1.3 s a step, two
  fifths of the wall clock, and the three lost runs (`trap_rounding`
  both seeds, one `fix_off_by_one`, all parked on a question, two with
  no edit) are on the tasks whose notes carry the most. Two seeds do
  not fix the size of the landing effect (D22); its direction agrees
  with this entry. The maintainer stays on. What this prices is the
  lever: a maintainer that runs on fewer steps, or writes less, is
  worth up to 1.3 s a step, and its cost in landing is what a battery
  has to show is still zero.

## D18. One integration test binary per crate
- **Decision.** Each crate's integration tests move from one file per
  binary (`tests/<name>.rs`) to one binary per crate: `tests/it/main.rs`
  declaring `mod <name>;` for every former file, now at
  `tests/it/<name>.rs`, with the shared `tests/common/mod.rs` helpers
  moved alongside. Nothing asserted changes; test names gain the module
  prefix, and the targeted command becomes
  `cargo test -p <crate> --test it <module>::`. Done crate by crate,
  largest first (skills 32 files, core 23, tools 15, cli 8, models 6,
  augment 2, tui 1), each gated before the next.
- **Why.** The gate launched about 103 binaries for under five minutes
  of test CPU: 87 integration files, nine unit binaries, doctests. The
  cost is per binary (link, then the Gatekeeper wait before the first
  instruction, D7), so the count is the lever. The user's question was
  why so many, and the answer was that nobody had chosen the layout.
  About 16 binaries is a six-fold cut whatever the vetting does.
- **Check.** Gate wall clock before and after on the same machine
  state; the count of `Running` lines in the gate log.
- **Addendum (gate 11).** With the terminal's host app granted the
  Developer Tools permission part way through the run, gate 11 took 93
  minutes for 103 binaries against about three hours before: 23
  binaries in the first 43 minutes, then 24 in the next ten. The
  per-binary wait is smaller, not gone. D18 attacks the count.

## D19. A guard's second strike is a question, its third is a park
- **Decision.** Every repeat guard (identical skipped read, identical
  tool error, and any added later) answers the second identical repeat
  with a `think` that quotes what happened and asks for a different
  exact action, and parks only on the third. A guard that parks on the
  second strike is a defect against this rule.
- **Why.** Build 10's identical-error cap parked at the third strike
  with only a prefix line at the second: eight parks, landing down from
  20 to 17 of 26 and the short tasks from 16 of 16 (build 8) to 12 of
  16. Build 11 puts a `think` at the second strike for both guards: 23
  of 26, two parks, short tasks 15 of 16, `multi_file_feature` landed
  on both seeds for the first time. Same tasks, same seeds count, same
  model; the difference is what the harness says on the second repeat.
  This is D9 (the question must fit the failure) applied to the guards
  themselves, and it recovers the regression I had let hide inside the
  full-battery totals.
- **Check.** The eight-short-task row in the README stays at 15 or 16
  of 16 on every later build; a build that drops below that gets the
  paired audit before anything else lands.
- **Result (gate 12).** 16 binaries, 12 minutes end to end, all green,
  against 103 binaries and 93 minutes for gate 11 on the same machine
  state (about three hours before the Developer Tools permission). The
  test-fixture git helpers also stop inheriting the developer's signing
  config, which had failed a test once under load.
- **Addendum (build 12).** A gate is stopped by killing its process
  group; anything launched from the same shell call shares that group.
  A battery runner launched alongside a gate died with it. Batteries
  and gates are launched from separate shell calls.

## D20. Situational rules stay off

- **Decision.** `guards.situational_rules` stays `false`. The static
  procedure graph and the `## Now` section stay in the tree behind the
  switch (Milestone 33); the whole rules block remains the default.
  This is "no gain seen at two seeds, so keep the cheaper setting"
  (D22), not a finding that the section hurts.
- **Evidence (build 13, same binary, same seeds).** Fixer arm with the
  switch off: 22 of 26, median 14.5 steps, mean peak prompt 7290
  tokens, 72 minutes. Switch on: 22 of 26, median 17.5 steps, mean peak
  prompt 7343 tokens, 92 minutes. The two arms differ on two runs in
  opposite directions (`design_fork` seed 2 landed only with the switch
  on, `trap_rounding` seed 2 only with it off), and every miss is a
  known stall shape (a `done` with an open step, a skip-and-continue
  mix, the `refactor_signature` no-green loop). The section saves
  about 130 prompt tokens a step at step 3 (the unit test) and nothing
  at the peak, because the peak is the working set, not the rules.
- **Why the paper's result does not carry.** Procedural Graphs' gain
  came from a learned graph, a guidance model reading a three-step
  trajectory window, and frontier solvers. The cheap adoption keeps
  only the localisation. On a 26B model whose misses are stalls rather
  than wrong rule choices, localising the rules changes which rules
  are shown and not whether the model follows them. The rules block
  was never the context cost that mattered here; the working set is.
- **Ruled out.** Enabling the switch by default; a learned or
  model-edited graph (D9: no second call per step on a 4090).
- **Check.** If a later build's prompt peak becomes rules-dominated
  (rules above a third of the peak), rerun this comparison.

## D21. The no-green cap stays; `refactor_signature` stops being the target

- **Decision.** Item 27 stays at think 12, park 16. No battery is
  spent on `refactor_signature` alone again; it stays in the full
  battery as the task this model does not land.
- **Evidence (build 14).** The cap did what it says: both seeds parked
  at exactly the sixteenth failing test, after the four questions the
  threshold implies, and no landed run in the record would have hit
  it. What it did not do is save much: 1265 s and 1387 s against the
  1500 s timeout. On this task a failing test cycle costs about 80 s,
  so sixteen of them is most of the budget. The run's problem is not
  that it goes on too long; it is that a 26B model cannot hold a
  six-file signature change in a working set of four windows and 6k
  tokens (the prompt peaks near 12k of a 16k context), and each edit
  fixes one compile error by creating another. Lowering the cap to save time
  would cut into landed runs (`multi_file_feature` needed 13 failing
  tests once).
- **Ruled out.** A lower park threshold; a per-task budget; retrying
  the task with a bigger working set (D3 fixed the window on purpose).
- **Check.** The cap's shape string, `no green in 16 test runs`,
  appears in a park only on runs that would otherwise have timed out.
  A landed-task regression that carries that shape reopens the
  threshold.
- **Addendum (build 22, statie with Qwen on the long tasks).**
  `refactor_signature` missed on both seeds with Qwen too (19 and 30
  steps, never green), inside a battery of 3 of 10 against OpenCode's
  10 of 10 on the same model and seeds (README, build 22). The
  "capability miss" reading is closed: the model lands it in a generic
  loop with a 32k window; statie does not with four windows of 16k
  (prompt peaks 12.4k to 14.3k) and, in this battery, a `think` that
  failed on every call (item 38). D25's addendum said this outcome
  would name the four-window layout as the limiter. It does. What to
  do about the layout is the next decision, taken after item 38 gives
  Qwen a working `think` and the numbers are re-measured once; the
  window count and size are config (item 33's sweep, six combinations
  unrun) and the sweep is the instrument.

## D22. Two-seed comparisons pick defaults; they do not establish effects

- **Decision.** A paired battery at two seeds a task (16 short-task
  runs or 26 full-battery runs an arm) decides which setting is the
  default when the two arms are within a couple of runs of each other:
  the cheaper one wins. It does not establish that a feature helps or
  hurts, and the record must not say it does. Claims of an effect need
  a difference larger than the run-to-run swing, which on this battery
  is at least three runs at two seeds, or a battery of six seeds a task
  (about 50 runs an arm, two hours on the 4090) for a gap of one or
  two runs.
- **Why.** The user asked whether 13 of 16 against 12 of 16 (D16) was
  conclusive. It is not, and neither are 21 against 18 of 26 (grounded
  review), 6, 5, 5, and 4 of 10 (D17), or 22 against 22 (D20). Each of
  those decisions actually rested on something else: a feature the
  model never invoked, a guard that never had anything to act on, the
  step count with the maintainer off, no prompt saving at the peak.
  The landing numbers were reported alongside as if they carried the
  decision, and they did not. The same tasks swing between 0 and 2 of
  2 on identical code (`multi_file_feature` across builds 12 to 14), so
  a one-run gap is inside the noise by construction.
- **What still holds.** Harness-owned against model-owned state (D15)
  rests on 13 against 20 median steps and a third of the wall clock
  across 24 runs an arm, not on the equal landing count. The
  second-strike question (D19) rests on a four-run swing on the short
  tasks (12 to 15 of 16) and a six-run swing on the full battery (17 to
  23 of 26), both larger than the noise seen anywhere else.
- **Check.** Any decision in this file that cites a landing gap of two
  runs or fewer as its evidence gets re-read against this entry; the
  README field notes say "not distinguishable at two seeds" for those
  comparisons.

## D23. A second model is a prompt lint; examples must not be answers

- **Decision.** Every example shape in the harness prompt carries a
  placeholder, never a plausible literal. The router cannot `ask_user`.
  Before any Qwen battery counts as a measurement, the eight short
  tasks run first as a smoke test, and any step-1 park is treated as a
  harness defect until shown otherwise.
- **Evidence (build 15).** qwen3-coder parked six of six runs at step 1
  with `ask_user` "Which file?", the literal example text, in about
  one second each, on tasks whose first sentence names the file. gemma
  never copied that example in 300 runs. The harness had handed the
  model an answer in the instructions and one model happened not to
  take it.
- **Why this is a harness result and not a Qwen result.** The prime
  directive puts the harness in charge of what the model sees. A prompt
  whose example is copyable is a harness defect regardless of which
  model copies it. A model with a different prior is the cheapest lint
  for that class of defect: one run, one second, no ambiguity.
- **Check.** The item 28 binary reruns the same six runs; zero step-1
  parks. Any later model swap starts with the same smoke test.

## D24. The decoding format is a per-model config choice; schema off for Qwen

- **Decision.** `bench/configs/qwen_json.json` is the Qwen
  configuration: `format: json`, temperature 0.3, everything else the
  fixer arm's. The code default stays `schema`, which is what gemma
  was measured on. No per-model logic in code; the recommendation is
  the file (the split agreed for the window knobs applies here too).
- **Evidence (build 15, six-run smoke, then sixteen).** qwen3-coder
  under `schema`: 0 of 6 and 0 of 16, with the first rejection at the
  router in fifteen of sixteen runs. Under `json`: 4 of 6, then 11 of
  16 at a median of 19 steps. Temperature 0.7 under `schema`: 1 of 6.
  Same binary, same prompts, same seeds.
- **Why.** Ollama's grammar-constrained decoding steers this model into
  a fixed output (copy the task, send `done`) that it then repeats
  regardless of the rejection note quoting the exact shape. Without
  the grammar the same model reads the note and hands off. The config
  comment already recorded a milder version of this for gemma on
  Ollama 0.33. Whatever the mechanism, it is a property of the model
  and host pair, which is what config files are for.
- **Check.** Any new model starts with the six-run smoke under both
  formats before a battery is read as a measurement.

## D25. The outside baseline: statie's cost is the cache, not the tokens

- **Decision.** The next harness work is the prompt's cacheable prefix,
  not another guard. Every future battery reports Ollama's
  `prompt_eval_duration` per step (item 35), so the share of each
  prompt that was actually evaluated is a first-class number beside
  landing and steps. Until statie's per-step time is within a small
  factor of the append-only baseline on the same model, landing
  comparisons between the two are read as "at best equal".
- **Evidence (build 20).** OpenCode with gemma: 16 of 16 on the eight
  short tasks at 21 s a run; with Qwen 14 of 16 at 15 s. statie on the
  same tasks, models, and seeds: 13 to 15 of 16 at 100 to 178 s with
  gemma, 13 of 16 at 75 to 211 s with Qwen. OpenCode's turns read about
  8,000 tokens from the prefix cache and send about 200 new; a direct
  probe measured a cached prompt at a tenth of its cold evaluation
  time. statie's median model step is 3.9 s for a 6,100-token prompt,
  which is the cold rate.
- **Why this follows from the prime directive rather than against
  it.** "Manage limited context via mutable state" was measured on
  prompt size, where it wins (7k against 9k peak on gemma). It was never
  measured on evaluation cost, where rewriting the prefix loses the
  cache every step. The prefix cache is the model host's own mutable
  state; a harness that owns state should be laying its prompt out so
  the host can keep it. Stable sections first (system, spec, rules,
  task), volatile sections last (working set, observation), and
  per-step edits that append rather than rewrite where the state
  allows it.
- **Confounds recorded.** OpenCode ran at the server's 32k default
  context against statie's pinned 16k; its tool set and system prompt
  are its own; its steps are tool calls, not model turns. None of
  these explain a tenfold time gap on tasks that take six to fifteen
  tool calls.
- **Ruled out.** Reading the landing gap as a harness verdict on gemma
  (one to three runs at two seeds, D22). Dropping harness-owned state:
  the arms measurement (D15) still stands on steps; the cost is in
  prompt layout, not in who owns the state.
- **Check.** A build whose per-step time on gemma is under 1.5 s at
  the same prompt size, and whose landing is unchanged, closes this
  entry.
- **Addendum (long tasks).** OpenCode with Qwen landed
  `refactor_signature` on both seeds with the protected tests untouched
  (38 to 40 tool calls, 19 to 20 edits, 117 s and 252 s); with gemma it
  "passed" only by editing the tests, which the scorer rejects. D21's
  "capability miss" was a statement about gemma. Whether the task is
  out of statie's reach with Qwen is measured on build 21; if statie
  misses it, the four-window layout is the limiter, which is this
  entry's conclusion from the other side.
- **Addendum (build 21): the diagnosis was wrong; the check still
  stands.** With `prompt_eval_duration` reported per step (item 35),
  gemma's build-21 battery (16 of 16) evaluated its 6,000 to 8,800-token
  prompts in a mean 0.46 s a step, a fifth of the cold rate, with 150
  of 266 steps under 0.5 s. The prefix cache is already doing most of
  its work: the system block never changes and the working set is
  rewritten only where the model edited it. The 3.25 s median step
  splits into 0.5 s prompt evaluation, 1.1 s generation (median 220
  completion tokens against OpenCode's 49), 1.4 s for the maintainer's
  own model call, and about 0.3 s of tools. Two thirds of the step is
  output, and the maintainer's share was never counted in D17, which
  weighed it in steps alone. The cacheable-prefix work (item 36) keeps
  a small ceiling, tenths of a second a step. The levers that could
  meet the 1.5 s check are the size of the patch the model writes each
  step and whether the maintainer runs on every step. Neither is
  decided here; item 36's measurement (build 22) comes first, and the
  maintainer's time cost joins D17's evidence when it is weighed again.

## D26. A layout change with a null on its own measure is reverted

- **Decision.** Item 36's prompt order (stable sections first, windows
  oldest first) is reverted to the build-21 order. Layout changes are
  kept only when the number they were made for moves; a reasoned
  order with no measured effect has no claim over the order that
  produced the best battery.
- **Evidence (build 22 against build 21, gemma short tasks, same
  seeds).** Mean `peval` 0.453 s against 0.461 s a step, median 0.48 s
  on both, the share of steps under half a second the same. The cache
  was already hitting (D25 addendum), so there was nothing for the
  order to win. Per-step output rose from a median 220 to 299
  completion tokens and generation from 1.1 s to 1.4 s; landing 13
  against 16. Under D22 the landing delta is not evidence of an effect
  at two seeds, and the output delta is one battery; neither is why
  the change goes. It goes because its measure is null.
- **Ruled out.** Keeping it on the argument that a stable prefix
  cannot hurt: it changed what the model writes, and that is the cost
  centre now (D25 addendum). Chasing the output delta with more seeds
  before reverting: the change would then be justified by a number it
  was not made for.
- **Follow-on.** The three build-22 misses share a shape older than
  the layout, a `done` on an open plan step answered by marking the
  step done and sending `done` again, walked to the non-progress cap.
  Seven such runs across four batteries, one landed. The rejection
  note invites the walk ("set its status to done in your patch");
  that is item 37, dispatched next.

## D27. The long-task gap is measured at equal room, and half of it is the harness

- **Decision.** Long-task comparisons against OpenCode are read at the
  16k limit, not the host's 32k default. The remaining gap at equal
  room, 6 of 10 against statie's 3 of 10 and 53 s against 277 s a run,
  is the harness's to close: the window is no longer the explanation.
  The next long-task work goes to what statie does inside 16k, not to
  a bigger window.
- **Evidence.** OpenCode's `limit` block (`bench/opencode.16k.template.json`,
  context 16384, output 2048) makes it compact near 14k; the event
  streams show peaks of 10.5k to 15.3k and compactions on the long
  runs. Landing fell from 10 of 10 at 32k to 6 of 10, so room was worth
  four runs to the generic loop. statie at 16k, with the four-window
  working set and prompt peaks of 12.4k to 14.3k, landed 3 of 10 on
  the same seeds (README, build 22), with `think` broken on every call
  in that battery (item 38, since fixed) and the not-a-tool loop
  unparked (item 39, since fixed).
- **Why this is the right comparison.** "More with less" is statie's
  premise (the prime directive: manage limited context via mutable
  state). A baseline at twice the room tests nothing about it. A
  generic loop that compacts at 14k is the honest opponent: same
  budget, its own way of spending it.
- **Confounds recorded.** OpenCode's compaction is a summary it writes
  itself, which costs a model call, like the maintainer (D17
  addendum); its per-turn output is a quarter of statie's; the
  8k-output attempt shows its landing is sensitive to exactly where
  the limit sits. statie's 3 of 10 predates items 38 and 39.
- **Ruled out.** Raising statie's window to 32k to match the 10 of 10:
  that abandons the premise to win the number. Reading 6 of 10 as
  "OpenCode fails at 16k": it lands the two tasks statie lands and
  three more.
- **Check.** statie with Qwen on the long tasks, on a build with items
  38 and 39, against 6 of 10 and 53 s. Landing at or above 6 closes
  the gap on landing; time is D25's check.

