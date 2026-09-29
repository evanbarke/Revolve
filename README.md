# Revolve

**Tell a (simulated) robot arm what to do in your own words.** The AI writes a
short program; the compiler checks it against the table and the safety policy,
compiles it to machine code and rehearses it on a digital twin; you decide; it
runs crash-safe, with the running line highlighted. See `docs/programs.md` and
the vision in `docs/vision.md`.

```
cargo build --release && ./target/release/revolve serve     # http://127.0.0.1:8080
```

Underneath, Revolve is a prototype execution runtime. You state a goal; a planner proposes
an explicit, versioned plan (the **Action IR**); a compiler checks it against
capability contracts, a runtime-held policy and the observed world; a
persistent executor carries it out step by step, survives crashes and unknown
outcomes without repeating effects, accepts goal corrections mid-run, and
reports only what a verifier actually observed.

v0 runs a small **simulated** kit-assembly workspace (bins, a gripper, kit
trays). It works offline, needs no GPU, no robot, no model download and no API
key. A benchmark compares it with a direct tool-calling baseline on the same
capabilities.

> Scope: a deterministic discrete simulator establishes execution semantics.
> It says nothing about physical feasibility, perception, collision avoidance or
> robot safety. Pause/cancel are logical controls, not an emergency stop.

## Quick start

Requires Rust ≥ 1.94 (tested 1.94.1 on Windows x86_64 and 1.95.0 on a Jetson
Orin Nano, aarch64). The first build downloads crates; the runtime is offline.

```
cargo build --release
./target/release/revolve doctor
./target/release/revolve demo --scenario nominal
./target/release/revolve demo --scenario crash-after-effect
./target/release/revolve serve            # UI at http://127.0.0.1:8080/
```

`demo` creates a fresh run under `state/runs/<run-id>/`, drives it with a
scripted human (answers questions, applies scenario corrections), restarts the
executor after an injected crash, then prints the run status and an
independent evaluation against simulator truth.

## Demo scenarios

`revolve scenarios` lists them. The canonical request is: *"Prepare three kits.
Each kit needs one red block, one blue block, and two washers. Update inventory
as you use parts. Tell me what is missing and ask before making substitutions."*

| scenario | what happens |
| --- | --- |
| `nominal` | enough stock; completes; inventory updated per verified placement |
| `shortage` | reports 2 of 3 kits and exactly what is missing |
| `substitution-approved` | asks before substituting green for blue; completes after approval |
| `record-mismatch` | record disagrees with observation; evidence-bound adjustment |
| `disturbance` | a stocked part disappears after planning; replans; partial |
| `destination-unavailable` | a tray becomes unavailable after planning; reassigns |
| `goal-change`, `goal-change-rework` | the human changes the goal mid-run; old dispatch stops; completed work is kept; rework is asked for |
| `crash-after-effect`, `crash-before-effect` | the executor process dies at the effect/result gap; a new one reconciles |
| `timeout-unknown` | a pick times out; status lookup shows it happened; not repeated |
| `unknown-outcome-blocked` / `-restored` | status and observation withheld: the run **blocks** and says why; resumes when evidence returns |
| `planner-unavailable` | planner lost after start: validated work continues; a new plan waits for reconnection |
| `unauthorized-proposal` | a proposal asks for a place outside the grants: rejected before any effect |
| `unknown-classification` | an unclassified item is needed: asks the human |
| `cancel` | cancelled mid-run: acknowledged, completed effects remain, reservations released |

## CLI

```
revolve doctor
revolve scenarios | capabilities [--json] | parse-request "<text>"
revolve demo --scenario <name> [--arm revolve|baseline] [--planner scripted|configured --budget N]
revolve new --scenario <name> [--plan file] [--scripted-human]      # create only; prints run id
revolve run --scenario nominal --plan examples/plans/kit-assembly.json
revolve validate --plan examples/plans/invalid-cycle.json [--scenario nominal]
revolve build --plan examples/plans/kit-assembly.json --out kit.wasm [--native kit.cwasm]  # plan -> program -> machine code
revolve script check --file examples/programs/trays-by-color.rvs --scenario messy-table     # check + rehearse
revolve script run --file examples/programs/sort-to-bins.rvs --scenario messy-table-power-cut
revolve resume --run <id>            # the executor; also recovers after a crash
revolve status [--run <id>] [--json]
revolve evaluate --run <id> [--json]  # score a run against simulator truth
revolve correct --run <id> --goal-file examples/goals/two-kits.json | --request "<text>"
revolve answer --run <id> --question <n> --choice <option>
revolve pause --run <id> | cancel --run <id>
revolve sim --run <id> restore-evidence | planner-offline | planner-online | unavailable --location tray-2 | arm-fault --fault '<json>'
revolve serve [--bind 127.0.0.1:8080]
revolve serve --lan                  # also tablets/phones on your private network, with an access key
revolve benchmark --suite development|held-out [--planner scripted|configured --budget N] [--trials N]
```

Revolve runs plans as compiled programs by default: each checked plan becomes a
WebAssembly program that runs as native code. `--engine interpreted` (on `demo`,
`new`, `run` and `benchmark`) selects the original step walker for comparison.
See `docs/codegen.md`.

Global: `--state-dir` (default `./state`, env `REVOLVE_STATE_DIR`),
`--scenarios-dir` (default `./scenarios`). Exit codes: 0 ok, 1 error or invalid
plan, 3 feature unavailable (e.g. live planner not configured), 86 simulated
crash (executor only).

## Planner modes

- **scripted** (default): deterministic rule-based planner for this task family.
  It is not an AI and accepts only the restricted request grammar.
- **fixed-plan**: executes a handwritten Action IR file once; cannot replan.
- **configured (live)**: Anthropic Messages API over HTTP. Copy
  `config/planner.example.json` to `config/planner.json`, put a key in the
  variable it names (default `REVOLVE_ANTHROPIC_API_KEY`) and pass an explicit
  `--budget <max HTTP calls>`. The key is read from the environment or from a
  gitignored `.env` in the project directory. Without a key or budget the command
  refuses with exit 3; it never falls back to the scripted planner. Only a
  60-call smoke test has been run so far (`results/live-smoke/summary.md`).

## Tests

```
cargo test            # unit + integration (includes real subprocess crash tests)
cargo clippy --all-targets && cargo fmt --check
```

## Using it from a tablet or phone (LAN mode)

`revolve serve --lan` also accepts devices on your private network (home Wi-Fi,
Tailscale). It prints a link with an access key: open
`http://<this computer's IP>:8080/?key=<key>` once on the device, and the key
is then kept in a strict cookie. Public addresses are refused, DNS names in the
Host header are refused, and the key (128 random bits) is stored in
`<state-dir>/lan-access-key`; delete that file to issue a new one. Windows
Firewall must allow `revolve.exe` on **private** networks; Revolve never
changes the firewall itself.

## Documentation

- `docs/vision.md` — the goal: human intent → compiled code → robot movement, and the proposed path
- `docs/programs.md` — the AI writes programs; checks, rehearsal, replay, the UI, evidence
- `docs/language.md` — Revolve Script (also the exact guide the AI receives)
- `docs/codegen.md` — plans compiled to WebAssembly / native code: ABI, replay, validation, evidence
- `docs/architecture.md` — runtime boundary and what the compiler does
- `docs/action-ir.md` — Action IR v1 and capability contracts
- `docs/recovery.md` — transactions, state machine, unknown outcomes, corrections
- `docs/benchmark.md` — protocol and results (scripted only so far)
- `docs/jetson.md` — discovery, native build, device measurements
- `docs/hypotheses.md`, `docs/decisions.md`, `docs/roadmap.md`, `docs/current-state.md`

## Limitations

Simulated world with synthetic observations; one workspace adapter; sequential
dispatch; loopback-only UI without accounts; no live-model results; no
generated capabilities; no physical hardware. See `docs/roadmap.md`.
