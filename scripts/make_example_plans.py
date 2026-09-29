#!/usr/bin/env python3
"""Write the example Action IR plans in examples/plans/.

kit-assembly.json is the reference plan for the `nominal` scenario (3 kits of
1 red block, 1 blue block, 2 washers). It is spelled out step by step here
rather than produced by the scripted planner, and it demonstrates typed output
references: every `place` takes its object from the preceding `pick`, and every
`consume`/`release` takes the reservation id from `reserve-stock`.

The invalid-*.json variants each break one rule, so `revolve validate` shows the
diagnostic category (syntax / schema / structure / stale / authorization /
budget / feasibility). They are also used by the integration tests.
"""
import copy
import json
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "examples", "plans")


def ref(step, out):
    return {"$ref": f"{step}.{out}"}


def build():
    steps = []

    def add(step):
        if steps:
            step.setdefault("depends_on", [steps[-1]["step_id"]])
        steps.append(step)

    add({
        "step_id": "reserve-stock",
        "capability": "inventory.reserve",
        "capability_version": "1",
        "arguments": {"counts": {"red_block": 3, "blue_block": 3, "washer": 6}},
    })
    kits = [
        ("tray-1", ["red-01", "blue-01", "washer-01", "washer-02"]),
        ("tray-2", ["red-02", "blue-02", "washer-03", "washer-04"]),
        ("tray-3", ["red-03", "blue-03", "washer-05", "washer-06"]),
    ]
    kind_of = {"red": ("red_block", "bin-red"), "blue": ("blue_block", "bin-blue"), "washer": ("washer", "bin-washer")}
    for tray, objs in kits:
        for o in objs:
            kind, bin_ = kind_of[o.split("-")[0]]
            pick_id = f"pick-{o}"
            place_id = f"place-{o}-in-{tray}"
            add({
                "step_id": pick_id,
                "capability": "workspace.pick",
                "capability_version": "1",
                "arguments": {"object_id": o, "source_id": bin_},
                "preconditions": [
                    {"kind": "gripper_empty"},
                    {"kind": "object_at", "object_id": o, "location_id": bin_},
                ],
                "postconditions": [{"kind": "gripper_holds", "object_id": o}],
                "on_unknown_outcome": "reconcile_before_retry",
            })
            add({
                "step_id": place_id,
                "capability": "workspace.place",
                "capability_version": "1",
                "arguments": {"object_id": ref(pick_id, "object_id"), "destination_id": tray},
                "preconditions": [
                    {"kind": "gripper_holds", "object_id": o},
                    {"kind": "destination_available", "destination_id": tray},
                ],
                "postconditions": [{"kind": "object_at", "object_id": o, "location_id": tray}],
                "on_unknown_outcome": "reconcile_before_retry",
            })
            add({
                "step_id": f"consume-{o}",
                "capability": "inventory.consume",
                "capability_version": "1",
                "depends_on": [place_id, "reserve-stock"],
                "arguments": {
                    "reservation_id": ref("reserve-stock", "reservation_id"),
                    "part_kind": kind,
                    "quantity": 1,
                    "evidence_object_id": o,
                },
                "preconditions": [{"kind": "object_at", "object_id": o, "location_id": tray}],
            })
    add({
        "step_id": "release-unused",
        "capability": "inventory.release",
        "capability_version": "1",
        "arguments": {"reservation_id": ref("reserve-stock", "reservation_id")},
    })
    caps = {s["capability"]: "1" for s in steps}
    return {
        "schema_version": "revolve.action-ir/v1",
        "plan_id": "handwritten-nominal-3-kits",
        "goal_id": "goal-nominal",
        "goal_revision": 1,
        "basis": {"world_revision": 0, "max_observation_age_ms": 5000},
        "capabilities": caps,
        "requested_permissions": sorted(caps),
        "budgets": {"max_actions": 60, "max_planner_rounds": 1, "max_elapsed_ms": 60000},
        "kit_assignments": [{"kit": 1, "tray_id": "tray-1"}, {"kit": 2, "tray_id": "tray-2"}, {"kit": 3, "tray_id": "tray-3"}],
        "explanation": "Reserve 3 red, 3 blue, 6 washers; fill tray-1..3 one part at a time, recording consumption after each verified placement; release anything unused.",
        "expected_outcome": {"kits_complete": ["tray-1", "tray-2", "tray-3"], "kits_incomplete": []},
        "steps": steps,
    }


def main():
    os.makedirs(OUT, exist_ok=True)
    good = build()
    variants = {"kit-assembly": good}

    v = copy.deepcopy(good)
    v["steps"][0]["depends_on"] = ["release-unused"]
    variants["invalid-cycle"] = v

    v = copy.deepcopy(good)
    v["steps"][2]["arguments"]["destination_id"] = "outside"
    variants["invalid-unauthorized-destination"] = v

    v = copy.deepcopy(good)
    v["requested_permissions"].append("system.shell")
    variants["invalid-ungranted-permission"] = v

    v = copy.deepcopy(good)
    v["granted_permissions"] = ["workspace.place"]
    variants["invalid-runtime-owned-field"] = v

    v = copy.deepcopy(good)
    v["steps"][1]["capability"] = "workspace.teleport"
    variants["invalid-unknown-capability"] = v

    v = copy.deepcopy(good)
    v["steps"][3]["arguments"]["reservation_id"] = ref("no-such-step", "reservation_id")
    variants["invalid-missing-reference"] = v

    v = copy.deepcopy(good)
    v["goal_revision"] = 7
    variants["invalid-stale-revision"] = v

    v = copy.deepcopy(good)
    v["steps"][1]["arguments"]["object_id"] = "red-09"
    v["steps"][1]["preconditions"][1]["object_id"] = "red-09"
    v["steps"][1]["postconditions"][0]["object_id"] = "red-09"
    variants["invalid-infeasible-object"] = v

    v = copy.deepcopy(good)
    v["expected_outcome"]["kits_complete"].append("tray-4")
    variants["invalid-false-claim"] = v

    v = copy.deepcopy(good)
    v["budgets"]["max_actions"] = 10
    variants["invalid-over-budget"] = v

    v = copy.deepcopy(good)
    # Two picks not ordered by a dependency: both need the gripper.
    v["steps"][4]["depends_on"] = ["reserve-stock"]
    variants["invalid-resource-conflict"] = v

    for name, plan in variants.items():
        with open(os.path.join(OUT, name + ".json"), "w", encoding="utf-8", newline="\n") as fh:
            json.dump(plan, fh, indent=2)
            fh.write("\n")
    with open(os.path.join(OUT, "invalid-syntax.json"), "w", encoding="utf-8", newline="\n") as fh:
        fh.write('{"schema_version": "revolve.action-ir/v1", "steps": [\n')
    print(f"wrote {len(variants) + 1} plan(s) to {OUT}")


if __name__ == "__main__":
    main()
