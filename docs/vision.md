# Vision

> "My whole goal was to be able to get AI to understand human intent and compile
> real code and robotic movements. My vision is a new integrated being mixing
> robot, AI and human." (project owner, 2026-09-29)

> "The ultimate goal is VR and taking thoughts to action, but it can be words at
> first because we don't have a brain–machine interface besides Neuralink for the
> moment. I have a Meta Quest 3 to test on later. And we can try to turn the
> Jetson into a wearable with a battery pack later." (2026-09-29)

## The idea in engineering terms

This is one system with a shared language at its centre:

```
human intent ──► AI writes a program ──► compiler checks it ──► machine code ──► robot moves
      ▲            (in a language made      (types, permissions,    (Wasm → native)   (through a
      │             for AI authors)          limits, reach)                           safety layer)
      └──────────── explained back in plain words · questions · corrections · stop ◄──┘
```

- **The human** supplies intent and judgement. They confirm, answer, correct and
  can stop anything at any time.
- **The AI** translates intent into a program. It is never trusted blindly.
- **The compiler** proves what it can before anything moves: permissions, parts,
  reach, limits. It then produces real executable code.
- **The robot** executes at machine speed, including offline. It moves only
  through a local layer that enforces speed, force, workspace and stop limits;
  the AI never commands motors directly.
- **The program** is the shared contract. The AI writes it, the compiler checks it,
  the machine runs it, and the human can read it (explained in plain words).

**Intent sources are pluggable.** Words come first (typed, then spoken). Later
sources are VR (gaze, gestures, hands) and eventually neural signals. Everything
downstream stays the same: confirm, program, compile, check, act. The noisier the
input channel (speech, EEG, a brain–computer interface), the more the
confirmation step and the compile-time checks matter. A misread thought must
never become an unchecked movement.

## Where the prototype stands (M0–M4)

| part | state |
| --- | --- |
| Understand intent | words → a program the AI writes for the table as it is, explained back in plain words, checked and rehearsed before the person decides; questions at run time; mid-run changes (pause, new program, switch) |
| Language for AI authors | **built (v1)**: Revolve Script, Python-shaped, with loops, branches, functions and required success checks; checker messages with line numbers the AI repairs from (`docs/language.md`, `docs/programs.md`) |
| Compiler front end | built: parse, types, references, permissions, abstract execution |
| Code generation | **step 1 built**: checked plans compile to WebAssembly → native code (x86_64 and the Jetson's aarch64), identical results to the interpreter on all 62 scenarios; programs are still straight-line (`docs/codegen.md`) |
| Crash-safe runtime | built: durable log, no repeated effects, recovery, verification from observation |
| Robot movement | simulated high-level actions only (pick/place); no arm model, no motion |
| Edge hardware | runs on the Jetson, including compiled plans (≤ 16.2 MB per executor, ~5 W measured before compilation was added) |

## Proposed path

1. **Compile to real code. Done 2026-09-29.** Checked plans compile to WebAssembly
   programs, and Cranelift turns them into native code. Capabilities are the only
   imports, and crash recovery is replay. The pass criterion was met: 62/62
   scenarios are identical to the interpreter on Windows and on the Jetson.
   See `docs/codegen.md`.
2. **A real language. Built 2026-09-29 (v1).** Revolve Script programs are
   written by the AI from the person's words and explained back in plain words.
   They are checked against the table and policy, compiled to native code,
   rehearsed on a digital twin, and run with deterministic replay (see
   `docs/programs.md`). Still to do: the comparison with an AI writing Python
   directly (first-try acceptance, repairs, cost, correctness under faults),
   which needs an API budget.
3. **Movement in simulation.** Add a simulated arm with joints, reach, speed
   limits and collisions. The compiler proves a program stays within limits;
   the safety layer enforces them at run time.
4. **Hardware.** Only after a requirements brief and an explicit decision to buy
   anything; a local safety controller and an independent stop come first.
5. **VR front end (Meta Quest 3, owned).** A WebXR view of the workspace and the
   plan in the Quest browser. Intent comes from voice, gaze and hands, with
   confirmation in VR. Transport keeps the server loopback-only: USB with
   `adb reverse` first, and LAN access only after authentication exists (as the
   brief requires).
6. **Wearable Jetson.** Battery-powered runtime and compiler on the body. For
   scale: the measured workload mean was about 5 W at the board input, but local
   model inference would draw far more. Pack size, heat and a safe power path
   need measuring first; no power-mode changes as a routine step.
7. **Neural intent (research).** Only through an existing, legitimately
   available interface. It stays behind the same confirmation and checking
   layers.

## Honest context

- Large labs pursue the opposite bet: vision-language-action models that map
  words and images straight to motor commands, with no program in between. They
  move fluidly but are hard to check or recover. This project bets on explicit,
  checkable, compiled programs. A hybrid is plausible: compiled programs
  orchestrate, while learned skills handle dexterous motion inside checked bounds.
- The closest software precedent is an AI writing robot code directly in Python
  ("code as policies"). The new language has to earn its place against that
  baseline with measured evidence.
- Each piece exists somewhere. What could be new is the combination and the
  evidence behind it.
