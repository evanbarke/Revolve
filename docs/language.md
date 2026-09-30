# Revolve Script

Revolve Script is the language for programs that tell the robot arm what to do. It
looks like Python and is deliberately small. Before anything moves, each program
is checked, compiled to native code and rehearsed on a copy of the table. It
always finishes.

## The workspace

- **Places:**
  - `"table"`: a mixed pile; it accepts anything.
  - Bins: `"bin-red"`, `"bin-blue"`, `"bin-green"`, `"bin-washer"`. Each accepts
    only its own kind.
  - Trays: `"tray-1"`, `"tray-2"`, ...; they accept anything.
  - Some setups have no table, or a different number of trays.
- **Part kinds:** `"red_block"`, `"blue_block"`, `"green_block"`, `"washer"`, and
  `"unknown"` for a part the camera could not identify. An unknown part cannot
  go into a bin.
- **The arm** holds one part at a time. Parts are physical objects (simulated
  with gravity and contact): they are lowered and let go just above where they
  will rest.
- **Barriers:** some tables have solid barriers, listed in `table.barriers`. The
  arm goes over what it can. A place it cannot reach without hitting something
  cannot be used.

## Looking at the table

Each call looks again, so it sees what is there now.

| call | returns |
| --- | --- |
| `parts(kind=None, at=None)` | a list of parts; a part has `.id`, `.kind` and `.at` (the place id where it is) |
| `places(kind=None)` | a list of places (`kind`: `"bin"`, `"tray"`, `"table"`); a place has `.id`, `.kind`, `.accepts` (a part kind or None), `.available` (True/False) and `.count` (parts in it) |
| `count(at, kind=None)` | how many parts are at a place |
| `holding()` | the part in the arm, or None |

## Moving parts

Every move is checked by the safety policy before the arm acts.

| call | effect |
| --- | --- |
| `move(part, to)` | picks the part up from where it is and puts it at `to` (a place id or a place); returns True if it worked |
| `pick(part)` / `place(part, to)` | the two halves of `move` |

A move that cannot be done returns False. If the part is gone, the place is
unavailable, a bin does not accept that kind, or the arm cannot get there
without hitting something, `move` finds out before picking anything up, so
nothing happens. A part with another part on top of it cannot be picked up. A `place` refused after a `pick` leaves the part
in the arm. Check the result when it matters. If the arm may already hold a part
(for example, when continuing earlier work), deal with `holding()` first.

## Talking to the person

| call | effect |
| --- | --- |
| `ask("question?", ["option 1", "option 2"])` | waits for the person and returns the chosen option as text (2–6 options) |
| `say("text", value, ...)` | shows a short message to the person |

## Saying how success is checked (required)

    expect count("tray-1", kind="red_block") == 3, "tray 1 has 3 red blocks"

- Every program needs at least one `expect`.
- The condition must look at the table (`count()`, `parts()`, `places()` or
  `holding()`) and is checked when the line runs.
- Put the checks at the end.
- The description is shown to the person, so write it for them.
- A failed expectation does not stop the program. It is reported.

## The language

- Assignments, `+=`, `-=`, `if`/`elif`/`else`, and `for x in list:` (with
  `break`, `continue` and `pass`).
- Functions: `def name(a, b):` at the top level, with `return`. Recursion is not
  allowed.
- List comprehensions: `[p for p in parts() if p.kind == "washer"]`.
- Conditional expressions: `a if cond else b`.
- Values: whole numbers, "text", True/False/None, and lists `[a, b]`.
- Operators: `+ - * // %`, `== != < <= > >=`, `in`, `not in`, `and`, `or`,
  `not`, `is None` and `is not None`.
- Built-ins: `len`, `range(n)` (at most 100 items), `str`, `int`, `min`, `max`,
  `sum`, `abs` and `sorted`. `list.append(x)`, and the text methods
  `.startswith`, `.endswith`, `.lower`, `.upper` and `.strip`.
- Join text with `+` and `str(value)`.
- Not available: `while`, imports, classes, lambda, try/except, dictionaries,
  tuples, slices, f-strings, decimals and assigning to `x[i]`.
- A `for` loop runs over the list as it was when the loop started.

## Example

    # Sort the table: blocks go to their bins; anything unidentified goes to tray 4.
    BINS = ["bin-red", "bin-blue", "bin-green", "bin-washer"]
    KINDS = ["red_block", "blue_block", "green_block", "washer"]

    def home(part):
        for i in range(len(KINDS)):
            if part.kind == KINDS[i]:
                return BINS[i]
        return "tray-4"

    for p in parts(at="table"):
        if not move(p, home(p)):
            say("Could not move", p.id)

    expect count("table") == 0, "the table is clear"
    expect count("tray-4", kind="unknown") == len(parts(kind="unknown")), "every unidentified part is in tray 4"
