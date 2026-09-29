# Architecture

Revolve v0 is one Rust binary (`revolve`) with a library crate. It executes a
human goal in a simulated kit-assembly workspace through an explicit plan
representation, and it can run the same task through a direct tool-calling
baseline for comparison.

```
 human goal (versioned)          planner (scripted | fixed plan | live model)
        │                                  │  proposal: Action IR JSON (untrusted)
        ▼                                  ▼
 ┌──────────────────────────── executor (runtime.rs) ────────────────────────────┐
 │ observe ─► inventory sync ─► ask planner ─► compile/validate (compiler.rs)      │
 │                                   policy (policy.rs, runtime-held grants)       │
 │ next step: fresh observation ─► preconditions ─► ATOMIC CLAIM (store.rs)        │
 │            ─► gateway.dispatch (gateway.rs: policy, types, evidence)            │
 │            ─► adapter (sim.rs world.db │ inventory.rs run.db)                   │
 │            ─► settle: verify effect by observation, or reconcile unknown        │
 │ finish: verifier.rs report from a fresh observation (never from claims)         │
 └────────────────────────────────────────────────────────────────────────────────┘
        │ persisted state (run.db)                         │ effects + receipts (world.db)
        ▼                                                  ▼
 CLI (main.rs) · UI server (server.rs + web/) · harness (harness.rs) · evaluator (eval.rs)
```

## Boundaries (logical modules, one process)

| Responsibility | Module | Notes |
| --- | --- | --- |
| Intent | `model.rs` (`GoalSpec`), `store.rs` (`goal_revisions`) | Goal is versioned; no permission fields exist on it. |
| World observations | `sim.rs::observe`, `model::Observation` | Synthesised from simulator state, labelled as such. May withhold locations. |
| Capability registry | `capability.rs` | Versioned contracts; retry/reconciliation decisions read these flags. |
| Planner | `planner/` | Returns a proposal, a clarification, or an inability. Never touches stores. |
| Compiler/validator | `compiler.rs` | Parse → schema → structure → binding → authorisation → budget → abstract execution. |
| Policy | `policy.rs` | Grants held by the runtime. Checked at compile time and again at dispatch. |
| Executor | `runtime.rs` | Sole owner of a run (OS file lock). Persists before dispatch. |
| Adapters | `sim.rs` (workspace), `inventory.rs` (ledger) | Each adapter enforces its own preconditions and idempotency. |
| Verifier | `verifier.rs` | Decides what was achieved from a fresh observation and the ledger. |
| Human interface | `main.rs` (CLI), `server.rs` + `web/` (UI) | Both call the same store functions; the UI never schedules. |
| Evaluation harness | `harness.rs`, `eval.rs`, `baseline.rs`, `scenarios/` | Evaluator uses simulator truth; the system under test never does. |

Three kinds of state are kept apart on purpose:

- **Desired state**: the current goal revision (`run.db`).
- **Planner/executor belief**: the observations they are given (`observations` table, plan basis revision).
- **Authoritative state**: the simulator store (`world.db`). Only the adapter,
  the evaluator and the UI's labelled "ground truth" panel read it directly.

## What the compiler actually does

It is a validator plus a scheduler, not a code generator. For a proposal it:

1. parses step by step so diagnostics name the step and JSON path; rejects
   runtime-owned fields (`op_key`, `granted_permissions`, …) with a specific message;
2. checks the IR version, capability ids and contract versions, argument names and types;
3. checks structure: unique ids, known dependencies, cycles (reports one concrete
   cycle), typed output references (`{"$ref": "step.output"}` must name a
   transitive dependency with a matching output type), and that any two steps
   touching the same exclusive resource (gripper, object, reservation) are ordered;
4. binds the plan to the current goal revision and the planning snapshot's world revision;
5. authorises: requested permissions ⊆ grants; every used capability requested;
   literal arguments within scope;
6. enforces budgets (steps vs `min(plan budget, runtime limit)`);
7. abstractly executes every step's contract semantics against the observed
   snapshot and the inventory record: declared preconditions must hold,
   declared postconditions must follow from the contract, stock and reservation
   arithmetic must work, the plan must end with an empty gripper and released
   reservations, and the claimed complete kits must equal what the plan builds.

Output: a deterministic dispatch order (Kahn's algorithm, ties by list order).
A compiled plan is only belief-level: the executor re-checks preconditions on a
fresh observation before each step, and the verifier checks the result.

Diagnostic categories: `syntax`, `schema`, `structure`, `stale`,
`authorization`, `budget`, `feasibility`. Syntax-valid is not meaningful;
meaningful is not authorised; authorised is not feasible; feasible is not done.

## Processes

- `revolve resume --run <id>` is the executor. `demo`, `run`, `benchmark` and
  the UI server all launch it as a child process, so an injected crash is a real
  process death and recovery is a real restart.
- The UI server (`serve`) is loopback-only and stateless apart from child
  handles; every view is read from the stores on each poll.

## Deliberate non-goals in v0

No distributed execution, message bus, vector DB, ROS, plugin marketplace,
physics, perception, or generated code in the executable path. See
`docs/roadmap.md` for where those would enter, and `docs/decisions.md`.
