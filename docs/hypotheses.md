# Hypotheses and evidence status

Kept separate on purpose. "Supported" below means *by the evidence listed*, not in general.

## H1 — Execution (primary)

*An explicit plan representation plus a compiler/runtime can improve
correctness, recovery or cost compared with a model directly calling the same
capabilities.*

- **Built**: both arms on the same gateway, policy, adapters and scenarios;
  evaluator against simulator truth; 45 scenario configurations.
- **Evidence so far (scripted planners only)**: both arms 100% correct on all
  suites; Revolve used 26× fewer planner decisions (45 vs 1,163 on the held-out
  set). The decision count is structural and will carry over to a live model;
  the correctness/recovery comparison will not, because the scripted baseline
  is a competent rule-based agent.
- **Status**: *untested against a real model.* Needs the live comparison
  (`docs/benchmark.md`, "Next").

## H2 — Portability (deferred, M5)

*A goal or plan can be reused across compatible capability implementations
without rewriting orchestration.*

- The plan boundary (contracts with versions) exists; only one implementation
  of each workspace capability exists. No evidence yet.

## H3 — Extension (deferred, M5)

*A missing bounded software capability can be generated, tested, registered and
composed under existing permission limits.*

- Not started. Design notes in `docs/roadmap.md`. No generated code runs in v0.

## H4 — Edge operation (first hardware experiment)

*The same core runtime can operate usefully on the available Jetson with
measured resource consumption and predictable behaviour during network loss.*

- **Evidence**: native aarch64 build; full test suite passing on the device;
  62/62 benchmark trials on the device; executor peak RSS ≤ 9 MB; ~0.9 s for a
  38-action run including process start-up; board input power 5.1 W mean during
  the workload; planner-loss scenarios behave as specified (validated work
  continues, new planning pauses, no authority change). See `docs/jetson.md`.
- **Limits**: simulated planner loss (a harness flag), not a real network
  partition; no physical actuation; no local inference.
- **Status**: *supported for the software runtime and simulator*.

## H5 — Latent interface (research; not started)

*A learned decoder from accessible model internals may improve action
generation compared with structured output.*

- No work in this prototype touches model internals. Structured JSON from an
  API, including reasoning summaries, is not access to hidden activations.
  Requirements are in `docs/roadmap.md`. **Status: untested research hypothesis.**
