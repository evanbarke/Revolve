# Vision

> "My whole goal was to be able to get AI to understand human intent and compile
> real code and robotic movements. My vision is a new integrated being mixing
> robot, AI and human." (project owner, 2026-09-29)

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

## Where the prototype stands (M0–M4)

| part | state |
| --- | --- |
| Understand intent | partial: own words → structured goal, confirmation, questions, mid-run corrections; one domain (kit assembly) |
| Language for AI authors | partial: Action IR has typed steps and structured errors the AI repairs from; no loops or functions |
| Compiler front end | built: parse, types, references, permissions, abstract execution |
| Code generation | **not built**: the executor interprets checked plans |
| Crash-safe runtime | built: durable log, no repeated effects, recovery, verification from observation |
| Robot movement | simulated high-level actions only (pick/place); no arm model, no motion |
| Edge hardware | runs on the Jetson (≤ 9 MB, ~5 W) |

## Proposed path

1. **Compile to real code.** Lower checked plans to WebAssembly (native code
   via a Wasm runtime), where capabilities are the only imports. Run them through
   the existing runtime, which replays recorded results after a crash. Pass: same
   benchmark and crash-test results on Windows and the Jetson. No AI spend.
2. **A real language.** Add loops, branches and functions, and explain programs
   back in plain words before they run. Compare with an AI writing Python
   directly against the same capabilities (first-try compile rate, repair
   rounds, cost, correctness under faults). Needs an API budget.
3. **Movement in simulation.** Add a simulated arm with joints, reach, speed
   limits and collisions. The compiler proves a program stays within limits;
   the safety layer enforces them at run time.
4. **Hardware.** Only after a requirements brief and an explicit decision to buy
   anything; a local safety controller and an independent stop come first.

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
