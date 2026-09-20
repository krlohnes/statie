#!/bin/zsh
# Knob sweep runner (BACKLOG.md item 33).
#
#   bench/run_sweep.sh <label> <base_config.json> --set <dotted.key>=<v1>,<v2>[,...] \
#     [--set ...] [--task NAME ...] [--seeds N] [--dry-run]
#
# STATIE_BIN=<path> picks the statie binary (default target/debug/statie), so a
# sweep can run from a copy while the workspace gate rebuilds the default.
#
# Derives one config per combination in the cartesian product of every --set
# (in the order given), refuses (prints a reason, skips) any combination
# whose budget.working_set > budget.total or budget.total > model.num_ctx,
# and runs the rest one after another with `statie bench`.
#
# The maintainer sweep this file used to hardcode (BACKLOG.md item 10: notes
# budget 200 / 400 (= arm_current) / 800 and maintainer off, on the long
# tasks and the two traps) is now just an invocation of this runner:
#
#   bench/run_sweep.sh maintainer_sweep bench/configs/sweep_m400.json \
#     --set maintainer.budget=200,400,800 \
#     --task multi_file_feature --task python_three_modules \
#     --task refactor_signature --task trap_rounding --task trap_interest
#
# Output: bench-runs/<label>/configs/<combo>.json (derived configs),
# bench-runs/<label>/<combo>.out (one battery run each), bench-runs/<label>.done
# (one "done <combo> <date>" line per completed run, plus a final
# "sweep complete <date>").
set -u
cd "$(dirname "$0")/.."

label=$1; base_config=$2
shift 2

sets=()
tasks=()
seeds=2
dry_run=0

while [ $# -gt 0 ]; do
  case "$1" in
    --set)
      sets+=("$2")
      shift 2
      ;;
    --task)
      tasks+=(--task "$2")
      shift 2
      ;;
    --seeds)
      seeds="$2"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [ ${#sets[@]} -eq 0 ]; then
  echo "at least one --set is required" >&2
  exit 1
fi

mkdir -p "bench-runs/$label/configs"

# Build the cartesian product of every --set's values as a list of
# "key1=val1;key2=val2;..." combo specs, one per line, preserving --set order.
combos=$(python3 - "${sets[@]}" <<'PY'
import itertools
import sys

sets = sys.argv[1:]
keys = []
value_lists = []
for s in sets:
    key, _, raw = s.partition("=")
    values = raw.split(",")
    keys.append(key)
    value_lists.append(values)

for combo in itertools.product(*value_lists):
    parts = [f"{k}={v}" for k, v in zip(keys, combo)]
    print(";".join(parts))
PY
)

if [ -z "$combos" ]; then
  echo "no combinations produced from --set arguments" >&2
  exit 1
fi

echo "$combos" | while IFS= read -r spec; do
  [ -z "$spec" ] && continue

  # combo name: join <lastkeysegment>-<value> with _
  combo_name=$(python3 - "$spec" <<'PY'
import sys

spec = sys.argv[1]
parts = []
for kv in spec.split(";"):
    key, _, val = kv.partition("=")
    last = key.rsplit(".", 1)[-1]
    parts.append(f"{last}-{val}")
print("_".join(parts))
PY
)

  derived="bench-runs/$label/configs/$combo_name.json"

  # Derive the config: start from base_config, set each dotted key (parsed as
  # JSON when it parses, else as a string), creating intermediate objects as
  # needed.
  python3 - "$base_config" "$derived" "$spec" <<'PY'
import json
import sys

base_path, out_path, spec = sys.argv[1], sys.argv[2], sys.argv[3]

with open(base_path) as f:
    cfg = json.load(f)

for kv in spec.split(";"):
    key, _, raw = kv.partition("=")
    try:
        value = json.loads(raw)
    except ValueError:
        value = raw

    segments = key.split(".")
    node = cfg
    for seg in segments[:-1]:
        if seg not in node or not isinstance(node[seg], dict):
            node[seg] = {}
        node = node[seg]
    node[segments[-1]] = value

with open(out_path, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
PY
  derive_status=$?
  if [ $derive_status -ne 0 ]; then
    echo "failed to derive config for $combo_name" >&2
    exit 1
  fi

  # Refusal check: budget.working_set > budget.total, or
  # budget.total > model.num_ctx.
  refuse_reason=$(python3 - "$derived" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    cfg = json.load(f)

budget = cfg.get("budget", {})
model = cfg.get("model", {})
working_set = budget.get("working_set")
total = budget.get("total")
num_ctx = model.get("num_ctx")

if working_set is not None and total is not None and working_set > total:
    print(f"budget.working_set ({working_set}) > budget.total ({total})")
elif total is not None and num_ctx is not None and total > num_ctx:
    print(f"budget.total ({total}) > model.num_ctx ({num_ctx})")
PY
)

  if [ -n "$refuse_reason" ]; then
    echo "refuse $combo_name: $refuse_reason"
    continue
  fi

  cmd=("${STATIE_BIN:-target/debug/statie}" bench --config "$derived" --out "bench-runs/$label/$combo_name" --keep --seeds "$seeds" "${tasks[@]}")

  if [ "$dry_run" -eq 1 ]; then
    echo "would run: ${cmd[*]} > bench-runs/$label/$combo_name.out 2>&1"
    continue
  fi

  echo "run: ${cmd[*]}"
  "${cmd[@]}" > "bench-runs/$label/$combo_name.out" 2>&1
  echo "done $combo_name $(date)" >> "bench-runs/$label.done"
done

if [ "$dry_run" -eq 0 ]; then
  echo "sweep complete $(date)" >> "bench-runs/$label.done"
fi
