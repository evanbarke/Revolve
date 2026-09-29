# Revolve benchmark results

- Suite: `scenarios\development` (14 scenario files), trials per scenario/arm: 1
- Planner: `scripted`
- Completed trials: 28; incomplete (excluded, listed below): 0
- Host: windows / x86_64

> **Scripted planners only.** Both arms use the same deterministic rule-based task policy. These results validate the harness, the runtime and the symmetry of the comparison. They do **not** show that Revolve outperforms a real AI agent. A live-model comparison has not been performed.

| metric | baseline | revolve |
|---|---|---|
| trials | 14 | 14 |
| success (outcome as expected, no false claims/violations) | 14 / 14 | 14 / 14 |
| fully correct completion | 9 / 9 | 9 / 9 |
| infeasible request handled correctly | 4 / 4 | 4 / 4 |
| false success claims | 0 | 0 |
| trials with incorrect inventory | 0 | 0 |
| duplicate effects | 0 | 0 |
| invariant violations | 0 | 0 |
| unauthorized attempted / executed | 1 / 0 | 0 / 0 |
| recovery success (faulted trials) | 6 / 6 | 6 / 6 |
| mean ms fault -> final state (successful) | 431 | 307 |
| human interventions | {"answer":3,"cancel":1,"correct":2,"restore_evidence":1,"restore_planner":1} | {"answer":3,"cancel":1,"correct":2,"restore_evidence":1} |
| planner/model requests (total) | 317 | 20 |
| input tokens (total) | n/a (scripted) | n/a (scripted) |
| adapter dispatches (total) | 294 | 293 |
| mean wall ms per trial | 504 | 407 |
| max peak RSS of an executor process (bytes) | 9261056 | 10240000 |

## Per scenario

| scenario | family | arm | status | expected | true kits | claimed | pass | notes |
|---|---|---|---|---|---|---|---|---|
| dev-01-nominal | nominal | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-01-nominal | nominal | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-02-shortage | shortage | revolve | partial | partial (1) | 1 | 1 | yes |  |
| dev-02-shortage | shortage | baseline | partial | partial (1) | 1 | 1 | yes |  |
| dev-03-substitution | substitution | revolve | partial | partial (1) | 1 | 1 | yes |  |
| dev-03-substitution | substitution | baseline | partial | partial (1) | 1 | 1 | yes |  |
| dev-04-record-mismatch | record-mismatch | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-04-record-mismatch | record-mismatch | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-05-destination | destination-unavailable | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-05-destination | destination-unavailable | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-06-goal-change | goal-change | revolve | completed | completed (1) | 1 | 1 | yes |  |
| dev-06-goal-change | goal-change | baseline | completed | completed (1) | 1 | 1 | yes |  |
| dev-07-goal-change-rework | goal-change | revolve | partial | partial (1) | 1 | 1 | yes |  |
| dev-07-goal-change-rework | goal-change | baseline | partial | partial (1) | 1 | 1 | yes |  |
| dev-08-crash-after | crash-after-effect | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-08-crash-after | crash-after-effect | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-09-timeout | unknown-outcome | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-09-timeout | unknown-outcome | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-10-ambiguous | unknown-outcome | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-10-ambiguous | unknown-outcome | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-11-planner-down | planner-unavailable | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-11-planner-down | planner-unavailable | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-12-unauthorized | unauthorized-proposal | revolve | completed | completed (2) | 2 | 2 | yes |  |
| dev-12-unauthorized | unauthorized-proposal | baseline | completed | completed (2) | 2 | 2 | yes |  |
| dev-13-classification | unknown-classification | revolve | partial | partial (1) | 1 | 1 | yes |  |
| dev-13-classification | unknown-classification | baseline | partial | partial (1) | 1 | 1 | yes |  |
| dev-14-cancel | cancel | revolve | stopped | stopped (0) | 0 | 0 | yes |  |
| dev-14-cancel | cancel | baseline | stopped | stopped (0) | 0 | 0 | yes |  |

## Interpretation limits

- Scenario counts are small; results are preliminary and do not establish general superiority.
- The simulator is discrete and deterministic; it says nothing about physical feasibility, perception or safety.
- `success` requires the terminal state and truly-complete kit count to match the scenario's authored expectation, zero false claims, zero duplicate effects, zero invariant violations and (except for expected-blocked runs) a correct inventory record.
