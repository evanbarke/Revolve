# Live-model smoke test — 2026-09-29

**Scope approved:** one scenario (`nominal`, 3 kits, 38 actions), both arms, one
trial, a total cap of **60 HTTP calls**. Model `claude-opus-5-5` (effort `high`,
server-side refusal fallback on), Anthropic Messages API, schema-constrained
output. All 60 calls were used. Cost figures below are estimates from token
counts at $4 / $20 per million input/output tokens (list price dated
2026-09-25); the actual bill may differ.

| run | arm | calls | input / output tokens | est. cost | outcome (evaluator, simulator truth) |
| --- | --- | --- | --- | --- | --- |
| `run-nominal-1790693709279-33e741` | Revolve | 4 | 22,213 / 25,885 | $0.61 | **failed**: all 4 proposals rejected before any effect (format); 0 actions |
| `run-nominal-1790694087339-d6a5cb` | Revolve | 2 | 12,129 / 14,980 | $0.35 | **completed**, PASS: 3/3 kits, inventory correct, 38 actions, 0 violations |
| `run-nominal-1790694207127-43706b` | baseline | 54 | 439,912 / 6,920 | $1.90 | **did not finish**: budget exhausted with 1/3 kits built; no false claims; inventory correct at stop |

Total: 60 calls, about $2.86.

## What happened

**Revolve, first attempt.** The model produced a substantively correct 38-step
plan, but with `capabilities` as a list of `{id, version}` instead of a map, and
without `plan_id`, `budgets` or `explanation`. The compiler rejected all four
proposals before any effect. Root cause on our side: the planning request did
not include the IR's exact format, and the header diagnostic ("invalid type:
sequence, expected a map") did not name the field, so repair attempts drifted.

**Fix** (commit after this test): every planning request now carries the IR
format with a worked example (`ir::plan_format`), and header diagnostics name
each bad, missing or unknown field with the expected shape. Replaying the
model's first proposal through the new compiler yields four precise diagnostics.

**Revolve, second attempt.** First proposal rejected on one field
(`expected_outcome` shape); the model fixed it from the diagnostic; plan #2
(38 steps) was validated, executed and verified: 3/3 kits, PASS. About 55 s
per planning call.

**Baseline.** One model call per action (~10 s each; input grows with history,
hence 440k input tokens). Of 54 calls: 23 actions succeeded; 28 decisions were
wasted — 12 rejected as malformed, 16 "calls" with an empty capability (counted
by the policy as denials) — and 2 `place` calls arrived with empty arguments.
Ten of the malformed decisions were caused by a harness bug (an empty
`arguments_json` for argument-less tools was rejected instead of read as `{}`,
and `workspace.observe` / `ops.status` were listed as tools but not served);
the run was stopped, the harness fixed, and the same run resumed within its
ledger (limit raised from 50 to 54 to stay inside the approved 60). The
remaining empty decisions are not explained yet: raw decision envelopes were
not recorded. They are now (`planner_calls.detail.envelope`), and a call without
a capability is now classified as malformed rather than as an authorisation
attempt.

## What this shows and does not show

- The live path works end to end on both arms (auth, structured output, usage
  accounting, budget enforcement, crash-safe resume of a live run).
- Revolve's compiler did its job on real model output: malformed plans never
  reached the world, and precise diagnostics let the model repair its plan.
- The structural cost difference is visible with a real model: 2 planning calls
  (~27k tokens) versus 54+ decisions (~447k tokens) for less than half the work.
- It does **not** show that direct tool calling is worse in general: the
  baseline's interface had defects on our side (fixed or instrumented now), and
  one trial of one scenario is not a comparison.

## Follow-up runs (owner approved further spend)

| run | arm | calls | input / output tokens | est. cost | outcome |
| --- | --- | --- | --- | --- | --- |
| `run-nominal-1790695052813-293b61` | baseline (JSON envelope) | 19 | 108,028 / 2,246 | $0.48 | stopped deliberately after 18 decisions; raw envelopes recorded |
| `run-nominal-1790695763830-d78932` | baseline (native tool use) | 39 | 423,781 / 5,056 | $1.80 | **completed**, PASS: 3/3 kits, inventory correct, 0 violations |

**Diagnosis from the recorded envelopes.** With the original baseline interface
(the decision as a JSON envelope whose tool arguments were a JSON *string*, all
fields required), about 40% of decisions came back as a complete but empty
envelope (`{"decision":"call","capability":"","arguments_json":""}`, ~100 output
tokens), sometimes several times from an identical input; one came back as a
bare `""` and one with `capability: ","`. Rejected decisions were also not fed
back into the agent's history, so identical inputs repeated. The run was stopped
at 18 decisions because it could not finish within its cap.

**Fix.** The baseline now uses the Messages API's native tool use: one tool per
capability plus `ops_status`, `retry_call`, `ask_human` and `finish`, each with a
strict input schema, `tool_choice: auto` with parallel calls disabled (forced
tool choice is not supported on this model). Invalid decisions are fed back into
the agent's history, as compiler diagnostics are for the planner. Result: 39 of
39 decisions valid, 38 actions and a final report, PASS.

## Clean comparison on `nominal` (one trial each, after both fixes)

| | Revolve | direct tool calling |
| --- | --- | --- |
| outcome (evaluator) | completed, PASS | completed, PASS |
| model calls | 2 (1 rejected by the compiler, repaired) | 39 |
| input / output tokens | 12,129 / 14,980 | 423,781 / 5,056 |
| est. cost at list price | $0.35 | $1.80 |
| total model latency | 111 s (2 long planning calls) | 177 s (39 × ~4.5 s) |
| false claims / inventory errors / violations | 0 / 0 / 0 | 0 / 0 / 0 |

One scenario, one trial each: this establishes that both arms work end to end
on the live model and quantifies the cost structure (Revolve ~5× cheaper here;
the baseline re-reads a growing history every step). It says nothing yet about
correctness under faults, which is what H1 is about.

**All live calls to date:** 118 HTTP calls, about 1.01M input and 55k output
tokens, about $5.13 at list price.

## Next

Fault scenarios on the live model, both arms, where the architectures should
differ: e.g. `crash-after-effect`, `timeout-unknown`, `unknown-outcome-restored`,
`goal-change`, `planner-unavailable`, `shortage`. Roughly 2–5 calls per scenario
for Revolve and 40–60 for the baseline, so about $2–3 per scenario pair at
current sizes.
