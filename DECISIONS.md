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
  measurement (backlog item 14) found no landing effect either way and a
  small cost to listing: about 400 prompt tokens at peak and a few more
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
