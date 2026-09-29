# Revolve benchmark results

- Suite: `scenarios/held-out` (31 scenario files), trials per scenario/arm: 1
- Planner: `scripted`
- Completed trials: 62; incomplete (excluded, listed below): 0
- Host: linux / aarch64

> **Scripted planners only.** Both arms use the same deterministic rule-based task policy. These results validate the harness, the runtime and the symmetry of the comparison. They do **not** show that Revolve outperforms a real AI agent. A live-model comparison has not been performed.

| metric | baseline | revolve |
|---|---|---|
| trials | 31 | 31 |
| success (outcome as expected, no false claims/violations) | 31 / 31 | 31 / 31 |
| fully correct completion | 22 / 22 | 22 / 22 |
| infeasible request handled correctly | 6 / 6 | 6 / 6 |
| false success claims | 0 | 0 |
| trials with incorrect inventory | 0 | 0 |
| duplicate effects | 0 | 0 |
| invariant violations | 0 | 0 |
| unauthorized attempted / executed | 2 / 0 | 1 / 0 |
| recovery success (faulted trials) | 14 / 14 | 14 / 14 |
| mean ms fault -> final state (successful) | 671 | 697 |
| human interventions | {"answer":5,"cancel":2,"correct":3,"restore_evidence":1,"restore_planner":2} | {"answer":5,"cancel":2,"correct":3,"restore_evidence":1,"restore_planner":1} |
| planner/model requests (total) | 1163 | 45 |
| input tokens (total) | n/a (scripted) | n/a (scripted) |
| adapter dispatches (total) | 1116 | 1119 |
| mean wall ms per trial | 805 | 831 |
| max peak RSS of an executor process (bytes) | 7450624 | 8929280 |

## Per scenario

| scenario | family | arm | status | expected | true kits | claimed | pass | notes |
|---|---|---|---|---|---|---|---|---|
| ho-01-nominal | nominal | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-01-nominal | nominal | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-02-nominal | nominal | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-02-nominal | nominal | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-03-nominal | nominal | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-03-nominal | nominal | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-04-shortage | shortage | revolve | partial | partial (2) | 2 | 2 | yes |  |
| ho-04-shortage | shortage | baseline | partial | partial (2) | 2 | 2 | yes |  |
| ho-05-shortage | shortage | revolve | partial | partial (2) | 2 | 2 | yes |  |
| ho-05-shortage | shortage | baseline | partial | partial (2) | 2 | 2 | yes |  |
| ho-06-shortage | shortage | revolve | partial | partial (2) | 2 | 2 | yes |  |
| ho-06-shortage | shortage | baseline | partial | partial (2) | 2 | 2 | yes |  |
| ho-07-substitution | substitution | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-07-substitution | substitution | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-08-substitution | substitution | revolve | partial | partial (2) | 2 | 2 | yes |  |
| ho-08-substitution | substitution | baseline | partial | partial (2) | 2 | 2 | yes |  |
| ho-09-record-mismatch | record-mismatch | revolve | completed | completed (2) | 2 | 2 | yes |  |
| ho-09-record-mismatch | record-mismatch | baseline | completed | completed (2) | 2 | 2 | yes |  |
| ho-10-record-mismatch | record-mismatch | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-10-record-mismatch | record-mismatch | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-11-disturbance | record-mismatch | revolve | partial | partial (2) | 2 | 2 | yes |  |
| ho-11-disturbance | record-mismatch | baseline | partial | partial (2) | 2 | 2 | yes |  |
| ho-12-destination | destination-unavailable | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-12-destination | destination-unavailable | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-13-destination | destination-unavailable | revolve | completed | completed (2) | 2 | 2 | yes |  |
| ho-13-destination | destination-unavailable | baseline | completed | completed (2) | 2 | 2 | yes |  |
| ho-14-goal-change | goal-change | revolve | completed | completed (2) | 2 | 2 | yes |  |
| ho-14-goal-change | goal-change | baseline | completed | completed (2) | 2 | 2 | yes |  |
| ho-15-goal-change | goal-change | revolve | completed | completed (2) | 2 | 2 | yes |  |
| ho-15-goal-change | goal-change | baseline | completed | completed (2) | 2 | 2 | yes |  |
| ho-16-goal-change-rework | goal-change | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-16-goal-change-rework | goal-change | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-17-crash-after | crash-after-effect | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-17-crash-after | crash-after-effect | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-18-crash-after | crash-after-effect | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-18-crash-after | crash-after-effect | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-19-crash-before | crash-after-effect | revolve | completed | completed (2) | 2 | 2 | yes |  |
| ho-19-crash-before | crash-after-effect | baseline | completed | completed (2) | 2 | 2 | yes |  |
| ho-20-timeout | unknown-outcome | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-20-timeout | unknown-outcome | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-21-timeout | unknown-outcome | revolve | completed | completed (4) | 4 | 4 | yes |  |
| ho-21-timeout | unknown-outcome | baseline | completed | completed (4) | 4 | 4 | yes |  |
| ho-22-ambiguous | unknown-outcome | revolve | blocked | blocked (2) | 2 | 0 | yes |  |
| ho-22-ambiguous | unknown-outcome | baseline | blocked | blocked (2) | 2 | 0 | yes |  |
| ho-23-ambiguous | unknown-outcome | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-23-ambiguous | unknown-outcome | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-24-planner-down | planner-unavailable | revolve | completed | completed (4) | 4 | 4 | yes |  |
| ho-24-planner-down | planner-unavailable | baseline | completed | completed (4) | 4 | 4 | yes |  |
| ho-25-planner-down | planner-unavailable | revolve | completed | completed (4) | 4 | 4 | yes |  |
| ho-25-planner-down | planner-unavailable | baseline | completed | completed (4) | 4 | 4 | yes |  |
| ho-26-unauthorized | unauthorized-proposal | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-26-unauthorized | unauthorized-proposal | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-27-unauthorized | unauthorized-proposal | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-27-unauthorized | unauthorized-proposal | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-28-classification | unknown-classification | revolve | completed | completed (3) | 3 | 3 | yes |  |
| ho-28-classification | unknown-classification | baseline | completed | completed (3) | 3 | 3 | yes |  |
| ho-29-classification | unknown-classification | revolve | partial | partial (1) | 1 | 1 | yes |  |
| ho-29-classification | unknown-classification | baseline | partial | partial (1) | 1 | 1 | yes |  |
| ho-30-cancel | cancel | revolve | stopped | stopped (2) | 2 | 2 | yes |  |
| ho-30-cancel | cancel | baseline | stopped | stopped (2) | 2 | 0 | yes |  |
| ho-31-cancel | cancel | revolve | stopped | stopped (1) | 1 | 1 | yes |  |
| ho-31-cancel | cancel | baseline | stopped | stopped (1) | 1 | 0 | yes |  |

## Interpretation limits

- Scenario counts are small; results are preliminary and do not establish general superiority.
- The simulator is discrete and deterministic; it says nothing about physical feasibility, perception or safety.
- `success` requires the terminal state and truly-complete kit count to match the scenario's authored expectation, zero false claims, zero duplicate effects, zero invariant violations and (except for expected-blocked runs) a correct inventory record.
