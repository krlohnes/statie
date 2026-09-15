# Status, 2026-09-15 afternoon

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 35 and 37,
the Qwen and OpenCode bench tooling, D23 to D26, and builds 15 to 22.
Item 36 was committed and then reverted (D26). Tree clean apart from
the item 38 agent's worktree. Gate: 16 binaries, 12 to 19 minutes.

## What the measurements said today
- Build 21 (gemma, `peval` reported): 16 of 16, the first full gemma
  battery. Prompt evaluation is 0.46 s a step at 6k to 8.8k tokens, a
  fifth of the cold rate: the cache already hits. The 3.25 s step is
  1.1 s generation (220 tokens), 1.4 s the maintainer's own call, 0.5 s
  prompt evaluation. D25's cache diagnosis was wrong; its 1.5 s check
  stands. The levers are per-step output and the maintainer call.
- Build 22 (item 36 layout): `peval` unchanged (0.453 against 0.461),
  output up (299 tokens a step), 13 of 16. Reverted, D26.
- The three build-22 misses walk the plan: a `done` on an open step,
  refused, answered by marking the step done and `done` again. Seven
  such runs across four batteries, one landed; the refusal note
  coached it. Item 37 (committed, Milestone 43): the repeat refusal
  drops the status change and demands a tool call. Measured next on
  the gemma short battery, build 23.
- statie with Qwen on the long tasks: 3 of 10 at 277 s against
  OpenCode's 10 of 10 at 159 s, `refactor_signature` 0 of 2. D21's
  capability reading is closed: the four-window 16k layout is the
  limiter (D21 addendum).
- Every `think` in every Qwen battery failed: Ollama refuses the
  thinking flag for qwen3-coder and the thinker sends it regardless of
  the model config. Item 38 (agent running) makes the thinker read the
  configured flag. All Qwen numbers so far are a floor.

## Running now
The item 38 agent. The host is idle. Next batteries, one at a time:
gemma short (build 23, item 37), Qwen short (build 23, item 38).

## Open threads
- Push is yours.
- What to do about the window layout for long tasks is the next
  decision, after item 38 and one re-measurement; item 33's sweep (six
  combinations unrun, paused by you) is the instrument.
- OpenCode's shell tool is not confined to its working directory; one
  Qwen run compiled a scratch program in the repository root (removed).
