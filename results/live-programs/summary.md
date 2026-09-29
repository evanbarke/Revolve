# Live AI program writing: first check (2026-09-29)

- Model: `claude-opus-5-5` via the configured planner key. Program writing uses
  effort `medium` and at most 3 calls per request (write, plus up to 2 repairs
  from the checker's messages).
- Cost: estimated from the dated pricing config (as of 2026-09-25: $4 input and
  $20 output per million tokens).
- Budget approved by the owner: up to 15 calls. **Used: 7 calls, about $0.24.**
- Table: the playground scenarios. Each accepted program was rehearsed on a
  digital twin. Requests 5a and 5b also ran for real.

| # | request (table) | calls | seconds | est. cost | result |
| --- | --- | --- | --- | --- | --- |
| 1 | "Sort the table: red blocks in tray 1, blue in tray 2, everything else back in its bin." (messy) | 1 | 13.2 | $0.032 | accepted first try; handled a part already in the arm; left the unidentified part on the table because no bin takes it, and said so; 6/6 checks in rehearsal. The explanation came back empty, which led to a fix. |
| 2 | "Make 2 kits in trays 1 and 2 … leftovers back in their bins, and ask me about anything you can't identify." (messy) | 1 | 18.6 | $0.050 | accepted first try; asks with 3 options; 4/4 checks in rehearsal |
| 3 | "Put all the yellow blocks in tray 1 and stack the blue blocks into a tower." (messy) | 1 | 5.4 | $0.018 | correctly refused (no yellow blocks; the arm cannot stack) and offered alternatives |
| 4 | "Build three kits in trays 1 to 3, each with two washers and a green block. Table first, then the bins; tray 4 gets what is left." (workshop) | 1 | 11.9 | $0.036 | accepted first try; worked out that the table has no green blocks; 5/5 checks in rehearsal |
| 5a | mid-run: "Actually, put the washers in tray 3 instead of their bin, and leave the unidentified part on the table." | 1 | 12.7 | $0.038 | the program was good, but the old program kept moving while it was written, so 3 rehearsal checks failed and the run finished before the switch. This design flaw led to pausing first. |
| 5b | the same, after the fix (the run paused with a green block in the arm) | 1 | 11.0 | $0.033 | new program puts the held block away first; 6/6 checks in rehearsal; switched mid-run; **completed with 6/6 checks held**; the first program was superseded and its moves stayed |
| 6 | "Keep taking parts off the table and filling the trays in order, at most 3 per tray, until the table is empty." (messy) | 1 | 16.4 | $0.035 | avoided the forbidden `while` by itself (bounded `for` loops); 3/3 checks in rehearsal |

The checker's repair loop was not needed in these 7 calls. It is covered by
offline tests only. This is a small first check, not a benchmark: one table
family, one trial per request.
