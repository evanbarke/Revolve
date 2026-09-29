#!/usr/bin/env python3
"""Compare two benchmark result directories trial by trial (e.g. compiled vs
interpreted engine): every verdict field except timing and ids, the exact
sequence of actions (capability, arguments, status, result) and the final
simulated world. Exit code 1 if anything differs.

usage: compare_engines.py <state-dir> <results-dir-A> <results-dir-B>
"""
import json
import re
import sqlite3
import sys
from pathlib import Path

IGNORE = {"run_id", "engine", "wall_ms", "peak_rss_bytes", "recovery_ms"}


def verdicts(d):
    out = {}
    for line in (Path(d) / "raw.jsonl").read_text(encoding="utf-8").splitlines():
        v = json.loads(line)
        out[(v["scenario_id"], v["arm"], v["trial"])] = v
    return out


# Ids derived from the run id (operation keys, reservations, receipts) differ between
# any two runs. They are renamed consistently within each run, in order of first
# appearance, so a reused id must still line up.
RUN_IDS = re.compile(r"\b(op|res|rcpt|ev)-[0-9a-f]{8,}\b")


def normalize(rows):
    names = {}

    def rename(m):
        return names.setdefault(m.group(0), f"{m.group(1)}#{len(names) + 1}")

    return json.loads(RUN_IDS.sub(rename, json.dumps(rows)))


def actions(state, run_id):
    db = sqlite3.connect(Path(state) / "runs" / run_id / "run.db")
    rows = db.execute(
        "SELECT origin, plan_seq, step_id, capability, resolved_arguments, status, result, error "
        "FROM actions ORDER BY action_id"
    ).fetchall()
    return normalize([
        [o, p, s, c, json.loads(a) if a else None, st, json.loads(r) if r else None, e]
        for (o, p, s, c, a, st, r, e) in rows
    ])


def world(state, run_id):
    db = sqlite3.connect(Path(state) / "runs" / run_id / "world.db")
    tables = [t for (t,) in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='objects'")]
    if not tables:
        return None
    return sorted(db.execute("SELECT * FROM objects").fetchall())


def main():
    state, a_dir, b_dir = sys.argv[1:4]
    a, b = verdicts(a_dir), verdicts(b_dir)
    problems = []
    if set(a) != set(b):
        problems.append(f"different trials: {sorted(set(a) ^ set(b))}")
    same = 0
    for k in sorted(set(a) & set(b)):
        va, vb = a[k], b[k]
        diff = [f for f in va if f not in IGNORE and va.get(f) != vb.get(f)]
        if diff:
            problems.append(f"{k}: verdict differs in {diff}")
            continue
        aa, ab = actions(state, va["run_id"]), actions(state, vb["run_id"])
        if aa != ab:
            first = next((i for i, (x, y) in enumerate(zip(aa, ab)) if x != y), min(len(aa), len(ab)))
            problems.append(f"{k}: action sequences differ at #{first} ({len(aa)} vs {len(ab)} actions)")
            continue
        if world(state, va["run_id"]) != world(state, vb["run_id"]):
            problems.append(f"{k}: final world differs")
            continue
        same += 1
    print(f"{same}/{len(set(a) | set(b))} trials identical (verdict, action sequence, final world)")
    for p in problems:
        print("  DIFF", p)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
