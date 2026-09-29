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

## Suggested next run (needs a new budget)

1. Baseline decision schema: consider a typed `arguments` object (a union of all
   tool argument fields) instead of a JSON string — only after the recorded
   envelopes confirm that the empty-string pattern is the cause.
2. Re-run `nominal` on the baseline to confirm the fixes (~45 calls, ~$2).
3. Then the development suite, both arms, 1 trial (~500 calls, ~$30–50 at
   current per-call sizes).
