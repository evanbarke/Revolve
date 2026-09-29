# Current state (handoff)

_Last updated: 2026-09-29, first build session (after the UI round with the user)._

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

- `cargo test` — 49 tests pass on Windows (24 unit, 25 integration). The
  Jetson ran the 39 that existed at the review-fix commit; the ten newer tests
  (plan-format example, .env lookup, empty-call handling, interpretation,
  preferred trays, different kits, served build) have run on Windows only.
- `cargo clippy --all-targets` — no warnings; `cargo fmt` applied.
- `revolve demo --scenario <each of 17 demo scenarios>` — expected outcome, evaluator PASS (both arms via `tests/evaluator.rs`).
- `revolve run --scenario nominal --plan examples/plans/kit-assembly.json` — completed, PASS.
- `revolve validate --plan examples/plans/<each>.json` — valid plan accepted; each invalid variant reports its category; exit code 1 when invalid.
- `revolve benchmark --suite development|held-out` — 14/14 and 31/31 on both arms (Windows); 31/31 both arms on the Jetson; regenerated after the review fixes with the stricter evaluator.
- `revolve correct/pause/resume/cancel` against a running executor from another process — behaves as documented.
- `revolve serve` — UI checked in headless Edge screenshots (question, mid-run); API boundary checks (missing header, cross-origin, rebinding Host, non-loopback bind) refused; crash + resume through the UI API; same on the Jetson through an SSH tunnel.
  The user then tried it interactively on Windows, which drove the changes below.

## UI after user testing

- **Person-first view**: one "Now" card says what is happening in plain words
  and offers only the buttons that make sense; plan, events, receipts and
  diagnostics are behind "Technical details".
- **Requests in your own words**: `/api/interpret` tries the offline parser
  first, then one low-effort AI call (1-call ledger, ~$0.005–0.013). The page
  shows "I understood: …" plus any assumptions; nothing runs until the person
  presses "Yes, start". The AI output is checked with `GoalSpec::validate` like
  any goal and is never executed directly.
- **Different kits in one request** (`GoalSpec.kits`, e.g. 3 green / 3 red /
  5 washers) and named trays per kit, through planner, compiler, verifier,
  evaluator and both arms (`tests/review_regressions.rs`).
- **Out-of-date page detection**: the server stamps a build id into the page and
  every response (`X-Revolve-Build`); an old page shows a reload banner. Added
  after an old page rendered a list of different kits as "each with nothing".
  The confirmation never offers to start a kit with no parts.

## Not performed / unverified

- **Live model**: smoke tests on `nominal` only (118 calls, ~$5.13 in total).
  After fixes, both arms pass: Revolve 2 calls, baseline (native tool use) 39
  calls. See `results/live-smoke/summary.md`. The key comes from
  `REVOLVE_ANTHROPIC_API_KEY` (environment or gitignored `.env`). No live fault
  scenarios have been run, so there is no live H1 result yet.
- Real network partition on the Jetson (planner loss is a harness flag).
- UI: no automated browser tests; checked by screenshots, API calls and the
  user's own session on Windows. The benchmark suites contain no different-kits
  scenarios (covered by integration tests only).
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
- The UI "change the request" form edits identical kits only; for a request with
  different kits it says so (the CLI `correct --goal-file` accepts either form).
- UI status probes briefly take the executor lock; acquisition retries for
  ~0.5 s to tolerate this. A dedicated liveness file would be cleaner.

## Next concrete task

With a call budget: run the live fault scenarios on both arms (crash-after-effect,
timeout-unknown, unknown-outcome-restored, goal-change, planner-unavailable,
shortage; ~$2–3 per scenario pair), then the development suite, and report them
next to the scripted results.
