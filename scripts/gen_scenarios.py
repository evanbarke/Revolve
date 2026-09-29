#!/usr/bin/env python3
"""Generate Revolve scenario fixtures deterministically.

    python scripts/gen_scenarios.py            # writes scenarios/{demo,development,held-out}

Each scenario states its authored expectation (terminal outcome and number of
kits that should truly be complete). The evaluator checks runs against
simulator truth, so an expectation mistake shows up as a failed trial to
investigate, never as a silently passing one.

Faults and human actions are triggered on world events ("the nth applied
effect of a capability"), so both benchmark arms face identical conditions
regardless of how many planner calls they make.
"""
import json
import os
import random

ROOT = os.path.join(os.path.dirname(__file__), "..", "scenarios")

CANON = "Prepare three kits. Each kit needs one red block, one blue block, and two washers. " \
        "Update inventory as you use parts. Tell me what is missing and ask before making substitutions."


def goal(q, comp, text=None):
    g = {"kit_quantity": q, "kit_components": comp}
    if text:
        g["source_text"] = text
    return g


def per_kit(comp):
    return sum(comp.values())


def enough(q, comp, spare=1):
    stock = {k: q * n + spare for k, n in comp.items()}
    for k in ["red_block", "blue_block", "washer", "green_block"]:
        stock.setdefault(k, 0)
    return stock


def trig(cap, n):
    return {"capability": cap, "nth": n}


def sc(sid, family, desc, world, g, expect_outcome, expect_kits, faults=None, human=None, record=None, seed=1, note=None):
    d = {
        "id": sid,
        "family": family,
        "description": desc,
        "seed": seed,
        "world": world,
        "goal": g,
        "faults": faults or [],
        "human": human or {},
        "expect": {"outcome": expect_outcome, "kits_complete": expect_kits},
    }
    if record is not None:
        d["inventory_record"] = record
    if note:
        d["expect"]["note"] = note
    return d


STD = {"red_block": 1, "blue_block": 1, "washer": 2}


def family_builders():
    """Return {family: builder(q, comp, rng, variant) -> scenario dict}."""

    def nominal(sid, q, comp, rng, trays=4):
        return sc(sid, "nominal", f"{q} kit(s), enough stock", {"stock": enough(q, comp, rng.randint(0, 2)), "trays": trays},
                  goal(q, comp), "completed", q)

    def shortage(sid, q, comp, rng, trays=4):
        stock = enough(q, comp, 1)
        short_kind = rng.choice(sorted(comp.keys()))
        # Exactly enough for q-1 kits of the short kind (and nothing to substitute).
        stock[short_kind] = comp[short_kind] * (q - 1)
        stock["green_block"] = 0
        return sc(sid, "shortage", f"{q} kits requested; {short_kind} only suffices for {q-1}", {"stock": stock, "trays": trays},
                  goal(q, comp), "partial", q - 1, note=f"missing {comp[short_kind]} {short_kind}")

    def substitution(sid, q, comp, rng, approve, trays=4):
        stock = enough(q, comp, 1)
        stock["blue_block"] = comp["blue_block"] * (q - 1)
        stock["green_block"] = comp["blue_block"] + 1
        return sc(sid, "substitution", f"blue short by {comp['blue_block']}; green available; human {'approves' if approve else 'declines'} substitution",
                  {"stock": stock, "trays": trays}, goal(q, comp), "completed" if approve else "partial", q if approve else q - 1,
                  human={"answers": {"substitution": "approve" if approve else "decline"}})

    def record_mismatch(sid, q, comp, rng, trays=4):
        stock = enough(q, comp, 0)
        record = dict(stock)
        record["red_block"] = stock["red_block"] + rng.randint(1, 2)
        return sc(sid, "record-mismatch", "inventory record overstates red blocks; observation must win, with an evidence-bound adjustment",
                  {"stock": stock, "trays": trays}, goal(q, comp), "completed", q, record=record)

    def disturbance(sid, q, comp, rng, trays=4):
        stock = enough(q, comp, 0)
        # Remove a red block that the plan is counting on, after the first place.
        victim = f"red-{stock['red_block']:02d}"
        return sc(sid, "record-mismatch", "an operator removes a stocked red block after planning; the record and the plan are now wrong",
                  {"stock": stock, "trays": trays}, goal(q, comp), "partial", q - 1,
                  faults=[{"kind": "remove_object", "on": trig("workspace.place", 1), "object_id": victim}])

    def destination(sid, q, comp, rng, trays=None):
        pk = per_kit(comp)
        trays = trays or q + 1
        return sc(sid, "destination-unavailable", "tray-2 becomes unavailable after planning, before any part reaches it",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q,
                  faults=[{"kind": "destination_unavailable", "on": trig("workspace.place", pk), "location_id": "tray-2"}])

    def goal_change(sid, q, comp, rng, trays=4):
        new_q = max(1, q - 1)
        new_comp = dict(comp)
        new_comp["washer"] = max(1, comp.get("washer", 1))
        at = rng.randint(1, per_kit(comp) - 1)
        return sc(sid, "goal-change", f"human changes the goal to {new_q} kit(s) while place #{at} is in flight",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", new_q,
                  human={"actions": [{"do": "correct", "on": trig("workspace.place", at), "goal": goal(new_q, new_comp)}]})

    def goal_change_rework(sid, q, comp, rng, approve, trays=4):
        pk = per_kit(comp)
        new_comp = dict(comp)
        new_comp["washer"] = comp["washer"] - 1
        return sc(sid, "goal-change", f"after kit 1 is complete, the composition drops one washer; rework {'approved' if approve else 'declined'}",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp),
                  "completed" if approve else "partial", q if approve else q - 1,
                  human={"answers": {"rework": "approve" if approve else "decline"},
                         "actions": [{"do": "correct", "on": trig("workspace.place", pk + 1), "goal": goal(q, new_comp)}]},
                  note="declined rework leaves kit 1 non-conforming (excess washer) and the partial kit 2 with its extra part")

    def crash_after(sid, q, comp, rng, cap="workspace.place", trays=4):
        n = rng.randint(1, q * per_kit(comp) - 1)
        return sc(sid, "crash-after-effect", f"executor process dies after {cap} #{n} commits, before recording it",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q,
                  faults=[{"kind": "crash_after_effect", "on": trig(cap, n)}])

    def crash_before(sid, q, comp, rng, trays=4):
        n = rng.randint(2, q * per_kit(comp))
        return sc(sid, "crash-after-effect", f"executor process dies after recording dispatch of pick #{n}, before the effect",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q,
                  faults=[{"kind": "crash_before_effect", "on": trig("workspace.pick", n)}])

    def timeout(sid, q, comp, rng, after, trays=4):
        n = rng.randint(1, q * per_kit(comp))
        kind = "timeout_after_effect" if after else "timeout_before_effect"
        return sc(sid, "unknown-outcome", f"pick #{n} times out; the effect {'did' if after else 'did not'} happen",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q,
                  faults=[{"kind": kind, "on": trig("workspace.pick", n)}])

    def ambiguous(sid, q, comp, rng, restore, trays=4):
        pk = per_kit(comp)
        n = rng.randint(2, q * pk)
        kits_before = (n - 1) // pk
        return sc(sid, "unknown-outcome", f"pick #{n} times out and both status and observation are withheld; "
                  + ("a human restores evidence" if restore else "nobody restores evidence"),
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp),
                  "completed" if restore else "blocked", q if restore else kits_before,
                  faults=[{"kind": "timeout_after_effect", "on": trig("workspace.pick", n), "blackout": True}],
                  human={"restore_evidence_when_blocked": restore})

    def planner_down(sid, q, comp, rng, needs_replan, trays=None):
        pk = per_kit(comp)
        trays = trays or q + 1
        faults = [{"kind": "planner_unavailable", "on": trig("workspace.place", 1)}]
        desc = "planner connection lost after execution starts"
        if needs_replan:
            faults.append({"kind": "destination_unavailable", "on": trig("workspace.place", pk), "location_id": "tray-2"})
            desc += "; later a tray becomes unavailable so a new plan is needed"
        return sc(sid, "planner-unavailable", desc, {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q,
                  faults=faults, human={"restore_planner_when_waiting": True})

    def unauthorized(sid, q, comp, rng, trays=4):
        on = None if rng.random() < 0.5 else trig("workspace.place", rng.randint(1, per_kit(comp)))
        f = {"kind": "inject_unauthorized_action"}
        if on:
            f["on"] = on
        return sc(sid, "unauthorized-proposal", "a planner decision requests a placement outside the granted locations",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "completed", q, faults=[f])

    def classification(sid, q, comp, rng, usable, trays=4):
        stock = enough(q, comp, 0)
        stock["washer"] = comp["washer"] * q - 1
        return sc(sid, "unknown-classification", f"one washer short unless an unclassified item (truly {'a washer' if usable else 'scrap'}) is usable",
                  {"stock": stock, "unknown_items": [{"bin": "bin-washer", "true_kind": "washer" if usable else "scrap"}], "trays": trays},
                  goal(q, comp), "completed" if usable else "partial", q if usable else q - 1)

    def cancel(sid, q, comp, rng, trays=4):
        pk = per_kit(comp)
        n = rng.randint(1, q * pk - 1)
        return sc(sid, "cancel", f"human cancels while place #{n} is in flight",
                  {"stock": enough(q, comp, 1), "trays": trays}, goal(q, comp), "stopped", n // pk,
                  human={"actions": [{"do": "cancel", "on": trig("workspace.place", n)}]})

    return locals()


def main():
    b = family_builders()
    rng = random.Random(20260929)
    out = {"demo": [], "development": [], "held-out": []}

    # --- demo: one per CLI name, canonical request
    std_goal_text = CANON
    def canon(s):
        s["goal"]["source_text"] = std_goal_text
        return s
    demo = [
        canon(b["nominal"]("nominal", 3, STD, random.Random(1))),
        canon(b["shortage"]("shortage", 3, STD, random.Random(3))),
        canon(b["substitution"]("substitution-approved", 3, STD, random.Random(2), True)),
        canon(b["record_mismatch"]("record-mismatch", 3, STD, random.Random(4))),
        canon(b["disturbance"]("disturbance", 3, STD, random.Random(5))),
        canon(b["destination"]("destination-unavailable", 3, STD, random.Random(6))),
        canon(b["goal_change"]("goal-change", 3, STD, random.Random(7))),
        canon(b["goal_change_rework"]("goal-change-rework", 3, STD, random.Random(8), True)),
        canon(b["crash_after"]("crash-after-effect", 3, STD, random.Random(9))),
        canon(b["crash_before"]("crash-before-effect", 3, STD, random.Random(10))),
        canon(b["timeout"]("timeout-unknown", 3, STD, random.Random(11), True)),
        canon(b["ambiguous"]("unknown-outcome-blocked", 3, STD, random.Random(12), False)),
        canon(b["ambiguous"]("unknown-outcome-restored", 3, STD, random.Random(12), True)),
        canon(b["planner_down"]("planner-unavailable", 3, STD, random.Random(13), True)),
        canon(b["unauthorized"]("unauthorized-proposal", 3, STD, random.Random(14))),
        canon(b["classification"]("unknown-classification", 3, STD, random.Random(15), True)),
        canon(b["cancel"]("cancel", 3, STD, random.Random(16))),
    ]
    out["demo"] = demo

    comps = [STD, {"red_block": 2, "blue_block": 1, "washer": 1}, {"red_block": 1, "blue_block": 2, "washer": 2},
             {"red_block": 1, "blue_block": 1, "washer": 1}, {"red_block": 1, "blue_block": 1, "washer": 3}]

    # --- development fixtures (used while building; small)
    dev_specs = [
        ("nominal", {}), ("shortage", {}), ("substitution", {"approve": False}), ("record_mismatch", {}),
        ("destination", {}), ("goal_change", {}), ("goal_change_rework", {"approve": False}), ("crash_after", {}),
        ("timeout", {"after": False}), ("ambiguous", {"restore": True}), ("planner_down", {"needs_replan": False}),
        ("unauthorized", {}), ("classification", {"usable": False}), ("cancel", {}),
    ]
    for i, (fam, kw) in enumerate(dev_specs):
        q = 2
        comp = STD
        s = b[fam](f"dev-{i+1:02d}-{fam.replace('_', '-')}", q, comp, random.Random(100 + i), **kw)
        s["seed"] = 100 + i
        out["development"].append(s)

    # --- held-out set: ~30 configurations across families, varied q/composition/triggers/seeds
    held_specs = [
        ("nominal", {}), ("nominal", {}), ("nominal", {}),
        ("shortage", {}), ("shortage", {}), ("shortage", {}),
        ("substitution", {"approve": True}), ("substitution", {"approve": False}),
        ("record_mismatch", {}), ("record_mismatch", {}), ("disturbance", {}),
        ("destination", {}), ("destination", {}),
        ("goal_change", {}), ("goal_change", {}), ("goal_change_rework", {"approve": True}),
        ("crash_after", {}), ("crash_after", {"cap": "workspace.pick"}), ("crash_before", {}),
        ("timeout", {"after": True}), ("timeout", {"after": False}),
        ("ambiguous", {"restore": False}), ("ambiguous", {"restore": True}),
        ("planner_down", {"needs_replan": False}), ("planner_down", {"needs_replan": True}),
        ("unauthorized", {}), ("unauthorized", {}),
        ("classification", {"usable": True}), ("classification", {"usable": False}),
        ("cancel", {}), ("cancel", {}),
    ]
    for i, (fam, kw) in enumerate(held_specs):
        r = random.Random(5000 + i)
        q = r.choice([2, 3, 3, 4])
        comp = r.choice(comps)
        if fam in ("substitution",):
            comp = r.choice([STD, comps[2], comps[3]])
        if fam in ("goal_change_rework",) and comp.get("washer", 0) < 2:
            comp = STD
        if fam in ("shortage", "disturbance") and q < 2:
            q = 2
        s = b[fam](f"ho-{i+1:02d}-{fam.replace('_', '-')}", q, comp, r, **kw)
        s["seed"] = 5000 + i
        out["held-out"].append(s)

    # --- playground: free-form tables for programs (not part of the benchmark).
    # The request comes with the program; the goal below is a placeholder.
    play_goal = goal(1, {"red_block": 1}, "(playground: the request comes with the program)")
    empty = {"red_block": 0, "blue_block": 0, "green_block": 0, "washer": 0}
    messy = {"red_block": 3, "blue_block": 2, "green_block": 2, "washer": 3}
    scrap = [{"bin": "table", "true_kind": "scrap"}]

    def play(sid, desc, stock, table, unknown=None, trays=4, faults=None):
        world = {"stock": stock, "table": table, "unknown_items": unknown or [], "trays": trays}
        return sc(sid, "playground", desc, world, play_goal, "completed", 0, faults=faults,
                  note="playground: judged by the program's own expectations, checked by observation")

    out["playground"] = [
        play("messy-table", "A messy table: 3 red, 2 blue and 2 green blocks, 3 washers and 1 item the camera can't identify. Bins and 4 trays are empty.",
             empty, messy, scrap),
        play("workshop", "Bins stocked with 4 of each block and 6 washers, a small pile on the table, 4 empty trays.",
             {"red_block": 4, "blue_block": 4, "green_block": 4, "washer": 6}, {"red_block": 1, "blue_block": 1, "washer": 2}),
        play("messy-table-power-cut", "The messy table; the power fails right after the 4th part is placed.",
             empty, messy, scrap, faults=[{"kind": "crash_after_effect", "on": trig("workspace.place", 4)}]),
        play("messy-table-thief", "The messy table; someone takes a red block off the table after the 2nd part is placed.",
             empty, messy, scrap, faults=[{"kind": "remove_object", "on": trig("workspace.place", 2), "object_id": "red-03"}]),
    ]

    for sub, items in out.items():
        d = os.path.join(ROOT, sub)
        os.makedirs(d, exist_ok=True)
        for f in os.listdir(d):
            if f.endswith(".json"):
                os.remove(os.path.join(d, f))
        for s in items:
            with open(os.path.join(d, s["id"] + ".json"), "w", encoding="utf-8", newline="\n") as fh:
                json.dump(s, fh, indent=2)
                fh.write("\n")
        print(f"{sub}: {len(items)} scenario(s)")


if __name__ == "__main__":
    main()
