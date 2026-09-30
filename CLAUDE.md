# Revolve — working notes for Claude Code sessions

Explicit-intent execution runtime prototype in Rust: human goal → planner
proposal (Action IR JSON) → compiler/validator + runtime-held policy →
persistent sequential executor with reconciliation → adapters (simulator
`world.db`, inventory ledger in `run.db`) → verifier. A direct tool-calling
baseline shares the gateway for benchmarking. Read `docs/current-state.md`
first; it records what works, what was verified, and the next task. `docs/vision.md`
is the goal the work serves (human intent → compiled code → robot movement).

## Commands

```
cargo build [--release]
cargo test                                   # ~45 s; spawns the revolve binary
cargo clippy --all-targets && cargo fmt
revolve demo --scenario <name>               # see `revolve scenarios`
revolve benchmark --suite development        # both arms, scripted
revolve serve                                # UI, loopback only
python scripts/gen_scenarios.py              # regenerate scenario fixtures
python scripts/make_example_plans.py         # regenerate examples/plans
revolve build --plan <file> --out x.wasm     # plan -> Wasm program (+ --native)
revolve script check|run --file examples/programs/<x>.rvs --scenario messy-table
python scripts/compare_engines.py <state> <resultsA> <resultsB>   # engine equivalence
```

Jetson (`ssh jetson`, key auth): source in `~/revolve/src`, target in
`~/revolve/target`, Rust in `~/.cargo/bin`. From Windows use
`C:\Windows\System32\OpenSSH\ssh.exe` (Git Bash's ssh cannot read the key).
Pipe files over ssh stdin; PowerShell pipes add CRLF.

## Invariants (do not break; tests cover them)

- Persist the claim (operation key, `dispatch_recorded`) before calling an adapter.
- The claim checks the goal revision and pause/cancel inside one `BEGIN IMMEDIATE`.
- Never hold a DB transaction across a planner or adapter call.
- The simulator commits effect + receipt + key/fingerprint check + fault consumption
  in ONE world.db transaction, separate from the executor's result write.
- Unknown outcome → status lookup → observation → same-key retry only if the
  contract says idempotent; otherwise block with a reason. Never blind retry,
  never mark in-flight work failed wholesale, never reset the world.
- Planner output is untrusted: runtime-owned fields are rejected; grants come
  only from `policy.rs`; goals carry no permissions.
- "Complete" is only ever reported from observation (verifier) or truth (evaluator).
- Physics (src/physics.rs, Rapier with `enhanced-determinism`): a fall is
  simulated before the effect transaction and committed inside it with the
  receipt; the physics world is rebuilt from world.db for every command, so
  results match bit for bit on Windows and the Jetson. Arm paths are checked,
  never pushed through: blocked means refused before anything moves.
- Compiled plan programs never perform effects: the host stops the program at the
  first undone step and the executor dispatches it through the normal path. Every
  program request is validated against the checked plan, and a module may import
  only `rt.*` and granted `cap.*`. Keep codegen deterministic (tests compare bytes).
- Revolve Script programs: every outside-world call (look, pick, place, ask, say,
  expect) is one logged action; replay must retrace the log or the run blocks;
  refused moves return False; completion only from `expect` checks made by looking.
  `docs/language.md` is sent verbatim to the AI: keep it accurate and short.
- Writing a replacement for a running program pauses the run first.
- Inventory `on_hand` = known parts not yet consumed (bins + gripper + trays awaiting record).
- Label scripted vs live everywhere; never substitute scripted for a selected live planner.
- No live model calls without an explicit, user-approved budget.

## Conventions

- Windows dev host: Python edits must write with `newline=''` or scripts get CRLF.
- Scenario/plan fixtures are generated; edit the generator scripts, not the JSON.
- Keep the executable path free of placeholders for future milestones; put
  future work in `docs/roadmap.md`.
- Commit locally at checkpoints; no remotes are configured.
