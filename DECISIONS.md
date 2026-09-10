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

## D3. Long tasks are calibrated by running, not by design
- **Decision.** A long-horizon task is accepted when a clean-build run
  lands at least one seed in 40 to 80 steps; a task that lands in under
  25 steps on both seeds is sent back to be made harder, and one that
  fails both seeds on the current build is kept only if the failure is
  a harness stall rather than an impossible task.
- **Why.** Step count on gemma4 cannot be predicted from the fixture;
  the existing seven tasks were expected to vary and all land in 9 to
  35 steps.
- **Check.** The accepted tasks' step counts in the README table.

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
