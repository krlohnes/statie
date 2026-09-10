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
