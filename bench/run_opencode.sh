#!/bin/zsh
# An outside baseline runner for OpenCode (BACKLOG.md item 34).
#
#   bench/run_opencode.sh <label> <provider/model> [--task NAME ...] \
#     [--seeds N] [--dry-run]
#
# OPENCODE_TEMPLATE=<path> picks the opencode.json template (default
# bench/opencode.template.json); bench/opencode.16k.template.json declares a
# 16k `limit.context` on both models so OpenCode compacts its history to the
# window statie pins, instead of riding the host's 32k server default. Its
# `limit.output` is 2048: OpenCode compacts at context minus output, and a
# first run with output 8192 compacted at about 8k of prompt (34 compactions
# in one run, no edits landed), which is not the 16k test.
#
# Runs the opencode CLI (or a stub via OPENCODE_BIN, for testing -- the real
# model host is down and must never be invoked by this script) over the same
# bench/tasks/<name>/ fixtures `statie bench` uses, and scores each run the
# same way `statie bench` does (cli/src/bench.rs, read-only): test_cmd exit
# 0 AND every protected file byte-identical to the fixture's.
#
# Output, mirroring bench/run_sweep.sh's conventions:
#   bench-runs/<label>/<task>/seed<N>/sandbox/   -- the opencode-run sandbox
#   bench-runs/<label>/<task>/seed<N>/events.jsonl
#   bench-runs/<label>/<task>/seed<N>/stderr.txt
#   bench-runs/<label>/<task>/seed<N>/verify.txt
#   bench-runs/<label>/ps_<task>_seed<N>.json    -- `ollama ps` snapshot
#   bench-runs/<label>.out                       -- one line per run, then
#                                                    the task table
#   bench-runs/<label>.done                      -- "done <label> <date>"
set -u
zmodload zsh/datetime 2>/dev/null
cd "$(dirname "$0")/.."

if [ $# -lt 2 ]; then
  echo "usage: run_opencode.sh <label> <provider/model> [--task NAME ...] [--seeds N] [--dry-run]" >&2
  exit 1
fi

label=$1; model=$2
shift 2

tasks=()
seeds=2
dry_run=0

while [ $# -gt 0 ]; do
  case "$1" in
    --task)
      tasks+=("$2")
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

opencode_bin="${OPENCODE_BIN:-opencode}"

# Which timeout mechanism this run uses: coreutils `timeout` when present,
# else a zsh background-and-wait loop that kills the job after the given
# number of seconds. Documented here rather than guessed at call sites.
if command -v timeout >/dev/null 2>&1; then
  timeout_mode="coreutils"
elif command -v gtimeout >/dev/null 2>&1; then
  timeout_mode="coreutils"
else
  timeout_mode="zsh-loop"
fi

out_dir="bench-runs/$label"
mkdir -p "$out_dir"
out_file="bench-runs/$label.out"
: > "$out_file"

# Task discovery: bench/tasks/<name>/task.json, default all tasks in
# directory order (matches cli/src/bench.rs's discover_tasks: sorted dir
# listing, filtered to --task names when given).
task_dirs=(bench/tasks/*(/N))
task_names=()
for d in "${task_dirs[@]}"; do
  name="${d:t}"
  if [ ${#tasks[@]} -gt 0 ]; then
    found=0
    for t in "${tasks[@]}"; do
      [ "$t" = "$name" ] && found=1
    done
    [ $found -eq 0 ] && continue
  fi
  task_names+=("$name")
done

# Run a shell command under the task's timeout, using whichever mechanism
# is available. Writes stdout to $1, stderr to $2, exit code left in
# $run_exit, wall seconds (one decimal) left in $run_secs.
run_with_timeout() {
  local timeout_secs="$1"; shift
  local stdout_file="$1"; shift
  local stderr_file="$1"; shift

  local start_ts=$EPOCHSECONDS
  if [ "$timeout_mode" = "coreutils" ]; then
    local to_cmd="timeout"
    command -v timeout >/dev/null 2>&1 || to_cmd="gtimeout"
    "$to_cmd" "${timeout_secs}s" "$@" >"$stdout_file" 2>"$stderr_file"
    run_exit=$?
  else
    "$@" >"$stdout_file" 2>"$stderr_file" &
    local job_pid=$!
    (
      sleep "$timeout_secs"
      kill -TERM "$job_pid" 2>/dev/null
    ) &
    local killer_pid=$!
    wait "$job_pid"
    run_exit=$?
    kill "$killer_pid" 2>/dev/null
    wait "$killer_pid" 2>/dev/null
  fi
  local end_ts=$EPOCHSECONDS
  run_secs=$(( end_ts - start_ts ))
}

for task_name in "${task_names[@]}"; do
  task_json="bench/tasks/$task_name/task.json"
  prompt=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['prompt'])" "$task_json")
  test_cmd=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['test_cmd'])" "$task_json")
  timeout_secs=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['timeout_secs'])" "$task_json")
  protected_json=$(python3 -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1])).get('protected', [])))" "$task_json")
  fixture_dir="bench/tasks/$task_name/fixture"

  for seed in $(seq 1 "$seeds"); do
    seed_dir="$out_dir/$task_name/seed$seed"
    sandbox="$seed_dir/sandbox"
    rm -rf "$seed_dir"
    mkdir -p "$sandbox"
    cp -R "$fixture_dir/." "$sandbox/"

    # Write opencode.json from the template, with the model already fixed
    # by -m on the command line, so the template carries only the provider
    # block (both models declared, no top-level "model" key -- see
    # bench/opencode.template.json).
    cp "${OPENCODE_TEMPLATE:-bench/opencode.template.json}" "$sandbox/opencode.json"

    cmd=("$opencode_bin" run --dir "$sandbox" -m "$model" --auto --format json --pure "$prompt")

    if [ "$dry_run" -eq 1 ]; then
      echo "would run: ${cmd[*]}"
      continue
    fi

    echo "run: ${cmd[*]}"

    events_file="$seed_dir/events.jsonl"
    stderr_file="$seed_dir/stderr.txt"
    verify_file="$seed_dir/verify.txt"

    start_epoch=$EPOCHSECONDS
    run_with_timeout "$timeout_secs" "$events_file" "$stderr_file" "${cmd[@]}"
    end_epoch=$EPOCHSECONDS
    wall_secs=$(python3 -c "print(f'{$end_epoch - $start_epoch:.1f}')")

    # Snapshot server state right after each run (tolerate failure).
    curl -s --max-time 5 http://192.168.1.222:11434/api/ps \
      > "$out_dir/ps_${task_name}_seed${seed}.json" 2>/dev/null || true

    # outcome: done if opencode exited 0 in time, timeout if killed by the
    # timeout, error otherwise. `timeout`/`gtimeout` report exit 124 when
    # they kill the child; the zsh-loop path leaves run_exit as whatever
    # `wait` returned for a TERM-killed job (128+15), also treated as a
    # timeout since the killer only fires after timeout_secs.
    if [ "$run_exit" -eq 0 ]; then
      outcome="done"
    elif [ "$run_exit" -eq 124 ] || [ "$run_exit" -eq 143 ] || [ "$run_exit" -eq $((128 + 15)) ]; then
      outcome="timeout"
    else
      outcome="error"
    fi

    # Score: run test_cmd in the sandbox, stdout+stderr to verify.txt;
    # landed = exit 0 AND every protected file byte-identical to fixture's.
    ( cd "$sandbox" && sh -c "$test_cmd" ) >"$verify_file" 2>&1
    test_exit=$?

    landed="false"
    green=0
    if [ "$test_exit" -eq 0 ]; then
      green=1
      protected_ok=$(python3 -c "
import json, sys, filecmp, os

protected = json.loads(sys.argv[1])
fixture_dir = sys.argv[2]
sandbox_dir = sys.argv[3]

ok = True
for rel in protected:
    a = os.path.join(fixture_dir, rel)
    b = os.path.join(sandbox_dir, rel)
    if not os.path.isfile(a) or not os.path.isfile(b):
        ok = False
        break
    if not filecmp.cmp(a, b, shallow=False):
        ok = False
        break
print('true' if ok else 'false')
" "$protected_json" "$fixture_dir" "$sandbox")
      if [ "$protected_ok" = "true" ]; then
        landed="true"
      fi
    fi

    # Parse events.jsonl defensively via bench/opencode_events.py.
    read -r steps tool_err edits srv_max <<< "$(python3 bench/opencode_events.py "$events_file")"

    # opencode's events.jsonl carries no Ollama prompt/eval timing, so this
    # runner always reports peval=- (unit 35: only statie's own Ollama
    # backend reports prompt_eval_secs/eval_secs).
    line="$task_name seed=$seed outcome=$outcome landed=$landed steps=$steps rej=0 tool_err=$tool_err edits=$edits green=$green srv_max=$srv_max secs=$wall_secs peval=-"
    echo "$line"
    echo "$line" >> "$out_file"

    # Stash per-run fields for the table pass below, keyed by task.
    echo "$task_name $landed $steps $srv_max $wall_secs $outcome" >> "$out_dir/.rows_$task_name.tmp"
  done
done

if [ "$dry_run" -eq 1 ]; then
  exit 0
fi

# Task table: task | landed k/n | median steps | mean srv_max | mean peval |
# mean secs | outcomes -- same shape as cli/src/bench.rs::print_task_table.
# opencode's events.jsonl carries no Ollama prompt/eval timing, so mean peval
# is always "-" here (unit 35).
table=$(python3 - "$out_dir" "${task_names[@]}" <<'PY'
import statistics
import sys

out_dir = sys.argv[1]
task_names = sys.argv[2:]

print(f"{'task':<24} {'landed':>10} {'median steps':>13} {'mean srv_max':>13} {'mean peval':>10} {'mean secs':>10}  outcomes")
for name in task_names:
    path = f"{out_dir}/.rows_{name}.tmp"
    try:
        with open(path) as f:
            rows = [line.split() for line in f if line.strip()]
    except OSError:
        rows = []
    if not rows:
        continue
    landed = sum(1 for r in rows if r[1] == "true")
    total = len(rows)
    steps = sorted(int(r[2]) for r in rows)
    n = len(steps)
    if n == 0:
        median_steps = 0.0
    elif n % 2 == 1:
        median_steps = float(steps[n // 2])
    else:
        median_steps = (steps[n // 2 - 1] + steps[n // 2]) / 2.0
    srv_vals = [float(r[3]) for r in rows if r[3] != "-"]
    mean_srv = sum(srv_vals) / len(srv_vals) if srv_vals else 0.0
    secs_vals = [float(r[4]) for r in rows]
    mean_secs = sum(secs_vals) / len(secs_vals) if secs_vals else 0.0
    outcomes = {}
    for r in rows:
        outcomes[r[5]] = outcomes.get(r[5], 0) + 1
    outcomes_str = ",".join(f"{k}={v}" for k, v in sorted(outcomes.items()))
    print(f"{name:<24} {landed:>7}/{total:<2} {median_steps:>13.1f} {mean_srv:>13.1f} {'-':>10} {mean_secs:>10.1f}  {outcomes_str}")
PY
)

echo "$table"
echo "$table" >> "$out_file"
echo "artifacts: bench-runs/$label" | tee -a "$out_file"

rm -f "$out_dir"/.rows_*.tmp

echo "done $label $(date)" >> "bench-runs/$label.done"
