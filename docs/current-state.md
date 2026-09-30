# Current state (handoff)

_Last updated: 2026-09-29, first build session (after vision step 2: the AI writes programs; see `docs/programs.md`)._

**Now in real time and 3D:** the simulator moves a jointed arm with real limits
and speeds; the page shows the cell live in 3D (drag/pinch), the AI's program
streams in as it is written, and the rehearsal plays as a ghost arm
(`docs/programs.md`, "The robot cell"). LAN tablets: `revolve serve --lan`.

**To try it:** `revolve serve`, open http://127.0.0.1:8080, pick "A messy table",
type what you want and press **Write the program** (or pick an example, no AI).
Then read what was checked and rehearsed and press **Run it for real**. While it
runs, try **Simulate a power cut** and **Change what it's doing**.

The goal these milestones serve is in `docs/vision.md`: human intent → program →
compiled code → robot movement, with the human in the loop. VR,
thoughts-to-action and a wearable Jetson are later stages.

## Milestones

| milestone | state |
| --- | --- |
| M0 inspect & define | done — empty directory → local git repo; Windows x86_64 dev host; Jetson reachable via `ssh jetson` |
| M1 handwritten plan offline | done — `revolve run --scenario nominal --plan examples/plans/kit-assembly.json` completes 3 verified kits; invalid/unauthorised plans are rejected with no effects |
| M2 uncertainty & corrections | done — durable receipts, stable keys, reconciliation, real subprocess crash tests, goal revisions, cancel/pause, stale-reply rejection |
| M3 AI experiment & demo | done for offline — scripted planner, live adapter (untested against the provider), baseline, UI, benchmark; **live-model comparison not performed** |
| M4 Jetson | executed on the device — native build, full tests, demos, crash recovery, planner loss, measurements, UI via SSH tunnel |
| Vision step 1: compile to real code | done — checked plans compile to WebAssembly → native code (Cranelift); default engine; 62/62 scenarios identical to the interpreter on Windows and the Jetson (`docs/codegen.md`) |
| Vision step 2: a real language | done (v1) — Revolve Script: Python-shaped, loops/branches/functions/comprehensions; checked against the table and policy; compiled to Wasm → native; run one outside-world call at a time with deterministic replay; rehearsal on a digital twin; the AI writes programs from words (live check: 5/5 accepted first try, 1 correct refusal, 1 mid-run change; 7 calls, ~$0.24); UI with live line highlight and an animated arm (`docs/programs.md`, `docs/language.md`) |
| M5, M6 | not started; see `docs/roadmap.md` |

## Verified commands (this session)

On Windows (debug and release) and on the Jetson (release):

- `cargo test` — 79 tests pass on Windows and on the Jetson (42 unit, 37
  integration).
- Jetson, vision step 2: native build 2 min; `script check` gives the same Wasm
  hash as Windows; the power-cut table recovers and completes (2 executor
  processes, about 0.66 s); the table where someone takes a part completes with
  the missing part reported (about 0.6 s).
- `revolve script check|run` on the three example programs and the four
  playground tables (power cut: 2 executor processes, 11 moves, none repeated;
  thief: the missing part reported, program completed).
- UI program flow checked in headless Edge screenshots (review card, running
  line highlight and arm, power cut and recovery, question, completion).
- Engine equivalence — `scripts/compare_engines.py` on the development,
  held-out and demo suites: 62/62 trials identical (verdict, every action with
  arguments and result, final world) on Windows and on the Jetson.
- `revolve build` — the example plan gives byte-identical Wasm on both machines;
  native `.cwasm` for x86_64 and aarch64.
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

## LAN mode

`revolve serve --lan` (opt-in) serves private-network devices with a 128-bit
access key (link once, then an HttpOnly SameSite=Strict cookie); public peers
and DNS-name Hosts are refused; mutations keep the custom-header and origin
checks. Verified with curl from the LAN address: no key or a wrong key gives
401; the right key gives a 303 and the cookie; with the cookie, pages and the
API give 200; a DNS-name Host gives 403; a POST without the custom header gives
403; loopback needs no key. Windows Firewall must allow the program on private
networks (the owner's choice; Revolve does not change the firewall).

## Public access (Jetson behind the owner's proxy)

On the owner's request the Jetson serves https://revolve.samix-technology.com.
The service (`deploy/revolve-public.user.service`, a systemd user unit with its
own state in `~/revolve/state/public`) runs `serve --lan --public-host`. The
proxy is mailcow's nginx on the proxy host (`deploy/nginx-revolve.conf`; on the
host it is split into `00-revolve.conf` and `00-revolve.ssl.conf`, mode 600,
because it holds the key). The proxy asks for a password (basic auth, HTTPS
only, a Let's Encrypt certificate renewed by the host's certbot) and then adds
the access key as `X-Revolve-Key`; the password is not forwarded. Verified
2026-09-30 through the proxy:
- plain http gives a 301;
- no password or a wrong one gives 401; the right one gives 200;
- an action without the client header, or from another origin, gives 403;
- an action from the site's own origin works.

On the Jetson itself: no key gives 401 and a foreign Host gives 403. The
password is weak by the owner's choice. The spend cap is 60 AI calls per server
start (3 per request).

Writing: after an answer cut off at `max_tokens`, the retry asks for a brief one
(low effort, 32k tokens). Every request's attempts (stop reason, effort, output
tokens, seconds) are recorded next to its ledger as
`budgets/program-*.attempts.json`.

## Physics (D19)

The cell is simulated with Rapier (`src/physics.rs`): gravity, contact,
friction, solid bins, trays, barriers and the robot's column. Every arm path is
collision-checked, and routed over what is in the way or refused before anything
moves. Placed parts fall and settle, and the 3D view plays the recorded falls.
New playground tables: `wall-table` (the arm lifts parts over a 22 cm wall) and
`caged-tray` (tray 2 cannot be reached; a `move` there is refused before
anything is picked up, naming the cage). Verified 2026-09-30:
- 93 tests pass on Windows, including physics unit tests (a fall into a bin; a
  repeated drop bit for bit; a barrier in the arm's way; a covered part) and
  program tests (a wall gone over; a caged tray refused).
- Both benchmark suites on Windows and on the Jetson: 180 of 180 runs give the
  same verdicts as before physics.
- Cross-machine determinism: 5 example programs, including the power-cut and
  thief tables (30 simulated falls), leave every part at the same position and
  rotation, bit for bit, on Windows x86_64 and the Jetson (aarch64). This needed
  portable math (`libm`) in the cell's geometry: the platform's `sin`/`cos`
  differ in the last bit.

Simplifications: grasp is an attachment; the arm never pushes (a protective
stop); vertical moves are straight lines, others joint moves; routes go over
obstacles, not around them.

## Known limitations / open issues

- Scripted planner assigns trays before checking destination availability at
  pick time (a pick can precede a failed place; the held part is then placed on
  replan). Correct but not optimal.
- The UI step list shows the active plan only; older plans are listed with status.
- The UI "change the request" form edits identical kits only; for a request with
  different kits it says so (the CLI `correct --goal-file` accepts either form).
- UI status probes briefly take the executor lock; acquisition retries for
  ~0.5 s to tolerate this. A dedicated liveness file would be cleaner.

- Compiled engine: Revolve executor peak memory rose from about 9–12 MB to about
  16 MB (Cranelift in-process); wall time rose by about 3–8%. Programs are
  straight-line, so they cannot yet do anything the interpreter cannot (see
  `docs/codegen.md`, "Limits").

## Next concrete tasks

1. **Vision step 3: movement in simulation** — done (D18, D19): joints, reach,
   speed limits, rigid-body physics and collision-checked paths. Next here:
   routes around obstacles (not only over), and friction grasps.
2. **Measure "optimized for the AI".** Run the step-2 comparison: Revolve Script
   against an AI writing Python directly against the same capabilities, across
   the playground and fault tables (first-try acceptance, repairs, cost,
   correctness under faults). Needs an approved budget.
3. **Still open from M3 (needs a budget):** the live fault scenarios on both arms.
