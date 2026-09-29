# Decision records

Short records of choices that shape the prototype or deviate from the brief.

**D1 — Single crate, library + binary.** Modules, not crates. Tests use the
library directly and the binary as a subprocess. Revisit when a second adapter
or a Wasm host needs a separate build boundary.

**D2 — Dependencies.** `rusqlite` (bundled SQLite: same engine on Windows and
the Jetson, no system library dependency), `serde`/`serde_json`, `clap`,
`tiny_http` (small synchronous HTTP server; no async runtime needed for a
loopback UI), `ureq` 3 (blocking HTTP client with rustls, for the live planner
only), `sha2`. Versions pinned in `Cargo.lock`. MSRV 1.94 (tested on 1.94.1 and
1.95.0); the floor feature is `std::fs::File::try_lock` (1.89).

**D3 — Two SQLite stores per run.** `world.db` is the adapter's store and
`run.db` the executor's, so an effect can survive a crash that happens before
the executor records it. Combining them into one transaction would remove the
failure mode under test.

**D4 — Inventory lives with the executor, as a separate adapter.** It is
project-owned local state, so its operations are single transactions with their
own idempotency records. The action's status is still a separate write, so
recovery uses the same status-lookup path as physical effects.

**D5 — Executors as child processes.** `demo`, `run`, `benchmark` and the UI
all launch `revolve resume` as a child. An injected crash is therefore a real
process exit (`std::process::exit(86)`, no unwinding, no executor cleanup), and
the OS releases the owner lock. The supervisor restarts it; `86` is reserved
for simulated crashes.

**D6 — Operation keys derive from the logical effect.** Keys are
`H(run, capability + canonical args, occurrence)` for Revolve. This lets a new
plan recognise an unsettled effect from an old plan. The baseline uses a fresh
key per call (with explicit same-key retry) because that is how tool-call ids
work in typical direct tool-calling agents; this difference is documented as
part of the experimental layer.

**D7 — Synchronous simulator semantics for "not found".** After a caller-side
timeout, a status of "not found" is final in v0 because the simulator processes
requests synchronously. A real controller needs a request-expiry guarantee (or
permanent same-key retries) before a key can be retired.

**D8 — Accounting model.** `on_hand` = known parts not yet recorded as
consumed (bins, gripper, trays awaiting their record). Chosen after the
evaluator caught a double decrement when a correction landed between a
placement and its consumption record under the earlier "bins only" definition.

**D9 — Scripted planner and baseline share one policy.** `decide()` produces a
whole plan for Revolve and the first step for the baseline, so the baseline is
not a deliberately weaker fixed sequence.

**D10 — Live provider.** The Anthropic Messages API over raw HTTP (Rust has no
official SDK), with schema-constrained output via `output_config.format`. Because
that feature requires `additionalProperties: false` on every object, the schema
constrains an envelope and the plan travels as a JSON string that Revolve
parses and compiles itself. Key variable defaults to `REVOLVE_ANTHROPIC_API_KEY`
so that a key present in the environment for other purposes is never used
implicitly; it is looked up in the environment, then in a gitignored project
`.env` (only the named variable is read). A Claude.ai subscription is not an API
credential (Messages API: API keys or workload identity; Agent SDK docs: API
keys, and no claude.ai login for third-party products), so the planner does not
use one. The first live calls were a 60-call smoke test approved by the owner.

**D11 — UI boundary.** Loopback bind only. Mutations require a custom header
(forces a CORS preflight that is never granted), a loopback Host (DNS-rebinding
guard; any port, so SSH tunnels work) and a loopback Origin if present.
Strict CSP; all rendering through DOM text nodes.

**D12 — Goal ids are deterministic per scenario** (`goal-<scenario>`), so
handwritten plans can bind to them. Unique within a run, which is the only scope
where they are compared.

**D14 — Independent review.** A read-only review by a separate agent traced
the recovery, claim, accounting, evaluator and UI paths. It confirmed the core
crash-safety and claim/correction properties and found liveness, accounting and
evaluator gaps (see the M2+ commit "Fix findings from independent review" for the
list). All confirmed findings were fixed with regression tests
(`tests/review_regressions.rs`); benchmark numbers were regenerated afterwards.

**D15 — Baseline uses native tool use.** The first live baseline wrapped each
tool call in a JSON envelope with the arguments as a JSON string; about 40% of
the model's decisions came back empty. A direct tool-calling baseline should use
the provider's tool-use interface: each capability is a tool with a strict
schema, invalid decisions are fed back into history (as compiler diagnostics
are for the planner). With that, 39/39 decisions were valid on `nominal`.

**D16 — Plans compile to WebAssembly; the program asks, the runtime acts.**
The owner's goal is an actual code compiler (`docs/vision.md`). Checked plans
are lowered with `wasm-encoder` to a Wasm module that wasmtime compiles to native
code with Cranelift. Wasm was chosen over emitting Rust or LLVM IR because:
- it is sandboxed by default;
- it is portable between x86_64 and the Jetson's aarch64;
- an import list gives least authority structurally;
- compilation takes milliseconds in-process, with no toolchain on the device.

The program performs no effects. It stops at the first undone step and the
executor dispatches that step through the unchanged path. Recovery is replay
from the first step. This keeps every invariant in one place and made the
engines provably equivalent on all 62 scenarios.

Translation validation (checking each request against the plan) guards codegen
bugs until the language grows constructs the plan cannot express. The engine
is recorded per run: old runs read as `interpreted` and the baseline as `direct`.

A related change applies to both engines: an active plan that contains a failed
or denied step is replanned instead of continued. Before, a crash between
recording a failure and replacing the plan left a hole that the interpreter
skipped. No benchmark result changed.

**D17 — The AI writes a Python-shaped language, not JSON or Python.**
- **Python-shaped:** models write Python fluently, which keeps first-try
  acceptance high (5/5 in the first live check).
- **Not a JSON AST:** it is verbose, hard for people to read and error-prone
  for models on nested control flow.
- **Not real Python:** it cannot be checked or bounded, and it reaches
  everything.

The restrictions (no while, no recursion, loops over fixed lists, required
`expect` checks, only built-in actions) are chosen so that the checker can make
the claims shown to the person.

- **Execution:** every outside-world call is one logged action, and the program
  is replayed from the top after interruptions. Refused moves are values.
  Completion comes only from the program's own checks, made by looking.
- **Rehearsal:** a real run on a `VACUUM INTO` copy with faults removed, so the
  person sees what will happen before deciding.
- **Changing a running program:** it pauses first. Live testing showed that
  otherwise the old program keeps moving while the new one is written.

**D18 — The simulator becomes a real-time robot controller.** The owner's
feedback was that the demo was slow and limited, and that it needs to emulate a
real world and run in real time. Motion now lives in the simulator, not the
browser:
- it uses real geometry, kinematics, and joint and speed limits;
- every segment is logged with wall-clock timing;
- a controller finishes a commanded segment even if the software dies, which
  matches the existing crash semantics;
- the browser only draws the logged motion.

three.js is bundled locally (MIT, integrity-checked from npm), so the page works
offline and on a LAN tablet. Speed 0 keeps tests and benchmarks instant and
unchanged. Program writing streams (Messages API `stream: true`, assembled into
the same response shape) through a background job the page polls. This was
chosen because tiny_http buffers chunked responses, which would defeat
server-sent events.

**D13 — Restricted grammar, not NLU, in scripted mode.** `parse-request` and
the UI accept only the kit family's phrasing and refuse anything else rather
than guessing.
