# Persistence, transaction boundaries and recovery

## Stores

| store | owner | contents |
| --- | --- | --- |
| `state/runs/<run>/run.db` | executor | run status and control flags, goal revisions, plans, actions, events, questions, human facts, observations (evidence), planner-call accounting, **inventory record** (ledger, reservations, idempotency records) |
| `state/runs/<run>/world.db` | simulator adapter | locations, objects, effect receipts (keyed by operation key), effect counters, armed/fired faults, harness log |
| `state/runs/<run>/executor.lock` | OS file lock | exclusive executor ownership; released by the OS if the process dies |

SQLite runs in WAL mode with `synchronous=FULL`. No transaction is held across
a planner request or an adapter call.

## Transaction boundaries

1. **Claim** (`RunStore::claim`, one `BEGIN IMMEDIATE`): refuse if a pause or
   cancel is in force; supersede the action if its goal revision is no longer
   current; refuse if another action with the same logical effect has an
   unestablished outcome; assign or reuse the operation key; set
   `dispatch_recorded`. **The operation key is persisted before the adapter is called.**
2. **Effect** (simulator, one `BEGIN IMMEDIATE` in `world.db`): key uniqueness
   and argument-fingerprint check, adapter-side preconditions, world mutation,
   receipt, effect counter, and any fault/trigger consumption, committed together.
   Reusing a key with different arguments is rejected with no effect.
3. **Result** (executor, separate write in `run.db`): acknowledgement, then
   verification by observation, then `succeeded`.
4. **Inventory operation** (one `BEGIN IMMEDIATE` in `run.db`): idempotency
   record + ledger rows + reservation update together. The action's status is a
   separate write, so the same status-lookup recovery applies.
5. **Goal correction** (one `BEGIN IMMEDIATE`): new revision + current pointer +
   obsolete open questions + event. Answering a substitution/rework question
   commits the answer and the resulting revision in the same transaction,
   against the goal as it is at that moment.
6. **Terminal transition** (`finish_run`, one `BEGIN IMMEDIATE`): the final
   status and report are written only if the goal revision and control flag are
   still the ones the executor finished against; otherwise nothing is written
   and the loop handles the correction, pause or cancel.

Steps 2 and 3 are deliberately separate so a crash can leave an effect that the
executor has not recorded. That gap is what the crash tests exercise.

## Action state machine

```
pending ──claim──► dispatch_recorded ──adapter: applied──► acknowledged ──observed──► succeeded
   │                    │   │                                   │
   │                    │   └─adapter: rejected / unavailable───┴─► failed (known: no effect)
   │                    └─timeout / crash────────────────────────────► outcome_unknown
   │                                                                     │ reconcile
   ├─preconditions false, postconditions true ─► satisfied               ├─► succeeded (receipt + observation, or observation)
   ├─goal revised / plan replaced ─► superseded                          ├─► failed (rejected receipt)
   ├─cancel ─► cancelled                                                 ├─► retry_pending (no receipt: re-dispatch SAME key)
   └─policy denial at dispatch ─► denied                                 └─► stays outcome_unknown → run BLOCKED
```

"In flight" = `dispatch_recorded` without a result: the claim committed, and
the adapter may or may not have been called.

## Reconciliation (`runtime.rs::reconcile_one`)

Run at every loop iteration before anything new is dispatched, and immediately
after a timeout:

1. **Status lookup** by operation key (contracts declare support).
   - applied → verify postconditions by observation → `succeeded`
     (if observation is withheld: `acknowledged`, run blocked until verifiable);
   - rejected → `failed` (no effect);
   - not found → no effect under this key → `retry_pending` with the **same key**
     if the action is still part of the active plan, the contract has an
     idempotency key and attempts remain (the adapter guarantees at most one
     effect per key); otherwise `failed` (the key is retired only on this
     confirmed status); or blocked if the step says `on_unknown_outcome: block`.
2. **Status unavailable** → observe. Postconditions hold → `succeeded`
   (attributed by observation). Effect clearly absent and same-key retry is safe
   → `retry_pending`. Otherwise → remains `outcome_unknown` and the run is
   **blocked**, with the reason stating what evidence is missing.

Effects are verified only against observable conditions (a goal revision is
not an effect). When a plan is superseded, its pending same-key retries return
to `outcome_unknown` and are settled by a status lookup before their key can be
retired. Runtime bookkeeping actions left unsettled by a crash are reconciled
the same way. Recovery never resets the world, never marks in-flight work
failed wholesale, and never replays an effect without establishing its outcome.

Loop order on every iteration: reconcile → cancel → pause → budgets → questions
→ goal revision → plan → next step. Budget exhaustion therefore never skips
reconciliation, and a cancel always ends as `stopped`.

## Operation identity across plans and goal revisions

Key = `op-` + H(run id, logical effect descriptor, occurrence), where the
descriptor is the capability plus canonical resolved arguments.

- A retry of the same action reuses its key.
- A new plan (after a replan or goal revision) that contains the same logical
  effect gets the same descriptor. If an earlier action with that descriptor is
  unsettled, the claim is refused (`SameEffectUnsettled`) until reconciliation
  settles it. If all earlier ones are settled, the occurrence counter increments,
  giving a new key for a genuinely new effect (e.g. picking an object again
  after it was returned to stock).
- Goal revisions do not change keys; reconciliation (not the revision number)
  decides whether an old effect happened.
- v0 simulator requests are synchronous, so "not found" after the caller's
  timeout is final. A real adapter would need a request-expiry guarantee before
  a key may be retired (see `docs/decisions.md`).

## Goal corrections

1. The correction commits a new revision (UI/CLI, any process).
2. Claims from the old revision are refused from that moment (claim checks the
   revision inside its transaction; tested by
   `tests/corrections.rs::concurrent_correction_cannot_slip_between_check_and_claim`).
3. An action claimed before the correction is in flight and is settled by
   reconciliation first.
4. The executor observes; completed effects are preserved (they are the world).
5. Pending steps of the old plan are marked `superseded`.
6. The planner is asked for a plan against the new revision and observation.
7. That plan is validated and authorised like any other. A planner reply that
   arrives for an obsolete revision is stored as `stale` and never executed.

Kit verification always uses the current revision's composition. Excess parts
after a composition change require an explicit human decision (rework question);
declined rework leaves the tray reported as non-conforming.

## Cancel and pause

Pause: no new claims; in-flight outcomes are established; status `paused`.
Cancel: pending actions are cancelled, in-flight outcomes are established, the
executor records consumption for parts observed in trays, releases
reservations, writes a verified report and acknowledges with status `stopped`.
Effects that already happened remain. These are logical controls, not a
safety-rated stop.

## Inventory accounting model

`on_hand` = known parts not yet recorded as consumed, wherever they are (stock
bin, gripper, or a tray before the consumption record). Consumption is recorded
only after the part is observed in a tray, one evidence object per record, and
never twice for the same object (checked inside the ledger transaction); rework
returns re-add it and require the object to be recorded as consumed. A consumed
part seen back in its own bin (an interrupted rework) is recorded as returned
before it is reused. `reserved`
≤ `on_hand` is a CHECK constraint. Before each planning round, the executor
compares the record with the observation and records an evidence-bound
`adjust_count` for any difference (citing the world revision). At finalisation
it does not auto-adjust; remaining differences are reported (and the evaluator
counts them).

## Invariants and where they are enforced

| invariant | enforcement |
| --- | --- |
| each object in exactly one place | schema: one `location_id` column; unique partial index on the gripper |
| objects not created/destroyed by pick/place | adapter only moves rows; evaluator checks counts |
| consumption matches observed placement | evidence check in the gateway; at most one consume per object (evaluator) |
| reservations ≤ on_hand; released at the end | CHECK constraint; finalize/cancel release; evaluator |
| a reported complete kit contains its parts | verifier uses observation; evaluator uses truth |
| completed effects survive a goal revision | no reset path exists; replans start from observation |
| unresolved unknown outcomes block replay | reconcile before dispatch; `SameEffectUnsettled` |
| rejected plans have no effects | rejected proposals are never materialised; evaluator checks |
