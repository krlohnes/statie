#!/bin/zsh
# Maintainer sweep (BACKLOG.md item 10): notes budget 200 / 400 (= arm_current) / 800
# and maintainer off, on the long tasks and the two traps. One battery at a time (D8).
cd "$(dirname "$0")/.."
mkdir -p bench-runs
TASKS=(--task multi_file_feature --task python_three_modules --task refactor_signature --task trap_rounding --task trap_interest)
for cfg in sweep_m200 sweep_m800 sweep_m_off; do
  label="${cfg}_$1"
  target/debug/statie bench --config "bench/configs/$cfg.json" --out "bench-runs/$label" --keep --seeds 2 "${TASKS[@]}" > "bench-runs/$label.out" 2>&1
  echo "done $label $(date)" >> "bench-runs/sweep_$1.done"
done
