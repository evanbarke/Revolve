# Benchmark: Revolve vs direct tool calling

## Protocol

**Arms**

- *Revolve*: the planner proposes a whole Action IR plan; the compiler
  validates, authorises and schedules it; the executor claims, dispatches,
  verifies by observation, reconciles unknown outcomes and replans; the verifier
  writes the report.
- *Direct tool calling (baseline)*: the same planner interface chooses **one**
  action per decision (`Planner::next_action`), seeing the goal, tool
  descriptions, the latest observation, the inventory record and its own tool
  history. It can look up operation status and explicitly retry under the same
  operation key. It writes its own final claim.

**Shared by both arms**: goal and revisions, capability contracts, the runtime
policy (checked at every dispatch), the gateway (argument types, evidence
checks for inventory records), adapters, observations, persistence of dispatch
intent before each call, human questions/corrections, cancel cleanup
(record observed consumption, release reservations), budgets, and the
scripted human's answers.

**Revolve-only (the experimental layer)**: whole-plan compilation and
abstract execution, automatic scheduling, operation keys derived from the
logical effect, automatic reconciliation, pre-planning inventory sync, and the
runtime verifier. The baseline instead uses a fresh key per tool call (explicit
retries reuse it) and reconciles through its own decisions.

**Budgets**: same action budget (120 dispatches) and elapsed-time budget.
Because the baseline needs a decision per action, its decision budget is
3 × the action budget; Revolve's is 12 planner decisions. This asymmetry is
inherent to the architectures and is reported, not hidden.

**Fault injection**: event-based, keyed to world effects ("after the nth
applied `workspace.place`"), stored in the adapter's store and consumed
exactly once, so both arms meet the same world condition regardless of how many
planner calls they make. Planner faults (connection loss, an injected
unauthorised proposal) are also event-triggered. Note the consequence: an
injected proposal that becomes due after execution starts only reaches an arm
that makes another decision — Revolve often makes none.

**Scenarios**: `scenarios/development/` (14, used while building) and
`scenarios/held-out/` (31, generated after the policy and runtime were written,
by `scripts/gen_scenarios.py` with fixed seeds) across 12 families: nominal,
shortage, substitution, record mismatch / disturbance, destination
unavailable, goal change (incl. rework), crash after/before effect, unknown
outcome (resolvable and ambiguous), planner unavailable, unauthorised proposal,
unknown classification, cancel. Each scenario states its expected terminal
outcome and number of truly complete kits. Caveat: the same author wrote the
policy and the held-out generator, so "held-out" means *not tuned against*,
not independent.

**Evaluator** (`src/eval.rs`) reads simulator truth and checks the system's
claims: truly complete kits (true kinds, current goal), false success claims,
inventory record vs truth, invariant violations, duplicate consumption,
unauthorised attempts (compile rejections + gateway denials) and executions
(every applied receipt re-checked against policy), recovery after a fired
fault, human interventions, planner requests, dispatches, wall time and peak RSS
of each executor process.

`success` = terminal state and truly-complete count match the expectation, zero
false claims, zero duplicate effects, zero invariant violations, zero
unauthorised executions, and a correct inventory record (except for runs that
are expected to end blocked).

## How to run

```
revolve benchmark --suite development --planner scripted
revolve benchmark --suite held-out --planner scripted
revolve benchmark --suite held-out --planner configured --budget <max HTTP calls> --trials 3
```

Output: `raw.jsonl` (one verdict per trial), `results.csv`, `summary.json`,
`report.md`, `incomplete.json` (trials that could not be evaluated, with the error).

## Results so far (scripted planners only)

> Both arms run the same deterministic rule-based task policy
> (`planner/scripted.rs::decide`). These runs validate the harness, the runtime
> and the symmetry of the comparison. **They are not evidence that Revolve
> outperforms a real AI agent.** No live-model comparison has been run.

| suite · host | arm | success | false claims | wrong inventory | dup. effects | refused repeats | unauthorized executed | faulted trials recovered | planner requests | mean wall ms/trial | max executor RSS |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| development · Windows x86_64 | Revolve | 14/14 | 0 | 0 | 0 | 0 | 0 | 6/6 | 20 | 406 | 10.4 MB |
| development · Windows x86_64 | baseline | 14/14 | 0 | 0 | 0 | 0 | 0 | 6/6 | 317 | 499 | 9.3 MB |
| held-out · Windows x86_64 | Revolve | 31/31 | 0 | 0 | 0 | 0 | 0 | 14/14 | 45 | 659 | 11.9 MB |
| held-out · Windows x86_64 | baseline | 31/31 | 0 | 0 | 0 | 0 | 0 | 14/14 | 1163 | 874 | 9.7 MB |
| held-out · Jetson Orin Nano (aarch64) | Revolve | 31/31 | 0 | 0 | 0 | 0 | 0 | 14/14 | 45 | 864 | 8.8 MB |
| held-out · Jetson Orin Nano (aarch64) | baseline | 31/31 | 0 | 0 | 0 | 0 | 0 | 14/14 | 1163 | 796 | 7.3 MB |

"Refused repeats" counts pick/place requests that repeated an already-applied
move and were refused by the adapter's own preconditions (no effect, but
evidence of an unsafe retry). Inventory correctness is judged from parts
physically in stock bins (not from the system's ledger), and any part in a kit
tray without a consumption record is an invariant violation. These stricter
checks were added after an independent code review; the numbers above were
produced with them.

Raw data: `results/development-scripted/`, `results/held-out-scripted/`,
`results/jetson-held-out-scripted/`. No trials were excluded.

### What this does and does not show

- **Supported**: the runtime and harness behave correctly on all 45 scenario
  configurations in both arms and on both hosts; the evaluator's checks are
  exercised (and it detects false claims — `tests/evaluator.rs`).
- **Structural differences that will persist with any planner**: Revolve asked
  the planner 45 times for work the baseline needed 1,163 decisions for (26×).
  With a paid model this is a cost and latency difference, and with a
  disconnected planner it is a resilience difference: in
  `planner-unavailable` scenarios Revolve finished its validated plan offline
  and needed the human to restore the planner only when a *new* plan was
  required (1 intervention vs 2 on the held-out set).
- **Not shown**: correctness, recovery or cost advantages against a real
  model. The scripted baseline is a competent, state-driven agent; a live model
  may make mistakes that Revolve's compiler, reconciliation and verifier catch
  (or may not). That is the H1 experiment still to run.
- Wall-time differences here are dominated by process start-up and SQLite
  `fsync`; they are not meaningful comparisons of the architectures.

## Next: live-model comparison (unperformed)

Requires: a project-designated key in `REVOLVE_ANTHROPIC_API_KEY` (or another
variable named in `config/planner.json`), an explicit call budget, and repeated
trials. Suggested first pass: development suite × both arms × 3 trials. Note
that the baseline needs ~25 model calls per trial against ~1–3 for Revolve, so
a symmetric trial count is budget-dominated by the baseline; the budget flag
counts every HTTP attempt. Cost is reported only if `pricing` (with an `as_of`
date) is filled in the config.
