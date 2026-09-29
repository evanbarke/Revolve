# Action IR v1 and capability contracts

Schema id: `revolve.action-ir/v1`. Types: `src/ir.rs`. Validation: `src/compiler.rs`.
Reference plan: `examples/plans/kit-assembly.json` (compiles against scenario
`nominal`). Every `examples/plans/invalid-*.json` breaks exactly one rule and is
covered by `tests/compiler_diagnostics.rs`.

## Semantics

- A plan is a DAG of steps. Dispatch is **sequential** in a deterministic
  topological order; ties follow list order. There is no parallel scheduler.
- Steps that use the same exclusive resource (the gripper, an object, a
  reservation) must be connected by a dependency path; otherwise the plan is
  rejected (`structure`).
- Argument values are literals or typed output references
  `{"$ref": "<step_id>.<output>"}`. A reference must name a transitive
  dependency, an output declared by that step's contract, and a type equal to
  the argument's declared type.
- Preconditions are evaluated against a fresh observation immediately before
  dispatch. If they fail but the step's postconditions already hold, the step is
  marked `satisfied` (not dispatched); otherwise it fails with no effect and the
  plan is replaced.
- Postconditions (declared plus those implied by the contract) are verified by
  observation after the adapter acknowledges an effect. An acknowledgement alone
  is never treated as proof.
- The whole document is an untrusted proposal. Runtime-owned facts live outside
  it: grants, operation keys, receipts, identities, timestamps.

## Plan fields

| field | meaning |
| --- | --- |
| `schema_version` | must be `revolve.action-ir/v1` |
| `plan_id` | planner-chosen label (not an identity the runtime relies on) |
| `goal_id`, `goal_revision` | must equal the run's goal id and **current** revision (`stale` otherwise) |
| `basis.world_revision` | world revision of the observation the planner was given (`stale` if different) |
| `basis.max_observation_age_ms` | informational in v0; the executor observes before every step |
| `capabilities` | capability id → contract version for every capability used |
| `requested_permissions` | capability ids; each must be granted and every used one requested |
| `budgets` | `max_actions`, `max_planner_rounds`, `max_elapsed_ms`; effective = min(plan, runtime limit) |
| `kit_assignments` | `[{kit, tray_id}]`; trays distinct, kits within 1..=quantity |
| `explanation` | short human-readable intent (displayed; never interpreted) |
| `expected_outcome` | `kits_complete: [tray]`, `kits_incomplete: [{kit, tray_id, missing, reason}]` — the compiler checks the complete set equals what the plan actually builds |
| `steps` | see below |

## Step fields

| field | meaning |
| --- | --- |
| `step_id` | unique, `[A-Za-z0-9_-]{1,80}` |
| `capability`, `capability_version` | a registered contract usable as a plan step |
| `depends_on` | step ids |
| `arguments` | object; exactly the contract's arguments |
| `preconditions`, `postconditions` | condition variants (below) |
| `on_unknown_outcome` | `reconcile_before_retry` (default) or `block` |
| `retry.max_attempts` | attempts under the **same** operation key (default 2, runtime cap 3) |
| `note` | optional text |

Condition variants (`kind`): `object_at {object_id, location_id}`,
`gripper_empty`, `gripper_holds {object_id}`, `location_available {location_id}`
(alias `destination_available {destination_id}`),
`quantity_at_least {location_id, part_kind, quantity}`,
`goal_revision_equals {revision}`. There is no expression language, eval, SQL or shell.

Example step (from `examples/plans/kit-assembly.json`):

```json
{
  "step_id": "place-red-01-in-tray-1",
  "capability": "workspace.place",
  "capability_version": "1",
  "depends_on": ["pick-red-01"],
  "arguments": {"object_id": {"$ref": "pick-red-01.object_id"}, "destination_id": "tray-1"},
  "preconditions": [
    {"kind": "gripper_holds", "object_id": "red-01"},
    {"kind": "destination_available", "destination_id": "tray-1"}
  ],
  "postconditions": [{"kind": "object_at", "object_id": "red-01", "location_id": "tray-1"}],
  "on_unknown_outcome": "reconcile_before_retry"
}
```

## Capability contracts (v1)

Printed by `revolve capabilities` (or `--json`) from `src/capability.rs`.

| id | class | idem. key | status lookup | completion | plan step | effect |
| --- | --- | --- | --- | --- | --- | --- |
| `workspace.observe` | read-only | – | – | immediate | no | none; may withhold locations |
| `workspace.pick {object_id, source_id}` → `object_id` | physical, uncertain | yes | yes | ack → observe | yes | object → gripper |
| `workspace.place {object_id, destination_id}` → `object_id` | physical, uncertain | yes | yes | ack → observe | yes | object → tray, or its own kind's bin |
| `inventory.reserve {counts}` → `reservation_id` | local transactional | yes | yes | immediate | yes | reserved += counts (≤ on_hand) |
| `inventory.consume {reservation_id, part_kind, quantity, evidence_object_id}` | local transactional | yes | yes | immediate | yes | on_hand, reserved, remaining −= q; evidence must be observed in a tray |
| `inventory.release {reservation_id}` → `released` | local transactional | yes | yes | immediate | yes | reserved −= remaining |
| `inventory.return {part_kind, quantity, evidence_object_id}` | local transactional | yes | yes | immediate | yes | on_hand += q; evidence observed back in its bin (rework) |
| `inventory.adjust_count {part_kind, observed_count, observation_revision}` | local transactional | yes | yes | immediate | no (runtime/agent bookkeeping) | on_hand := count cited from a recorded observation |
| `ops.status {op_key}` | read-only | – | – | immediate | no (agent tool) | none |

Error categories: `precondition_failed`, `key_conflict`, `unavailable`,
`timeout`, `malformed` (workspace); `insufficient_stock`, `unknown_reservation`,
`key_conflict`, `evidence_mismatch`, `malformed` (inventory).

Every contract declaration is matched by adapter behaviour and a test:
idempotent replay and key conflict (`tests/adapters.rs`), status lookup
(`tests/crash_recovery.rs`), evidence binding and bounds (`tests/adapters.rs`).

## Policy (runtime-held grants)

Default grants: all nine capabilities above at version 1. Argument scope: pick
sources and place destinations must be `bin-*` or `tray-*` (never `outside` or
the gripper); reserve ≤ 24 per kind and ≤ 64 total; consume/return 1–6 units;
adjust ≤ 200. Denials are `capability_not_granted`, `version_not_granted`,
`argument_out_of_scope`. The same policy object guards both benchmark arms, at
compile time for Revolve and at every dispatch for both.
