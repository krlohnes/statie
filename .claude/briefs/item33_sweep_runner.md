# Unit 33: a knob sweep runner

This unit is about `bench/`; editing files under `bench/` is allowed for it. No Rust changes.

## Spec
1. `bench/run_sweep.sh` is rewritten (keep the file name; the maintainer sweep it used to
   hardcode becomes an invocation documented in its header comment):
   `bench/run_sweep.sh <label> <base_config.json> --set <dotted.key>=<v1>,<v2>[,...] [--set ...] [--task NAME ...] [--seeds N] [--dry-run]`
   - Values are parsed as JSON when they parse (numbers, true/false), else as strings.
   - Combinations: the cartesian product of every `--set`, in the order given. Each derives
     `bench-runs/<label>/configs/<combo>.json` from the base, where `<combo>` joins
     `<lastkeysegment>-<value>` with `_` (e.g. `max_windows-8_working_set-9000_num_ctx-24576`).
     Derivation is done by an inline `python3` snippet (no new dependency), setting the dotted key
     (creating intermediate objects if missing).
   - Refuse (print a one-line reason, skip, continue) any combination whose
     `budget.working_set > budget.total` or `budget.total > model.num_ctx`.
   - Run each remaining combination as
     `target/debug/statie bench --config <derived> --out bench-runs/<label>/<combo> --keep --seeds <N> <tasks...> > bench-runs/<label>/<combo>.out 2>&1`
     one after another, appending `done <combo> <date>` to `bench-runs/<label>.done`, and finally
     `sweep complete <date>`.
   - `--dry-run` writes the derived configs and prints each command and each refusal, runs nothing.
   - Default seeds 2; no default tasks (all tasks).
2. `bench/sweep_summary.py <label>`: reads every `bench-runs/<label>/*.out`, parses the
   `outcome=` lines (same format the batteries print: `landed=`, `steps=`, `srv_max=`, `secs=`),
   and prints a table: combo, landed/total, median steps, mean srv_max (skip `-`), mean secs,
   sorted by landed descending then median steps ascending. Plain text, aligned columns.
3. `bench/run_arms.sh` and the existing `sweep_*` configs stay as they are.

## Acceptance
- `bench/run_sweep.sh demo bench/configs/qwen_json.json --set working_set.max_windows=4,8 --set budget.working_set=6000,9000,20000 --set model.num_ctx=16384,24576 --task add_function --seeds 1 --dry-run`
  prints 12 combinations: 8 commands and 4 refusals (every `working_set=20000` combination fails
  `budget.working_set > budget.total` since total is 14000), and the derived files exist with the
  right values (show `python3 -c` checks of two of them in your report).
- `python3 bench/sweep_summary.py sweep_b5` against the existing `bench-runs/sweep_m200_b5.out`
  style files is NOT expected to work (different layout); instead create a fixture
  `bench-runs/demo_summary/a.out` and `b.out` with three `outcome=` lines each in your report's
  own test and show the table. Remove the fixture afterwards.
- `zsh -n bench/run_sweep.sh` passes; `python3 -m py_compile bench/sweep_summary.py` passes.

## Boundaries
`bench/run_sweep.sh`, `bench/sweep_summary.py` only. Do not run any real battery (no invocation
without `--dry-run`); the model host must not be called. No cargo. No .md edits.
