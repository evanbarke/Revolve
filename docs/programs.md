# Programs: the AI writes code, the compiler checks it, the robot runs it

Vision step 2 (`docs/vision.md`). A person says what they want. The AI writes a
short program in **Revolve Script** (`docs/language.md`, which is also the exact
guide the AI receives). The program is checked, compiled to machine code,
rehearsed on a copy of the table, shown to the person, and only then run by the
crash-safe executor.

```
words ─► AI writes a program ─► checker (lines, names, places, kinds, policy, no recursion, success checks)
                 ▲                     │ problems with line numbers (up to 2 repairs)
                 └─────────────────────┘
      ─► WebAssembly ─► native code (Cranelift) ─► rehearsal on a digital twin ─► the person decides
      ─► durable executor: one outside-world call at a time, replayed after any interruption
      ─► success judged only by the program's own `expect` checks, made by looking at the table
```

## Try it

```
revolve serve                       # then open http://127.0.0.1:8080
```

1. Pick a table ("A messy table", "A stocked workshop", "… + a power cut",
   "… + someone takes a part").
2. Type what you want, then press **Write the program**. Or press an example
   (no AI needed).
3. Read the program, the checks and the rehearsal, then press **Run it for real**.
4. While it runs:
   - the line that is running is highlighted and the arm moves to each place;
   - **Simulate a power cut** kills the executor after the next move, and
     **Continue** picks up at the same line without repeating anything;
   - **Change what it's doing** pauses the run and writes a new program from the
     table as it is.

CLI, without the browser or the AI:

```
revolve script check --file examples/programs/trays-by-color.rvs --scenario messy-table
revolve script run   --file examples/programs/sort-to-bins.rvs  --scenario messy-table-power-cut
```

## Why the language looks like Python

Models already write Python well, so a Python-shaped language keeps the
first-try success rate high. It is restricted so that each program can be
checked, and it always finishes:
- no `while` loops and no recursion; `for` loops run over a copy of a finite
  list;
- the robot is reached only through `pick`, `place` and `move`;
- every program must state how success is checked (`expect`), by looking at
  the table.

Anything outside the language gets a message with a line number that tells the
AI what to use instead (see `script/syntax.rs` and `script/check.rs`).

## Safety properties

- **Checked against the real table:** names, arity, part and place fields,
  literal part kinds and place ids (with "did you mean"), and literal
  destinations against the policy.
- **Least authority in the binary:** the module imports only the outside-world
  functions its program uses (`cap.observe`, `cap.pick`, `cap.place`,
  `human.ask`, `human.say`). Loading refuses anything else.
- **Every move goes through the unchanged executor path:** a fresh look (a pick
  names the part; where it is comes from the look), the atomic claim, gateway
  policy, adapter preconditions, verification, and reconciliation after unknown
  outcomes. A destination computed while running is still held to the policy:
  a denied move blocks the run with nothing placed (tested).
- **Refused moves are values:** `move` returns False when a part is gone or a
  place refuses it. The program decides what to do, and the report counts it.
- **Deterministic replay:** every look, move, question, message and check is one
  entry in the run's durable call log. After a crash the program runs from the
  top, and logged calls return their recorded results. A program that does not
  retrace its log is stopped before anything is dispatched.
- **Human in the loop:** `ask()` waits for the person, whose answer steers the
  program. Changing the request pauses the run first. The replacement is written,
  checked and rehearsed for exactly the table it will get, and takes over at a
  step boundary. Moves already made stay.
- **Honest completion:** a run is `completed` only if every `expect` held when
  it was checked by looking at the table; otherwise it is `partial`, with the
  failed checks listed.
- **Bounded:** fuel (50 M instructions per run of the program), 600
  outside-world calls, a value cap, `range` of at most 100, and the executor's
  action budget.

## Rehearsal (digital twin)

`harness::rehearse` works like this:
1. It copies the table with SQLite `VACUUM INTO`, a consistent snapshot even
   while a run is active.
2. It removes future faults and scripted events from the copy.
3. It runs the program through the same executor in-process, answering any
   question with its first option and noting that.
4. It reports the moves, the checks, the questions and where every part ends up.

Nothing in the source world changes. A test checks that the prediction matches
the real run on a fault-free table.

## Evidence (2026-09-29, Windows x86_64 and the Jetson)

- 79 tests pass on Windows and on the Jetson Orin Nano. On the Jetson the same
  program compiles to the same Wasm hash, and the power-cut and part-taken tables
  complete in about 0.6 s each. The tests include:
  - `tests/programs.rs` (8): run, power cut with no repeated move, part taken
    mid-run, question, mid-run replacement, rehearsal equals run, stale program
    refused, computed destination held to policy;
  - `script/tests.rs` (8): language semantics against a fake world, replay and
    divergence, errors with lines, fuel, checker messages, least-authority
    imports;
  - a writer test against a local fake API endpoint.
- Live AI check (`results/live-programs/summary.md`), 7 calls, about $0.24:
  - 5 requests accepted on the first attempt, with every rehearsal check passing;
  - 1 impossible request refused with alternatives;
  - 1 mid-run change with a part in the arm, completed with all checks held.

  This is a small sample on one table family, not a benchmark.

## Limits (honest)

- One domain: parts, bins, trays and a table. The arm moves between places, not
  through joint-level trajectories (vision step 3).
- Values are handles into a runtime-managed table: control flow is native code,
  and values are managed by the host. Lists nested in lists are copies.
- Replay re-runs the program from the top at each outside-world call:
  O(calls²) host calls, fine for hundreds of calls.
- The AI's explanation is its own words; what was checked is the compiler's list
  and the rehearsal, shown separately.
- The comparison with an AI writing Python directly (`docs/vision.md`, step 2)
  has not been run.
