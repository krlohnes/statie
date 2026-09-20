# Unit 34: an outside baseline runner for OpenCode

This unit is about `bench/`; editing files under `bench/` is allowed. No Rust changes.
The model host is DOWN and must not be called; the `opencode` binary must not be invoked
against a real model. Test with a stub (below).

## Spec
`bench/run_opencode.sh <label> <provider/model> [--task NAME ...] [--seeds N] [--dry-run]`
(zsh, like `bench/run_sweep.sh`; read that file and `bench/run.sh` first for conventions):
1. Tasks come from `bench/tasks/<name>/task.json` (`name`, `prompt`, `test_cmd`, `protected`,
   `timeout_secs`); default all tasks in directory order; `--seeds` default 2.
2. Per task and seed: `rm -rf` then `cp -R bench/tasks/<name>/fixture/. bench-runs/<label>/<name>/seed<N>/sandbox/`;
   write `bench-runs/<label>/<name>/seed<N>/sandbox/opencode.json` from the template
   `bench/opencode.template.json` (new file; copy the provider block of
   `~/.config/opencode/opencode.json` as it is today but with BOTH models declared:
   `gemma4:26b` and `qwen3-coder:latest`, each `"tool_call": true`; no `model` key).
   Then run, with the task's `timeout_secs` (`timeout` from coreutils if present, else a
   zsh background-and-wait loop; document which):
   `${OPENCODE_BIN:-opencode} run --dir <sandbox> -m <provider/model> --auto --format json --pure "<prompt>"`
   with stdout to `events.jsonl` and stderr to `stderr.txt` in the seed dir. Record start and end
   epoch seconds.
3. Score exactly as `statie bench` does (`cli/src/bench.rs`, read-only): run `test_cmd` in the
   sandbox, stdout+stderr to `verify.txt`; landed = exit 0 AND every `protected` file is
   byte-identical to the fixture's (`cmp -s`); a missing file on either side is a violation.
   outcome = `done` if opencode exited 0 in time, `timeout` if killed by the timeout, `error`
   otherwise.
4. Append to `bench-runs/<label>.out` one line in the SAME shape the batteries print:
   `<task> seed=<N> outcome=<done|timeout|error> landed=<true|false> steps=<S> rej=0 tool_err=<E> edits=<W> green=<G> srv_max=<P> secs=<T>`
   where, from `events.jsonl` parsed defensively (each line JSON if it parses; unknown shapes
   ignored, never a crash): S = number of events that contain a tool invocation (a `part` or
   `tool` object with a `tool` name), E = those whose state/status contains `error`, W = those
   whose tool name is one of `edit`, `write`, `patch`, `multiedit`, G = 1 if `test_cmd` passed
   else 0, P = the largest `tokens.input`/`input_tokens`/`prompt_tokens` value found anywhere in
   the events or `-` if none, T = wall seconds with one decimal. Implement this parsing in
   `bench/opencode_events.py <events.jsonl>` printing `S E W P` on one line, called by the script.
5. After all runs, print the same task table `statie bench` prints (task, landed x/y, median
   steps, mean srv_max, mean secs, outcomes) to stdout and append it to the `.out`, followed by
   `artifacts: bench-runs/<label>`. Append `done <label> <date>` to `bench-runs/<label>.done`.
6. Also record `curl -s --max-time 5 http://192.168.1.222:11434/api/ps` output to
   `bench-runs/<label>/ps_<task>_seed<N>.json` right after each run (tolerate failure).
7. `--dry-run`: set up sandboxes and configs, print each command, run nothing.

## Acceptance
- A stub `bench/tests/fake_opencode.sh` (new; executable) that ignores its arguments except
  `--dir`, writes a passing solution for `add_function` into the sandbox by copying
  `bench/tasks/add_function/solution/.` over it, and prints five JSON event lines to stdout of
  these shapes (one per line):
  `{"type":"step_start"}`, `{"type":"message.part.updated","part":{"type":"tool","tool":"read","state":{"status":"completed"}}}`,
  `{"type":"message.part.updated","part":{"type":"tool","tool":"edit","state":{"status":"completed"}}}`,
  `{"type":"message.part.updated","part":{"type":"tool","tool":"bash","state":{"status":"error"}}}`,
  `{"type":"step_finish","tokens":{"input":4321,"output":120}}`.
  Running `OPENCODE_BIN=bench/tests/fake_opencode.sh bench/run_opencode.sh oc_stub ollama/gemma4:26b --task add_function --seeds 1`
  must produce the `.out` line `add_function seed=1 outcome=done landed=true steps=3 rej=0 tool_err=1 edits=1 green=1 srv_max=4321 secs=<T>` (assert everything but T in your report by grepping) and the table. Then a second stub run with `--task python_method` (no solution copied for it, so the tests fail) must give `landed=false green=0`. Remove `bench-runs/oc_stub` afterwards.
- `zsh -n` on the script, `py_compile` on the parser.
- `bench/opencode_events.py` on an empty file prints `0 0 0 -`.

## Boundaries
`bench/run_opencode.sh`, `bench/opencode_events.py`, `bench/opencode.template.json`,
`bench/tests/fake_opencode.sh` only. No cargo. No real `opencode` invocation. No .md edits.
