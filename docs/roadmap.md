# Roadmap (deferred work)

Current: M0–M3 implemented; M4 executed on the device; vision step 1 (plans
compiled to native code, `docs/codegen.md`) done. See `docs/current-state.md`.
The goal these steps serve, and the proposed path beyond them, is in `docs/vision.md`.

## Next concrete steps

0. **Vision step 2: a real language** (loops, branches, functions checked by
   the compiler itself; programs explained back in plain words; compared with
   an AI writing Python directly). See `docs/vision.md`.
1. **Live-model H1 comparison** — with an approved call budget: development
   suite × both arms × 3 trials, then held-out. Record usage, latency, failures
   by category. Decide from the data whether the compiler/recovery layer changes
   outcomes, not only cost.
2. **Validation-only ablation** — a Revolve mode with compilation but no
   automatic reconciliation, to separate the two sources of any advantage.
3. **Real network-loss test on the Jetson** — run the executor on the device
   with a live remote planner and drop the link (e.g. with a user-space proxy),
   instead of the harness flag.

## M5 — Extend and substitute capabilities (H2, H3)

- **Second compatible adapter (H2)**: e.g. a different workspace layout or a
  "slow controller" adapter with asynchronous completion (requires implementing
  `completion: acknowledged → observed later` for real). Measure: changes needed
  outside the adapter (target: none), plan reuse rate, new failure modes.
- **First generated capability (H3)**: a pure function with typed I/O, e.g. kit
  allocation (`stock, composition, trays → assignment`), behind a contract.
  Lifecycle: detect missing capability → write contract + property tests →
  generate implementation with a pinned, tiny dependency allowlist → build in an
  isolated environment with time/memory/disk limits (the Rust compile step runs
  build scripts and proc-macros, so it must itself be sandboxed — a Wasm runtime
  does not isolate compilation) → run independent tests → register the artifact
  by content hash with fixed permissions → compose it into a plan that compiles
  normally → observe executions. Runtime candidate: Wasmtime with fuel/epoch
  limits and a memory cap, host calls bounded separately; verify the chosen
  version's aarch64 support first. A generated module can never add grants.

## M6 — Research and physical embodiment

- **Physical adapter**: define frames, units, calibration, reachable workspace,
  observation uncertainty and controller guarantees for the actual hardware.
  High-level operations only (observe, pick, place, status, request-stop); no
  model-generated joint/torque commands; fast control and protective stops stay
  in a local controller. Tests: action completion, cancellation, grasp failure,
  object identity, communication loss, independent stopping. A requirements-based
  hardware brief comes before any purchase.
- **Perception**: start from supplied images/video or synthetic observations,
  clearly labelled; never claim camera integration from simulator coordinates.
- **Local planning on the Jetson**: only after measuring task-specific usable
  plan rate, latency and memory (weights + KV cache + OS + runtime) with a
  runtime that matches JetPack 6 / L4T 36.4. A local model that emits invalid
  plans more often may be unsuitable even if fast.
- **Latent decoder (H5)**: choose a small open model whose hidden states are
  accessible; define the representation, decoder objective, Action IR target,
  splits and a model/decoder compatibility contract; compare against a strong
  structured-output baseline with matched data/compute where possible; keep the
  compiler and policy after the decoder. Training resources need a separate
  decision; the Jetson is not assumed suitable for training.
