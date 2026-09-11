#!/bin/zsh
# Run the three state-ownership arms back to back on the full battery
# (BACKLOG.md item 2). One battery at a time on the GPU (DECISIONS.md D8).
cd "$(dirname "$0")/.."
mkdir -p bench-runs
for arm in current model_owned harness_max; do
  label="arm_${arm}_$1"
  target/debug/statie bench --config "bench/configs/arm_${arm}.json" --out "bench-runs/$label" --keep --seeds 2 > "bench-runs/$label.out" 2>&1
  echo "done $label $(date)" >> bench-runs/arms_$1.done
done
