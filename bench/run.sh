#!/bin/zsh
# Run a battery detached from the terminal and keep every artifact.
#   bench/run.sh <label> <config> [--task NAME ...] [--seeds N]
# Output: bench-runs/<label>/ (gitignored), summary in bench-runs/<label>.out
set -u
cd "$(dirname "$0")/.."
label=$1; cfg=$2; shift 2
mkdir -p bench-runs
if [ ! -x target/debug/statie ]; then cargo build -p statie 2>&1 | tail -3; fi
nohup target/debug/statie bench --config "$cfg" --out "bench-runs/$label" --keep --seeds 2 "$@" \
  > "bench-runs/$label.out" 2>&1 &
disown
echo "started $label (pid $!) -> bench-runs/$label.out"
