# Code generation: plans become programs

Vision step 1 (`docs/vision.md`). A checked plan is compiled to a WebAssembly
program. Wasmtime then compiles that program to native machine code with
Cranelift, and the durable executor runs it. The default engine for new Revolve
runs is `compiled`; `interpreted` (the original step walker) remains for comparison.

```
Action IR ─► compiler.rs (checks) ─► codegen.rs (Wasm module) ─► wasmtime/Cranelift (native code)
                                                   │
                         program.rs host: replay done steps, request the next one
                                                   │
                         runtime.rs: preconditions ─► atomic claim ─► gateway ─► verify
```

## What the program is

For each step in dispatch order, the generated `run()` function does three things:
1. builds the step's argument object: literals come from the module's data
   segment, and outputs of earlier steps are read through `rt.out`;
2. calls the step's capability import;
3. stops if the host says so.

The control flow and data flow of the plan are therefore machine code. Codegen
is deterministic: the same steps always give the same bytes (unit-tested).

ABI (`revolve-wasm/1`, all `i32`):

| import / export | meaning |
| --- | --- |
| `rt.json(ptr, len) -> value` | a literal from the data segment |
| `rt.obj() -> value` | a new, empty argument object |
| `rt.set(obj, key_ptr, key_len, value)` | set one argument |
| `rt.out(step, src_step, key_ptr, key_len) -> value \| -1` | an output of an earlier step |
| `cap.<capability id>(step, args) -> 0 \| 1` | continue, or stop now |
| export `run() -> 0 \| 1` | all steps done, or stopped |
| export `memory` | fixed size; it cannot grow |

A custom section, `revolve.plan`, lists the step ids by index for people and tools.

## Safety properties

- **Least authority by construction.** A module imports only the capabilities
  its plan uses, one import per capability. Loading refuses any import that the
  policy does not grant, and anything outside `rt.*` / `cap.*`.
  Example: the 38-step nominal plan imports 5 of the 9 capabilities.
- **The program never performs an effect.** At the first step that is not done,
  the host records the request and tells the program to stop. The executor then
  dispatches that one step through its usual path: fresh observation,
  preconditions, atomic claim, gateway, verification. After that it runs the
  program again. Every check between two effects (reconciliation, pause/cancel,
  budgets, questions, goal revisions) is unchanged.
- **Replay instead of special recovery.** Each run of the program starts at step
  one. Done steps return at once, and their outputs come from the durable record.
  A crash anywhere is therefore recovered simply by running the program again.
  The technique is deterministic replay, as used by durable workflow engines.
- **Translation validation.** Every request is checked against the checked plan:
  dispatch order, capability, and argument values. Replayed steps are checked too,
  against the arguments recorded when they were dispatched. Any difference blocks
  the run before anything is dispatched. So do a program that keeps going after
  being told to stop, and a program that finishes early.
- **Bounded.** Each run of a program has a fuel limit (5 M instructions), fixed
  memory and a cap on how many values it may create.
- **Recorded and checked artifact.** The `programs` table in `run.db` stores
  each accepted plan's program with its sha256 and imports. On load the hash is
  checked, and a mismatch blocks the run. A plan accepted before its program was
  stored is compiled from its recorded steps on resume.

## Commands

```
revolve build --plan examples/plans/kit-assembly.json --scenario nominal \
              --out kit.wasm [--native kit.cwasm]     # check + compile; nothing runs
revolve demo --scenario nominal [--engine compiled|interpreted]
revolve benchmark --suite held-out --engine compiled
python scripts/compare_engines.py <state-dir> <results-A> <results-B>
```

`--native` writes Cranelift's ahead-of-time machine code for this machine. It
loads only with the same wasmtime version and configuration.

## Evidence (2026-09-29)

Scripted planner, one trial per scenario. Every suite was run on both engines
and compared trial by trial with `scripts/compare_engines.py`. A trial counts as
identical when its verdict, every action (with arguments and result) and the
final world all match.

| suite | Windows x86_64 | Jetson Orin Nano (aarch64) |
| --- | --- | --- |
| development | 14 / 14 identical | 14 / 14 identical |
| held-out | 31 / 31 identical | 31 / 31 identical |
| demo | 17 / 17 identical | 17 / 17 identical |
| `cargo test` | 60 / 60 | 60 / 60 |

The same checked plan produces byte-identical Wasm on both machines (sha256
`94bef657…` for `examples/plans/kit-assembly.json`). Only the native code differs
per CPU.

The regenerated held-out and development results (`results/`) match the earlier
interpreter results in every verdict field. Only timing and memory differ.

`tests/compiled_programs.rs` covers:
- engine equivalence on five hard scenarios (crash, timeout, rework, cancel);
- one least-authority program per accepted plan;
- a tampered program is refused with no new effect;
- a missing program is regenerated on resume.

Unit tests cover:
- a module importing an ungranted capability or `env.system` is refused;
- a runaway loop is stopped by fuel;
- a program that differs from its plan, or ignores a stop, is blocked.

Cost of the compiled engine (same trials, held-out suite, Revolve arm):

| | Windows interpreted | Windows compiled | Jetson interpreted | Jetson compiled |
| --- | --- | --- | --- | --- |
| executor peak memory (max) | 11.7 MiB | 15.4 MiB | 9.3 MiB | 15.4 MiB |
| median wall time per trial | 663 ms | 682 ms | 864 ms | 918 ms |

The release binary grew from about 6 MB to 19.7 MB (Windows). A native build on
the Jetson takes 4 minutes. The 38-step nominal plan compiles to a 3.7–4.0 KB
Wasm module; its ahead-of-time native code is 30 KB for x86_64 (Windows) and
264 KB for aarch64 (Linux; file size includes the object-file container).

## Limits (honest)

- Plans are still straight-line step lists, so the program can do nothing the
  interpreter would not; translation validation enforces exactly that. The
  payoff comes with vision step 2: loops, branches and functions that a step
  list cannot express, where the compiler must check the program itself.
- Replay costs O(steps²) host calls per plan. That is trivial at tens of steps; a
  long-lived instance between effects is the fix when plans grow.
- Cranelift runs inside the executor (about 4 MiB extra). Precompiled `.cwasm`
  artifacts would remove that but tie runs to one wasmtime build.
- wasmtime is pinned to 47, the newest release whose MSRV (1.94) matches this crate.
