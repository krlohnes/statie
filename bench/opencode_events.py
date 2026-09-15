#!/usr/bin/env python3
"""Parse an opencode `events.jsonl` transcript into the four bench counters.

Usage: opencode_events.py <events.jsonl>
Prints one line: "S E W P"
  S = number of events that contain a tool invocation (a `part` or `tool`
      object with a `tool` name)
  E = number of those whose state/status contains "error"
  W = number of those whose tool name is one of edit, write, patch,
      multiedit
  P = the largest tokens.input / input_tokens / prompt_tokens value found
      anywhere in the events, or "-" if none

Parsing is defensive: each line is JSON if it parses, unknown shapes are
ignored, and the file never causes a crash (a missing file, an empty file,
or a file full of garbage all just produce "0 0 0 -").
"""

import json
import sys

WRITE_TOOLS = {"edit", "write", "patch", "multiedit"}


def _tool_obj(event):
    """Return the `part` or `tool` object carrying a tool invocation, if
    `event` has one, else None. A tool invocation is any dict (the event
    itself, its "part", or its "tool") that has a "tool" key naming the
    tool.
    """
    if not isinstance(event, dict):
        return None
    for candidate in (event.get("part"), event.get("tool"), event):
        if isinstance(candidate, dict) and isinstance(candidate.get("tool"), str):
            return candidate
    return None


def _has_error(obj):
    """True if `obj`'s state/status (however nested) mentions "error"."""
    state = obj.get("state")
    status = None
    if isinstance(state, dict):
        status = state.get("status")
    elif isinstance(state, str):
        status = state
    if status is None:
        status = obj.get("status")
    if isinstance(status, str) and "error" in status.lower():
        return True
    return False


def _numeric_tokens(value):
    """Yield every int/float found in the token-count fields of interest,
    walking `value` (dict/list/scalar) recursively but only collecting from
    keys named tokens/input/input_tokens/prompt_tokens.
    """
    if isinstance(value, dict):
        for key, sub in value.items():
            if key == "tokens" and isinstance(sub, dict):
                for inner_key in ("input", "input_tokens", "prompt_tokens"):
                    inner = sub.get(inner_key)
                    if isinstance(inner, (int, float)) and not isinstance(inner, bool):
                        yield inner
            elif key in ("input_tokens", "prompt_tokens", "input") and isinstance(
                sub, (int, float)
            ) and not isinstance(sub, bool):
                yield sub
            yield from _numeric_tokens(sub)
    elif isinstance(value, list):
        for item in value:
            yield from _numeric_tokens(item)


def parse_events(lines):
    steps = 0
    tool_errors = 0
    edits = 0
    srv_max = None

    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except (ValueError, TypeError):
            continue

        tool_obj = _tool_obj(event)
        if tool_obj is not None:
            steps += 1
            if _has_error(tool_obj):
                tool_errors += 1
            if tool_obj.get("tool") in WRITE_TOOLS:
                edits += 1

        for n in _numeric_tokens(event):
            if srv_max is None or n > srv_max:
                srv_max = n

    return steps, tool_errors, edits, srv_max


def main(argv):
    if len(argv) != 2:
        print("usage: opencode_events.py <events.jsonl>", file=sys.stderr)
        return 2

    try:
        with open(argv[1], "r", encoding="utf-8", errors="replace") as f:
            lines = f.readlines()
    except OSError:
        lines = []

    steps, tool_errors, edits, srv_max = parse_events(lines)
    srv_max_str = "-" if srv_max is None else str(int(srv_max) if float(srv_max).is_integer() else srv_max)
    print(f"{steps} {tool_errors} {edits} {srv_max_str}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
