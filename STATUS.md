# Status, 2026-09-15 morning

## Where the code is
Main is ahead of the last push (`c191e12`) by items 28 to 36, the Qwen
and OpenCode bench tooling, D23 to D25, and builds 15 to 20. Tree
clean. Gate: 16 binaries, 12 to 19 minutes.

## The outside baseline (D25) changed the direction
OpenCode with the same two models on the same tasks, scored the same
way: gemma 16 of 16 short tasks at 21 s a run, Qwen 14 of 16 at 15 s;
statie 13 to 15 and 13 at 100 to 211 s. Long tasks: OpenCode Qwen 10 of
10 including `refactor_signature` honestly on both seeds; OpenCode
gemma 8 of 10, its refactor "passes" rejected for editing the
protected tests. A direct probe on Ollama showed a cached prompt
evaluating in a tenth of its cold time. OpenCode reads 97% of each
prompt from the cache; statie rewrites its prompt every step and pays
the cold rate. The state design saves tokens and spends the cache.

So the next harness work is the prompt's cacheable prefix:
- Item 35 (committed): every step records Ollama's prompt-evaluation
  time (`peval`), the only cache signal the host gives.
- Item 36 (committed): stable sections first, volatile last, windows in
  arrival order. Measured by `peval` on build 22 against build 21.

## Running now
Nothing can run: the Ollama host dropped at 09:17 and is still down.
A chain waits for it and then runs, in order: build 21 (item 35
binary, from a side worktree), build 22 (item 36 layout), the statie
Qwen long tasks. About two hours once the host answers.

## Open threads
- Push is yours.
- OpenCode's shell tool is not confined to its working directory; one
  Qwen run compiled a scratch program in the repository root (removed).
- `refactor_signature` is a gemma limit, not a task limit; whether
  statie lands it with Qwen is in the chain above.
