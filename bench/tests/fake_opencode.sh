#!/bin/zsh
# Stub for the real `opencode` binary, used by bench/run_opencode.sh's own
# tests (unit 34). Ignores every argument except `--dir <sandbox>`, writes a
# passing solution for add_function into that sandbox (by copying
# bench/tasks/add_function/solution/. over it -- this is the ONLY task this
# stub knows how to "solve"; any other task is left untouched, so its
# test_cmd fails and the run is not landed), then prints five JSON event
# lines to stdout mimicking opencode's --format json event shapes.
set -u

# Resolve the repo root from this script's own location so it works no
# matter what directory it's invoked from.
script_dir="${0:A:h}"
repo_root="${script_dir:h:h}"

dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dir)
      dir="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

if [ -n "$dir" ]; then
  solution="$repo_root/bench/tasks/add_function/solution/."
  if [ -d "$repo_root/bench/tasks/add_function/solution" ]; then
    cp -R "$solution" "$dir/"
  fi
fi

cat <<'EOF'
{"type":"step_start"}
{"type":"message.part.updated","part":{"type":"tool","tool":"read","state":{"status":"completed"}}}
{"type":"message.part.updated","part":{"type":"tool","tool":"edit","state":{"status":"completed"}}}
{"type":"message.part.updated","part":{"type":"tool","tool":"bash","state":{"status":"error"}}}
{"type":"step_finish","tokens":{"input":4321,"output":120}}
EOF
