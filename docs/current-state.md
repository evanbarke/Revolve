# Current state (handoff)

_Last updated: 2026-09-29, end of the first build session._

## Milestones

| milestone | state |
| --- | --- |
| M0 inspect & define | done — empty directory → local git repo; Windows x86_64 dev host; Jetson reachable via `ssh jetson` |
| M1 handwritten plan offline | done — `revolve run --scenario nominal --plan examples/plans/kit-assembly.json` completes 3 verified kits; invalid/unauthorised plans are rejected with no effects |
| M2 uncertainty & corrections | done — durable receipts, stable keys, reconciliation, real subprocess crash tests, goal revisions, cancel/pause, stale-reply rejection |
| M3 AI experiment & demo | done for offline — scripted planner, live adapter (untested against the provider), baseline, UI, benchmark; **live-model comparison not performed** |
| M4 Jetson | executed on the device — native build, full tests, demos, crash recovery, planner loss, measurements, UI via SSH tunnel |
| M5, M6 | not started; see `docs/roadmap.md` |

## Verified commands (this session)

On Windows (debug and release) and on the Jetson (release):

- `cargo test` — 43 tests pass on Windows (20 unit, 23 integration). The
  Jetson ran the 39 that existed at the review-fix commit; the four newer unit
  tests (plan-format example, .env lookup, empty-call handling) have run on
  Windows only.
- `cargo clippy --all-targets` — no warnings; `cargo fmt` applied.
- `revolve demo --scenario <each of 17 demo scenarios>` — expected outcome, evaluator PASS (both arms via `tests/evaluator.rs`).
- `revolve run --scenario nominal --plan examples/plans/kit-assembly.json` — completed, PASS.
- `revolve validate --plan examples/plans/<each>.json` — valid plan accepted; each invalid variant reports its category; exit code 1 when invalid.
- `revolve benchmark --suite development|held-out` — 14/14 and 31/31 on both arms (Windows); 31/31 both arms on the Jetson; regenerated after the review fixes with the stricter evaluator.
- `revolve correct/pause/resume/cancel` against a running executor from another process — behaves as documented.
- `revolve serve` — UI checked in headless Edge screenshots (question, mid-run); API boundary checks (missing header, cross-origin, rebinding Host, non-loopback bind) refused; crash + resume through the UI API; same on the Jetson through an SSH tunnel.

## Not performed / unverified

- **Live model**: only a 60-call smoke test (approved budget, fully used) on
  `nominal`: Revolve completed after an IR-format fix; the baseline did not
  finish within its share (harness defects, now fixed or instrumented). See
  `results/live-smoke/summary.md`. The key now comes from
  `REVOLVE_ANTHROPIC_API_KEY` (environment or gitignored `.env`); the config
  points there. No live comparison has been run.
- Real network partition on the Jetson (planner loss is a harness flag).
- UI tested with screenshots and API calls, not with an interactive browser session.
- The pasted master brief was truncated at "Ask for an explicit API-call budget
  before …" (end of §26). Save the full brief as `docs/brief.md` for future sessions.

## Independent review

A read-only review agent traced the recovery, claim, accounting, evaluator and
UI paths. Core crash safety and claim/correction serialisation held. Confirmed
findings (liveness around postconditions and finalisation, evidence binding,
interrupted rework returns, evaluator trusting the ledger, store races, lost
human actions after a crash, UI GET Host check and scenario paths) were all
fixed, each with a regression test in `tests/review_regressions.rs` or the
server unit tests.

## Known limitations / open issues

- Scripted planner assigns trays before checking destination availability at
  pick time (a pick can precede a failed place; the held part is then placed on
  replan). Correct but not optimal.
- The UI step list shows the active plan only; older plans are listed with status.
- UI status probes briefly take the executor lock; acquisition retries for
  ~0.5 s to tolerate this. A dedicated liveness file would be cleaner.

## Next concrete task

With a new call budget: re-run the baseline on `nominal` (~45 calls) and inspect
the recorded decision envelopes (`planner_calls.detail`) to explain the empty
decisions; decide on a typed `arguments` object for the baseline schema; then run
the development suite on both arms and report it next to the scripted results.
