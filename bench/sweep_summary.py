#!/usr/bin/env python3
"""Summarize a knob sweep (BACKLOG.md item 33).

    python3 bench/sweep_summary.py <label>

Reads every bench-runs/<label>/*.out file (one per combo, written by
bench/run_sweep.sh), parses each battery's "outcome=" lines (the same
format the batteries print: task, seed, outcome, landed, steps, rej,
tool_err, edits, green, srv_max, secs, peval), and prints one row per combo:

    combo  landed/total  median steps  mean srv_max  mean peval  mean secs

sorted by landed descending, then median steps ascending. srv_max/peval
values of "-" are skipped when computing that combo's mean; a combo with no
peval value at all reports "-" for mean peval rather than 0.0.
"""
import statistics
import sys
from pathlib import Path


def parse_out_file(path):
    """Parse the outcome= lines of one .out file into a list of dicts."""
    rows = []
    for line in path.read_text().splitlines():
        fields = line.split()
        if not any(f.startswith("outcome=") for f in fields):
            continue

        row = {}
        for field in fields:
            if "=" not in field:
                continue
            key, _, value = field.partition("=")
            row[key] = value
        if "outcome" not in row:
            continue
        rows.append(row)
    return rows


def summarize(label):
    runs_dir = Path("bench-runs") / label
    out_files = sorted(runs_dir.glob("*.out"))

    combo_rows = []
    for out_file in out_files:
        combo = out_file.stem
        rows = parse_out_file(out_file)
        if not rows:
            continue

        total = len(rows)
        landed = sum(1 for r in rows if r.get("landed") == "true")

        steps = [int(r["steps"]) for r in rows if "steps" in r]
        median_steps = statistics.median(steps) if steps else 0.0

        srv_maxes = [
            int(r["srv_max"])
            for r in rows
            if r.get("srv_max") not in (None, "-")
        ]
        mean_srv_max = sum(srv_maxes) / len(srv_maxes) if srv_maxes else 0.0

        secs = [float(r["secs"]) for r in rows if "secs" in r]
        mean_secs = sum(secs) / len(secs) if secs else 0.0

        pevals = [
            float(r["peval"])
            for r in rows
            if r.get("peval") not in (None, "-")
        ]
        mean_peval = sum(pevals) / len(pevals) if pevals else None

        combo_rows.append(
            {
                "combo": combo,
                "landed": landed,
                "total": total,
                "median_steps": median_steps,
                "mean_srv_max": mean_srv_max,
                "mean_peval": mean_peval,
                "mean_secs": mean_secs,
            }
        )

    combo_rows.sort(key=lambda r: (-r["landed"], r["median_steps"]))
    return combo_rows


def format_table(combo_rows):
    header = (
        "combo",
        "landed",
        "median steps",
        "mean srv_max",
        "mean peval",
        "mean secs",
    )
    lines = []

    body = []
    for r in combo_rows:
        mean_peval = r["mean_peval"]
        body.append(
            (
                r["combo"],
                f"{r['landed']}/{r['total']}",
                f"{r['median_steps']:.1f}",
                f"{r['mean_srv_max']:.1f}",
                f"{mean_peval:.2f}" if mean_peval is not None else "-",
                f"{r['mean_secs']:.1f}",
            )
        )

    widths = [len(h) for h in header]
    for row in body:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    def fmt_row(row):
        return "  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row))

    lines.append(fmt_row(header))
    for row in body:
        lines.append(fmt_row(row))

    return "\n".join(lines)


def main():
    if len(sys.argv) != 2:
        print("usage: sweep_summary.py <label>", file=sys.stderr)
        return 1

    label = sys.argv[1]
    combo_rows = summarize(label)
    print(format_table(combo_rows))
    return 0


if __name__ == "__main__":
    sys.exit(main())
