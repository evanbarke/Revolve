"use strict";
// Revolve UI. Every value shown comes from persisted run state returned by the
// server. All text is inserted with textContent / createTextNode; nothing from
// the server, the planner or the user is ever parsed as HTML.

const $ = (id) => document.getElementById(id);
const SVGNS = "http://www.w3.org/2000/svg";

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, String(v));
  }
  for (const k of kids.flat()) {
    if (k === null || k === undefined) continue;
    el.append(k instanceof Node ? k : document.createTextNode(String(k)));
  }
  return el;
}

function s(tag, attrs, ...kids) {
  const el = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, String(v));
  for (const k of kids.flat()) {
    if (k === null || k === undefined) continue;
    el.append(k instanceof Node ? k : document.createTextNode(String(k)));
  }
  return el;
}

function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); }

let current = null;
let scenarios = [];
let view = null;
let busy = false;

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (method !== "GET") {
    opts.headers["X-Revolve-Client"] = "ui";
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body || {});
  }
  const r = await fetch(path, opts);
  noteBuild(r.headers.get("X-Revolve-Build"));
  let j;
  try { j = await r.json(); } catch { j = { error: "HTTP " + r.status }; }
  if (!r.ok) throw new Error(j.error || ("HTTP " + r.status));
  return j;
}

// The server labels every response with the build of the page it serves. If that
// changes while this page is open, this page is out of date: say so rather than
// misread newer data.
const pageBuild = (document.querySelector('meta[name="revolve-build"]') || {}).content || null;
function noteBuild(b) {
  if (b && pageBuild && b !== pageBuild) $("stale-banner").hidden = false;
}

let toastTimer = null;
function toast(msg) {
  const t = $("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 4500);
}

async function act(action, body, quiet) {
  if (!current) {
    toast("Start a run first (top of the page), or pick one under Previous runs.");
    return false;
  }
  try {
    const r = await api("POST", "/api/runs/" + encodeURIComponent(current) + "/" + action, body);
    if (!quiet) toast(r.message || "Done");
    await poll();
    return true;
  } catch (e) {
    toast("Not possible right now: " + e.message);
    return false;
  }
}

// ------------------------------------------------------------------ plain-language helpers

const KIND = { red_block: ["red block", "red blocks"], blue_block: ["blue block", "blue blocks"], green_block: ["green block", "green blocks"], washer: ["washer", "washers"] };
const PREFIX = { red: "red_block", blue: "blue_block", green: "green_block", washer: "washer" };

function kindName(k, n) {
  const names = KIND[k] || [String(k).replace("_", " "), String(k).replace("_", " ") + "s"];
  return n === 1 ? names[0] : names[1];
}
function kindOf(objectId) { return PREFIX[(objectId || "").split("-")[0]] || "unknown"; }
function listJoin(items) {
  if (items.length <= 1) return items.join("");
  return items.slice(0, -1).join(", ") + " and " + items[items.length - 1];
}
const ORDER = ["red_block", "blue_block", "green_block", "washer"];
function countsText(obj) {
  const rank = (k) => (ORDER.indexOf(k) < 0 ? 99 : ORDER.indexOf(k));
  const items = Object.entries(obj || {}).filter(([, n]) => n > 0).sort((a, b) => rank(a[0]) - rank(b[0])).map(([k, n]) => n + " " + kindName(k, n));
  return items.length ? listJoin(items) : "nothing";
}
function binName(loc) {
  const k = { "bin-red": "red_block", "bin-blue": "blue_block", "bin-green": "green_block", "bin-washer": "washer" }[loc];
  return k ? "the " + kindName(k, 2) + " bin" : loc;
}
function trayNum(t) { return parseInt((t || "").split("-")[1], 10); }

function trayKitMap(v) {
  const map = {};
  const plans = (v.plans || []).filter((p) => Array.isArray(p.kit_assignments) && p.status !== "rejected" && p.status !== "stale");
  const p = plans.find((x) => x.plan_seq === v.active_plan_seq) || plans[plans.length - 1];
  for (const a of (p && p.kit_assignments) || []) map[a.tray_id] = a.kit;
  return map;
}
function placeName(loc, trayKit) {
  if (loc === "gripper") return "the robot hand";
  if ((loc || "").startsWith("tray-")) return trayKit[loc] ? "kit " + trayKit[loc] + " (tray " + trayNum(loc) + ")" : "tray " + trayNum(loc);
  return binName(loc);
}
// The kits one by one, whichever form the request uses (identical kits or a list).
function kitSpecs(goal) {
  if (Array.isArray(goal.kits) && goal.kits.length) return goal.kits.map((k) => ({ components: k.components || {}, tray: k.tray || null }));
  const trays = goal.preferred_trays || [];
  return Array.from({ length: goal.kit_quantity }, (_, i) => ({ components: goal.kit_components || {}, tray: trays[i] || null }));
}
function sizeOf(components) { return Object.values(components || {}).reduce((a, b) => a + b, 0); }
function kitSizeFor(goal, kit) { const k = kitSpecs(goal)[kit - 1]; return k ? sizeOf(k.components) : 0; }

function describeStep(a, v, trayKit) {
  const args = a.arguments || {};
  const obj = args.object_id || "";
  const kind = kindOf(obj);
  switch (a.capability) {
    case "workspace.pick": {
      const place = (v.steps || []).find((x) => x.capability === "workspace.place" && (x.arguments || {}).object_id === obj && x.action_id > a.action_id);
      const dest = place && (place.arguments || {}).destination_id;
      const forWhat = dest && dest.startsWith("tray-") ? " for " + placeName(dest, trayKit) : "";
      return "Picking up a " + kindName(kind, 1) + " from " + placeName(args.source_id, trayKit) + forWhat;
    }
    case "workspace.place":
      return (args.destination_id || "").startsWith("bin-")
        ? "Putting a " + kindName(kind, 1) + " back into stock"
        : "Putting a " + kindName(kind, 1) + " into " + placeName(args.destination_id, trayKit);
    case "inventory.consume": return "Recording that a " + kindName(args.part_kind, 1) + " was used";
    case "inventory.reserve": return "Setting aside the parts it needs: " + countsText(args.counts);
    case "inventory.release": return "Freeing parts it no longer needs";
    case "inventory.return": return "Recording that a " + kindName(args.part_kind, 1) + " went back into stock";
    case "inventory.adjust_count": return "Correcting the stock record for " + kindName(args.part_kind, 2);
    default: return "Working";
  }
}

// ------------------------------------------------------------------ scenarios

const FRIENDLY = [
  ["messy-table", "A messy table", "3 red, 2 blue and 2 green blocks, 3 washers and 1 part the camera can't identify, all on the table. Bins and trays are empty."],
  ["workshop", "A stocked workshop", "Bins full of parts, a small pile on the table, 4 empty trays."],
  ["messy-table-power-cut", "A messy table + a power cut", "The power fails right after the 4th part is put down. Press Continue: it checks what happened and the program carries on without repeating anything."],
  ["messy-table-thief", "A messy table + someone takes a part", "A red block disappears from the table mid-run. Watch the program notice and carry on."],
  ["nominal", "Build 3 kits (plenty of parts)", "Everything is available. Try changing the request while it works."],
  ["substitution-approved", "Build 3 kits: short of blue blocks", "It will ask whether it may use a green block instead."],
  ["shortage", "Build 3 kits: not enough blue blocks", "It builds what it can and tells you exactly what is missing."],
  ["unknown-classification", "Build 3 kits: one part can't be identified", "It will ask you what the unidentified item is."],
  ["record-mismatch", "Build 3 kits: the stock records are wrong", "It notices the records disagree with what it sees and corrects them."],
  ["disturbance", "Build 3 kits: someone takes a part mid-way", "A part disappears after planning; it re-plans and reports what's missing."],
  ["destination-unavailable", "Build 3 kits: a tray becomes unusable", "Tray 2 is taken away mid-run; it moves that kit to a spare tray."],
  ["crash-after-effect", "Build 3 kits: the software crashes", "It crashes right after a move. Press Continue: it checks what happened and does not repeat it."],
  ["timeout-unknown", "Build 3 kits: the robot doesn't confirm a move", "It checks whether the move happened before doing anything else."],
  ["unknown-outcome-blocked", "Build 3 kits: it can't tell if a move happened", "It stops and asks you to check the workspace instead of guessing."],
  ["planner-unavailable", "Build 3 kits: the AI planner goes offline", "Work that was already checked continues; it waits when it needs a new plan."],
  ["unauthorized-proposal", "Build 3 kits: the planner proposes something not allowed", "The bad plan is blocked before anything moves; a safe one is used."],
];

async function loadScenarios() {
  scenarios = await api("GET", "/api/scenarios");
  fillScenarioSelect();
}

function fillScenarioSelect() {
  const sel = $("scenario-select");
  const keep = sel.value;
  clear(sel);
  const ids = new Set(scenarios.map((x) => x.id));
  for (const [id, label] of FRIENDLY) if (ids.has(id)) sel.append(h("option", { value: id }, label));
  if ($("show-all-scenarios").checked) {
    const known = new Set(FRIENDLY.map((f) => f[0]));
    for (const set of ["demo", "development", "held-out"]) {
      const og = h("optgroup", { label: "test scenarios: " + set });
      for (const sc of scenarios.filter((x) => x.set === set && !known.has(x.id))) og.append(h("option", { value: sc.id }, sc.id));
      if (og.childNodes.length) sel.append(og);
    }
  }
  sel.value = keep && [...sel.options].some((o) => o.value === keep) ? keep : "messy-table";
  describeScenario();
}

function describeScenario() {
  const id = $("scenario-select").value;
  const f = FRIENDLY.find((x) => x[0] === id);
  const sc = scenarios.find((x) => x.id === id);
  $("scenario-desc").textContent = f ? f[2] : sc ? sc.description : "";
}

// ------------------------------------------------------------------ the "Now" card

function setBadge(cls, text) {
  const b = $("now-badge");
  b.className = "badge " + cls;
  b.textContent = text;
}

function actionButton(label, fn, cls) {
  return h("button", { type: "button", class: cls || "", onclick: fn }, label);
}

function progress(v, trayKit) {
  let placed = 0;
  for (const l of v.workspace_truth.locations.filter((x) => x.loc_type === "tray" && trayKit[x.location_id])) placed += Math.min(kitSizeFor(v.goal, trayKit[l.location_id]), l.objects.length);
  const total = kitSpecs(v.goal).reduce((a, k) => a + sizeOf(k.components), 0);
  return { placed, total };
}

function renderNow(v, trayKit) {
  const st = v.state.status;
  const report = v.state.final_report;
  const actions = $("now-actions");
  clear(actions);
  $("question-box").hidden = true;
  const truth = v.workspace_truth;
  const active = v.executor.active;
  const crashed = !active && v.executor.last_exit_code === v.executor.simulated_crash_code;
  const q = v.goal.kit_quantity;
  const prog = progress(v, trayKit);
  $("progress-bar").hidden = st === "created";
  $("progress-fill").style.width = (prog.total ? Math.round((100 * prog.placed) / prog.total) : 0) + "%";
  $("now-progress").textContent = prog.total ? prog.placed + " of " + prog.total + " parts in kits" : "";
  const title = (t, d) => { $("now-title").textContent = t; $("now-detail").textContent = d || ""; };
  const start = () => act("start", {}, true);

  if (st === "created") {
    setBadge("idle", "Ready");
    title("Ready to start", "It will look at the workspace, make a plan, check that plan, and then carry it out.");
    actions.append(actionButton("Start", start, "big"));
  } else if ((st === "running" || st === "paused") && crashed) {
    setBadge("warn", "Interrupted");
    title("The software stopped unexpectedly (a simulated crash)", "Press Continue. Before anything else it checks what the last move actually did, so nothing is done twice.");
    actions.append(actionButton("Continue", start, "big"));
  } else if (st === "running" && !active) {
    setBadge("idle", "Not running");
    title("Nothing is running right now", "Press Continue to carry on from where it stopped.");
    actions.append(actionButton("Continue", start, "big"));
  } else if (st === "running") {
    setBadge("info", "Working");
    const inflight = ["dispatch_recorded", "acknowledged", "retry_pending", "outcome_unknown"];
    const planSteps = (v.steps || []).filter((a) => a.plan_seq === v.active_plan_seq);
    const cur = planSteps.find((a) => inflight.includes(a.status)) || planSteps.find((a) => a.status === "pending");
    const done = planSteps.filter((a) => a.status === "succeeded" || a.status === "satisfied").length;
    title(cur ? describeStep(cur, v, trayKit) : "Planning the job",
      cur ? "Step " + (done + 1) + " of " + planSteps.length + " in the current plan." : "Looking at the workspace and checking the plan before anything moves.");
    actions.append(actionButton("Pause", () => act("pause"), "ghost"), actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "awaiting_human") {
    setBadge("info", "Needs your decision");
    title("It needs your decision before it continues", "Nothing more will move until you answer.");
    const qn = v.questions.find((x) => x.status === "open" && x.goal_revision === v.goal_revision);
    if (qn) {
      $("question-box").hidden = false;
      $("question-prompt").textContent = humanQuestion(qn);
      const opts = $("question-options");
      clear(opts);
      for (const o of qn.options) opts.append(actionButton(humanOption(qn, o), () => act("answer", { question_id: qn.question_id, choice: o.id })));
    }
    actions.append(actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "paused") {
    setBadge("idle", "Paused");
    title("Paused: nothing new will start", "Anything that was already moving has finished and been checked.");
    actions.append(actionButton("Continue", start, "big"), actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "blocked") {
    setBadge("bad", "Stopped to check");
    const reason = v.state.status_reason || "";
    const unknown = reason.includes("outcome unknown") || reason.includes("cannot confirm") || reason.includes("not verifiable");
    title(unknown ? "It isn't sure whether the last move happened, so it stopped" : "It stopped because it can't safely continue",
      unknown ? "The robot didn't confirm the move and the view of that area is unavailable. It will not guess or repeat the move. Look at the workspace, then continue." : reason);
    if ((truth.evidence_withheld_from_system || []).length) {
      actions.append(actionButton("I've checked: the view is back (simulation)", async () => { if (await act("sim", { control: "restore_evidence" }, true)) start(); }, "big"));
    } else {
      actions.append(actionButton("Continue", start, "big"));
    }
    actions.append(actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "awaiting_planner") {
    setBadge("violet", "Waiting for the planner");
    title("The AI planner can't be reached", "Work that was already planned and checked is done. A new plan is needed to continue.");
    if (truth.planner_offline) actions.append(actionButton("Reconnect the planner (simulation)", async () => { if (await act("sim", { control: "planner_online" }, true)) start(); }, "big"));
    else actions.append(actionButton("Continue", start, "big"));
    actions.append(actionButton("Stop", () => act("cancel"), "danger"));
  } else if (report && report.claims_source === "agent") {
    const n = (report.kits_claimed_complete || []).length;
    setBadge(st === "completed" ? "ok" : "warn", st === "completed" ? "Done" : st.replace("_", " "));
    title("The agent reports " + n + " of " + q + " kits built", "Comparison mode: this is the agent's own claim, not a check by the system. " + (report.message || ""));
  } else if (st === "completed") {
    const n = ((report && report.kits_verified_complete) || []).length;
    setBadge("ok", "Done");
    title("Done: " + n + " " + (n === 1 ? "kit" : "kits") + " built and checked", "Each kit was checked by looking at the tray, not just by trusting the steps. " + stockSentence(v));
  } else if (st === "partial") {
    const n = ((report && report.kits_verified_complete) || []).length;
    setBadge("warn", "Done, but not everything");
    const missing = report && Object.keys(report.missing_to_finish || {}).length ? "Missing to finish the rest: " + countsText(report.missing_to_finish) + "." : explainIncomplete(report);
    title("Built " + n + " of " + q + " kits", missing);
  } else if (st === "stopped") {
    const n = ((report && report.kits_verified_complete) || []).length;
    setBadge("idle", "Stopped");
    title("Stopped as you asked: " + n + " " + (n === 1 ? "kit was" : "kits were") + " finished", "Parts already placed stay where they are. Reserved stock was released.");
  } else if (st === "failed") {
    setBadge("bad", "Could not finish");
    title("It could not finish the job", v.state.status_reason || "");
  }
  if (["completed", "partial", "stopped", "failed"].includes(st)) {
    actions.append(actionButton("New request", () => { newRequest(); window.scrollTo({ top: 0, behavior: "smooth" }); }, "ghost"));
  }
}

function explainIncomplete(report) {
  if (!report) return "";
  const extra = (report.trays || []).filter((t) => Object.keys(t.excess || {}).length);
  if (extra.length) return "Some trays hold parts that are no longer in the kit, and you chose not to move them.";
  return "";
}

function stockSentence(v) {
  return "Stock left: " + countsText(v.inventory.on_hand) + ".";
}

function humanQuestion(q) {
  const c = q.context || {};
  if (q.kind === "substitution") {
    return "There aren't enough " + kindName(c.replace, 2) + " for every kit, but " + (c.spare || "some") + " " + kindName(c.with, c.spare || 2) + " are available. May it use a " + kindName(c.with, 1) + " instead of a " + kindName(c.replace, 1) + "?";
  }
  if (q.kind === "classification") {
    const want = (q.options[0] || {}).id;
    return "One item (" + c.object_id + ") couldn't be identified. It is short of " + kindName(want, 2) + ". Is this item a usable " + kindName(want, 1) + "?";
  }
  if (q.kind === "rework") return "After your change, some parts are in kits that no longer need them. May it move them back into stock?";
  return q.prompt;
}

function humanOption(q, o) {
  if (q.kind === "substitution") return o.id === "approve" ? "Yes, use a " + kindName((q.context || {}).with, 1) : "No, just tell me what's missing";
  if (q.kind === "classification") return o.id === "not_usable" ? "No, don't use it" : "Yes, it's a " + kindName(o.id, 1);
  if (q.kind === "rework") return o.id === "approve" ? "Yes, move them back" : "No, leave them";
  return o.label;
}

// ------------------------------------------------------------------ request, kits, story

function goalSentence(g) {
  const specs = kitSpecs(g);
  const trayText = (t) => (t ? " in tray " + trayNum(t) : "");
  const uniform = !(Array.isArray(g.kits) && g.kits.length);
  if (uniform) {
    const trays = specs.map((k) => k.tray).filter(Boolean);
    return g.kit_quantity + " " + (g.kit_quantity === 1 ? "kit" : "kits") + (g.kit_quantity === 1 ? " with " : ", each with ") + countsText(g.kit_components) +
      (trays.length ? ", in " + listJoin(trays.map((t) => "tray " + trayNum(t))) : "");
  }
  return specs.length + " kits: " + listJoin(specs.map((k, i) => "kit " + (i + 1) + " with " + countsText(k.components) + trayText(k.tray)));
}

function renderGoal(v) {
  const g = v.goal;
  $("goal-summary").textContent = goalSentence(g);
  const dec = $("goal-decisions");
  clear(dec);
  for (const x of g.approved_substitutions || []) dec.append(h("li", {}, "You allowed a " + kindName(x.with, 1) + " instead of a " + kindName(x.replace, 1) + "."));
  for (const x of g.declined_substitutions || []) dec.append(h("li", {}, "You said no to using " + kindName(x.with, 2) + " instead of " + kindName(x.replace, 2) + "."));
  if (g.rework_permitted === true) dec.append(h("li", {}, "You allowed moving extra parts back into stock."));
  if (g.rework_permitted === false) dec.append(h("li", {}, "You chose to leave extra parts where they are."));
  for (const q of v.questions.filter((x) => x.kind === "classification" && x.status === "answered")) {
    dec.append(h("li", {}, "You said " + (q.context || {}).object_id + (q.answer === "not_usable" ? " is not usable." : " is a " + kindName(q.answer, 1) + ".")));
  }
  const changes = v.goal_history.filter((r) => r.revision > 1 && !(r.reason || "").startsWith("answer to question"));
  for (const r of changes) dec.append(h("li", {}, "You changed the request to " + goalSentence(r.spec) + "."));
  if (!$("correct-form").contains(document.activeElement)) {
    $("c-qty").value = g.kit_quantity;
    $("c-red").value = g.kit_components.red_block || 0;
    $("c-blue").value = g.kit_components.blue_block || 0;
    $("c-green").value = g.kit_components.green_block || 0;
    $("c-washer").value = g.kit_components.washer || 0;
  }
  $("change-goal").hidden = ["completed", "partial", "stopped", "failed"].includes(v.state.status);
  $("mixed-note").hidden = !(Array.isArray(g.kits) && g.kits.length);
}

function renderKits(v, trayKit) {
  const ul = $("kits");
  clear(ul);
  const report = v.state.final_report;
  const verified = new Set((report && report.claims_source !== "agent" && report.kits_verified_complete) || []);
  const byKit = {};
  for (const [tray, kit] of Object.entries(trayKit)) byKit[kit] = tray;
  for (let k = 1; k <= v.goal.kit_quantity; k++) {
    const tray = byKit[k];
    const loc = tray && v.workspace_truth.locations.find((l) => l.location_id === tray);
    const n = loc ? loc.objects.length : 0;
    const size = kitSizeFor(v.goal, k);
    let state, cls;
    if (!tray) { state = "not planned"; cls = "wait"; }
    else if (verified.has(tray)) {
      const subs = (v.goal.approved_substitutions || []).filter((x) => loc && loc.objects.some((o) => o.kind === x.with) && !(v.goal.kit_components || {})[x.with]);
      state = "✓ built and checked" + (subs.length ? " (with a " + kindName(subs[0].with, 1) + ")" : "");
      cls = "ok";
    }
    else if (n >= size) { state = "all " + size + " parts placed"; cls = "ok"; }
    else if (n > 0) { state = n + " of " + size + " parts"; cls = "wait"; }
    else { state = "not started"; cls = "wait"; }
    if (loc && !loc.available) { state += " (tray unavailable)"; cls = "warn"; }
    const mixed = Array.isArray(v.goal.kits) && v.goal.kits.length;
    const what = mixed ? " (" + countsText(kitSpecs(v.goal)[k - 1].components) + ")" : "";
    ul.append(h("li", {}, h("span", {}, "Kit " + k + (tray ? " · tray " + trayNum(tray) : "") + what), h("span", { class: "state " + cls }, state)));
  }
  const reserved = v.inventory && Object.values(v.inventory.reserved || {}).some((n) => n > 0);
  $("stock-line").textContent = stockSentence(v) + (reserved ? " Set aside for this job: " + countsText(v.inventory.reserved) + "." : "");
}

function storyLines(v, trayKit) {
  const events = v.events.slice().reverse();
  const base = events.length ? events[0].at_ms : 0;
  const stepById = {};
  for (const a of v.steps || []) stepById[a.action_id] = a;
  const perTray = {};
  const lines = [];
  const add = (text, cls, e) => lines.push({ text, cls, t: e ? ((e.at_ms - base) / 1000).toFixed(0) + "s" : "" });
  const first = v.goal_history[0];
  for (const e of events) {
    const m = e.message || "";
    switch (e.kind) {
      case "run_created": if (first) add("You asked for " + goalSentence(first.spec) + ".", "you", e); break;
      case "plan_accepted": {
        const steps = (m.match(/(\d+) step/) || [])[1];
        add("Made a plan" + (steps ? " (" + steps + " steps)" : "") + " and checked it before starting.", "", e);
        break;
      }
      case "plan_rejected": add("A proposed plan failed the checks, so nothing moved; it asked for a corrected plan.", "alert", e); break;
      case "plan_superseded":
        if (m.includes("goal revised")) add("Dropped the rest of the old plan because the request changed; finished work stays.", "", e);
        else if (!m.includes("cancel")) add("Something wasn't as expected, so it made a new plan.", "", e);
        break;
      case "question_asked": {
        const q = v.questions.find((x) => m.includes("#" + x.question_id + ":"));
        add("Asked you: " + (q ? humanQuestion(q) : m), "you", e);
        break;
      }
      case "question_answered": {
        const q = v.questions.find((x) => m.includes("#" + x.question_id + " "));
        const o = q && q.options.find((x) => x.id === q.answer);
        add("You answered: " + (q && o ? humanOption(q, o) : m), "you", e);
        break;
      }
      case "goal_revised": if (!m.includes("answer to question")) add("You changed the request.", "you", e); break;
      case "control_requested":
        if (m.endsWith("pause")) add("You paused it.", "you", e);
        else if (m.endsWith("cancel")) add("You asked it to stop.", "you", e);
        break;
      case "precondition_failed": add("Something wasn't where it expected, so it did not move and re-planned.", "", e); break;
      case "action_rejected": add("The robot refused a move; nothing moved.", "alert", e); break;
      case "outcome_unknown": add("The robot didn't confirm a move. Checking before doing anything else.", "alert", e); break;
      case "reconciled":
        if (m.includes("not repeated")) add("Checked: that move had already happened, so it was not repeated.", "", e);
        else if (m.includes("same key") || m.includes("no effect")) add("Checked: that move had not happened; it will be done once.", "", e);
        break;
      case "recovery_started": {
        const n = parseInt((m.match(/; (\d+) action/) || [])[1] || "0", 10);
        if (n > 0) add("Started again and first checked what the interrupted move had actually done.", "", e);
        break;
      }
      case "blocked": add("Stopped: it can't confirm what happened, so it won't guess.", "alert", e); break;
      case "inventory_discrepancy": {
        const mm = m.match(/says (\d+) (\w+), observation .* shows (\d+)/);
        add(mm ? "The stock record said " + mm[1] + " " + kindName(mm[2], +mm[1]) + " but it counted " + mm[3] + "; the record was corrected." : "The stock record didn't match what it saw; it was corrected.", "", e);
        break;
      }
      case "policy_denied": add("Refused an action that isn't allowed.", "alert", e); break;
      case "planner_unavailable": add("Lost contact with the AI planner.", "alert", e); break;
      case "stale_plan_rejected": add("Ignored a plan that was made for the old request.", "", e); break;
      case "cancel_acknowledged": add("Stopped. Finished work stays; reserved stock was released.", "", e); break;
      case "action_succeeded": {
        const a = stepById[e.action_id];
        if (a && a.capability === "workspace.place") {
          const dst = (a.arguments || {}).destination_id || "";
          if (dst.startsWith("tray-")) {
            perTray[dst] = (perTray[dst] || 0) + 1;
            if (trayKit[dst] && perTray[dst] === kitSizeFor(v.goal, trayKit[dst])) add("All parts are in " + placeName(dst, trayKit) + ".", "", e);
          }
        }
        break;
      }
      case "run_finished":
        if (m.startsWith("run completed")) add("Finished: every kit was checked by looking at the trays.", "", e);
        else if (m.startsWith("run partial")) add("Finished what it could and reported what's missing.", "", e);
        else if (m.startsWith("run failed")) add("Could not finish.", "alert", e);
        else if (m.includes("baseline agent finished")) add("The agent reported it was finished.", "", e);
        break;
      default: break;
    }
  }
  return lines;
}

function renderStory(v, trayKit) {
  renderStoryLines(storyLines(v, trayKit));
}

function renderStoryLines(lines) {
  const ol = $("story");
  clear(ol);
  for (const l of lines.slice(-40)) ol.append(h("li", { class: l.cls }, l.text, h("span", { class: "t" }, l.t)));
}

// ------------------------------------------------------------------ workspace drawing

function partShape(kind, x, y, r) {
  if (kind === "washer") return s("circle", { cx: x + r, cy: y + r, r: r - 3, class: "washer" });
  if (kind === "red_block" || kind === "blue_block" || kind === "green_block") return s("rect", { x, y, width: 2 * r, height: 2 * r, rx: 3, class: kind.split("_")[0] });
  return s("circle", { cx: x + r, cy: y + r, r: r - 2, class: "unknown" });
}

function drawObjects(g, objs, x, y, w, hgt) {
  const r = 9, gap = 4, per = Math.max(1, Math.floor((w - 12) / (2 * r + gap)));
  objs.forEach((o, i) => {
    const cx = x + 8 + (i % per) * (2 * r + gap);
    const cy = y + 26 + Math.floor(i / per) * (2 * r + gap);
    if (cy + 2 * r > y + hgt) return;
    const kind = o.observed_kind === "unknown" ? "unknown" : o.kind;
    const shape = partShape(kind, cx, cy, r);
    shape.append(s("title", {}, o.observed_kind === "unknown" ? "unidentified item " + o.object_id : kindName(o.kind, 1) + " " + o.object_id));
    g.append(shape);
  });
}

function drawWorkspace(v, trayKit) {
  const svg = $("workspace");
  clear(svg);
  svg.append(s("defs", {},
    s("pattern", { id: "hatch", width: 8, height: 8, patternUnits: "userSpaceOnUse", patternTransform: "rotate(45)" },
      s("rect", { width: 8, height: 8, fill: "rgba(0,0,0,0.25)" }),
      s("line", { x1: 0, y1: 0, x2: 0, y2: 8, stroke: "rgba(0,0,0,0.5)", "stroke-width": 3 }))));
  const t = v.workspace_truth;
  const withheld = new Set(t.evidence_withheld_from_system || []);
  const bins = t.locations.filter((l) => l.loc_type === "bin");
  const trays = t.locations.filter((l) => l.loc_type === "tray").sort((a, b) => trayNum(a.location_id) - trayNum(b.location_id));
  const table = t.locations.find((l) => l.loc_type === "table");
  layoutBoxes = {};
  const report = v.state.final_report;
  const verified = new Set((report && report.claims_source !== "agent" && report.kits_verified_complete) || []);
  const box = (l, x, y, w, hgt, label) => {
    const g = s("g", {});
    layoutBoxes[l.location_id] = { x, y, w, h: hgt };
    g.append(s("rect", { x, y, width: w, height: hgt, rx: 8, "data-slot": l.location_id, class: (l.loc_type === "table" ? "slot table-top" : "slot") + (l.available ? "" : " off") + (verified.has(l.location_id) ? " done" : "") }));
    g.append(s("text", { x: x + 8, y: y + 17 }, label));
    drawObjects(g, l.objects, x, y, w, hgt);
    if (!l.available) g.append(s("text", { x: x + 8, y: y + hgt - 10, class: "off-label" }, "NOT AVAILABLE"));
    if (verified.has(l.location_id)) g.append(s("text", { x: x + w - 72, y: y + hgt - 10, class: "check" }, "✓ checked"));
    if (withheld.has(l.location_id)) {
      g.append(s("rect", { x, y, width: w, height: hgt, rx: 8, class: "hidden-overlay" }));
      g.append(s("text", { x: x + 8, y: y + hgt - 10 }, "hidden from the system"));
    }
    svg.append(g);
  };
  const bw = (880 - (bins.length - 1) * 10) / Math.max(1, bins.length);
  bins.forEach((l, i) => {
    const k = { "bin-red": "red_block", "bin-blue": "blue_block", "bin-green": "green_block", "bin-washer": "washer" }[l.location_id];
    const label = (k ? kindName(k, 2).replace(/^./, (c) => c.toUpperCase()) : l.location_id) + " (" + l.objects.length + ")";
    box(l, 10 + i * (bw + 10), 10, bw, 104, label);
  });
  if (table) box(table, 10, 124, 330, 96, "Table (" + table.objects.length + ")");
  const tw = (880 - (trays.length - 1) * 10) / Math.max(1, trays.length);
  trays.forEach((l, i) => box(l, 10 + i * (tw + 10), 232, tw, 138,
    trayKit[l.location_id] ? "Kit " + trayKit[l.location_id] + " · tray " + trayNum(l.location_id) : "Tray " + trayNum(l.location_id) + (l.objects.length || v.program ? "" : " (spare)")));
  drawArm(svg, v);
}

// ------------------------------------------------------------------ programs (Revolve Script)

let examples = [];
let reviewing = null; // the proposal shown in the start review card
let revision = null; // a proposed replacement for a running program

function placeLabel(id) {
  if (!id) return "?";
  if (id === "table") return "the table";
  if (id === "gripper") return "the arm";
  if (id.startsWith("bin-")) return "the " + id.slice(4) + " bin";
  if (id.startsWith("tray-")) return "tray " + id.slice(5);
  return id;
}

function partLabel(id) {
  const k = kindOf(id);
  return k === "unknown" ? "the unidentified part " + id : "a " + kindName(k, 1) + " (" + id + ")";
}

const KEYWORDS = new Set(["if", "elif", "else", "for", "in", "not", "and", "or", "def", "return", "break", "continue", "pass", "expect", "is", "True", "False", "None"]);
const BUILTIN_FNS = new Set(["parts", "places", "count", "holding", "move", "pick", "place", "ask", "say", "print", "len", "range", "str", "int", "min", "max", "sum", "abs", "sorted"]);

// Syntax colouring by tokens; all text goes in via text nodes.
function codeLine(text) {
  const out = [];
  const re = /(#.*$)|("(?:[^"\\]|\\.)*"?|'(?:[^'\\]|\\.)*'?)|(\b\d+\b)|([A-Za-z_][A-Za-z0-9_]*)|(\s+)|(.)/g;
  let m;
  while ((m = re.exec(text)) !== null) {
    if (m[1]) out.push(h("span", { class: "com" }, m[1]));
    else if (m[2]) out.push(h("span", { class: "str" }, m[2]));
    else if (m[3]) out.push(h("span", { class: "num-lit" }, m[3]));
    else if (m[4]) out.push(KEYWORDS.has(m[4]) ? h("span", { class: "kw" }, m[4]) : BUILTIN_FNS.has(m[4]) ? h("span", { class: "fn" }, m[4]) : m[4]);
    else out.push(m[0]);
  }
  return out;
}

function codeView(source) {
  const pre = h("pre", { class: "code" });
  source.split("\n").forEach((line, i) => {
    pre.append(h("span", { class: "ln", "data-line": i + 1 }, h("span", { class: "num" }, String(i + 1)), ...codeLine(line)));
  });
  return pre;
}

function markLines(pre, marks) {
  for (const el of pre.querySelectorAll(".ln")) {
    const n = +el.getAttribute("data-line");
    el.className = "ln" + (marks.err && marks.err.has(n) ? " err" : "") + (marks.cur === n ? " cur" : marks.ran && marks.ran.has(n) ? " ran" : "");
  }
}

const EFFECT_WORDS = { look: "look at the table", move: "move parts", pick: "pick parts up", place: "put parts down", ask: "ask you questions", say: "show you messages" };
const IMPORT_WORDS = { "cap.observe": "looking", "cap.pick": "picking up", "cap.place": "putting down", "human.ask": "asking you", "human.say": "messages" };

function checksList(c, target) {
  const ul = h("ul", { class: "checks" });
  const f = c.facts || {};
  const li = (cls, text, detail) => ul.append(h("li", { class: cls }, text, detail ? h("span", { class: "detail" }, detail) : null));
  li("ok", "Reads correctly: " + c.lines + " lines", "Every name, part kind and place was checked against the table.");
  li("ok", "It can only " + listJoin((f.effects || []).map((e) => EFFECT_WORDS[e] || e)), "Anything else is impossible: the compiled program has no other way out.");
  const named = f.places_named || [];
  li("ok", named.length ? "Places it names: " + listJoin(named.map(placeLabel)) : "It names no places directly",
    f.dynamic_destinations ? "Places chosen while it runs are checked by the safety policy before every move." : null);
  li("ok", "Always finishes", "No endless loops and no recursion" + (f.loops ? " (" + f.loops + " loop" + (f.loops === 1 ? "" : "s") + ", each over a fixed list)." : "."));
  for (const q of f.questions || []) li("warn", "It may ask you: “" + q + "”");
  li("ok", "Says how to check success: " + (f.expectations || []).length + " check" + ((f.expectations || []).length === 1 ? "" : "s"), (f.expectations || []).map((e) => "• " + e).join("  "));
  li("ok", "Compiled to machine code", c.wasm_bytes + " bytes of WebAssembly → native " + (target || "") + " code. It can reach only: " + listJoin((c.imports || []).map((i) => IMPORT_WORDS[i] || i)) + ". sha256 " + (c.sha256 || "").slice(0, 12) + "…");
  return ul;
}

function endStateChips(r) {
  const by = {};
  for (const o of r.end_state || []) {
    if (o.at === "outside") continue;
    (by[o.at] = by[o.at] || []).push(o.kind);
  }
  const chips = h("div", { class: "chips" });
  for (const [at, kinds] of Object.entries(by).sort()) {
    const counts = {};
    for (const k of kinds) counts[k] = (counts[k] || 0) + 1;
    const txt = Object.entries(counts).map(([k, n]) => n + " " + (k === "unknown" ? (n === 1 ? "unidentified part" : "unidentified parts") : kindName(k, n))).join(", ");
    chips.append(h("span", {}, placeLabel(at).replace(/^the /, "").replace(/^./, (c) => c.toUpperCase()) + ": " + txt));
  }
  return chips;
}

function rehearsalView(r) {
  const box = h("div", {});
  if (!r) return box;
  const rep = r.report || {};
  const exps = rep.expectations || [];
  const held = exps.filter((e) => e.held).length;
  const ul = h("ul", { class: "checks" });
  if (r.status === "completed") ul.append(h("li", { class: "ok" }, "Rehearsed on a copy of the table: " + r.moves.length + " move" + (r.moves.length === 1 ? "" : "s") + ", all " + exps.length + " checks passed", h("span", { class: "detail" }, "Took " + (r.duration_ms / 1000).toFixed(1) + " s. Nothing real moved.")));
  else if (r.status === "partial") ul.append(h("li", { class: "warn" }, "Rehearsal: " + held + " of " + exps.length + " checks passed", h("span", { class: "detail" }, "Read the checks below before running it.")));
  else ul.append(h("li", { class: "bad" }, "The rehearsal stopped: " + (r.reason || r.status)));
  for (const e of exps) ul.append(h("li", { class: e.held ? "ok" : "bad" }, e.description));
  for (const q of r.questions || []) ul.append(h("li", { class: "warn" }, "It will ask you: “" + q.question + "”", h("span", { class: "detail" }, "The rehearsal assumed: " + q.assumed)));
  box.append(ul, h("p", { class: "muted small" }, "Where things will end up:"), endStateChips(r));
  return box;
}

function diagList(diags) {
  const box = h("div", {});
  for (const d of diags || []) box.append(h("p", { class: "diag" }, "Line " + d.line + ": " + d.message));
  return box;
}

function metaLine(r) {
  const bits = [];
  if (r.by) bits.push("written by " + r.by);
  if (r.seconds) bits.push(r.seconds + " s");
  if (r.cost_usd_estimate != null) bits.push("about $" + Number(r.cost_usd_estimate).toFixed(3));
  const fixes = (r.attempts || []).filter((a) => !a.accepted).length;
  if (fixes && r.accepted) bits.push("the checker caught problems " + fixes + "× and the AI fixed them");
  return bits.join(" · ");
}

// A proposed program: code, what was checked, the rehearsal, and what to do next.
function renderReview(card, r, opts) {
  clear(card);
  card.hidden = false;
  const head = h("div", { class: "prog-head" }, h("h2", {}, opts.title || "The program"), h("span", { class: "muted small" }, metaLine(r)));
  card.append(head);
  if (r.explanation) card.append(h("p", { class: "explain" }, r.explanation));
  if (r.understood === false && r.not_possible) card.append(h("p", { class: "diag" }, "Not possible here: " + r.not_possible));
  const buttons = h("div", { class: "buttons" });
  if (!r.source) {
    card.append(buttons);
    buttons.append(actionButton("Change my words", opts.onChangeWords, "ghost"));
    return;
  }
  const left = h("div", {});
  const pre = codeView(r.source);
  const edit = h("textarea", { class: "code-edit", hidden: true, spellcheck: "false" });
  edit.value = r.source;
  left.append(pre, edit);
  const right = h("div", {});
  if (r.accepted) {
    right.append(h("h3", {}, "Checked before anything moves"), checksList(r.compiled, r.native_target));
    right.append(h("h3", {}, "Rehearsal"), rehearsalView(r.rehearsal));
    if (cell3d && r.rehearsal && r.rehearsal.motions && r.rehearsal.motions.length) {
      right.append(h("p", { class: "muted small" }, "Watch the see-through arm in the robot cell below: it replays this rehearsal, then fades, leaving the parts where they will end up."));
    }
  } else {
    right.append(h("h3", {}, "The checker found problems"), diagList(r.diagnostics), h("p", { class: "muted small" }, "Nothing ran. Change your words, or edit the program and check it again."));
    markLines(pre, { err: new Set((r.diagnostics || []).map((d) => d.line)) });
  }
  card.append(h("div", { class: "prog-grid" }, left, right), buttons);
  const canRun = r.accepted && r.rehearsal && r.rehearsal.status !== "blocked" && r.rehearsal.status !== "failed";
  if (canRun) buttons.append(actionButton(opts.runLabel || "Run it for real", () => { if (cell3d) window.RevolveCell.stopPreview(); opts.onRun(r); }, "big"));
  if (r.rehearsal && r.rehearsal.motions && r.rehearsal.motions.length && cell3d) {
    buttons.append(actionButton("Replay the rehearsal", () => { previewRehearsal(r.rehearsal); $("cell-card").scrollIntoView({ behavior: "smooth", block: "start" }); }, "ghost"));
    previewRehearsal(r.rehearsal);
  }
  buttons.append(actionButton("Edit the program", () => {
    const editing = edit.hidden;
    edit.hidden = !editing;
    pre.hidden = editing;
    if (editing) edit.focus();
  }, "ghost"));
  buttons.append(actionButton("Check my changes", async () => {
    try {
      const out = await api("POST", "/api/programs/check", Object.assign({ source: edit.value }, opts.where()));
      renderReview(card, Object.assign(out, { source: edit.value, explanation: r.explanation, by: "you (edited)" }), opts);
    } catch (e) { toast(e.message); }
  }, "ghost"));
  if (opts.onChangeWords) buttons.append(actionButton(opts.changeLabel || "Change my words", opts.onChangeWords, "ghost"));
}

function writingState(card, what) {
  clear(card);
  card.hidden = false;
  const t0 = Date.now();
  const secs = h("span", { class: "muted small" }, "");
  card.append(h("div", { class: "writing" }, h("span", { class: "dots" }, what), secs));
  card.append(h("p", { class: "muted small" }, "Then the compiler checks it, compiles it to machine code and rehearses it on a copy of the table. Usually 10–40 seconds."));
  const timer = setInterval(() => { if (!card.contains(secs)) return clearInterval(timer); secs.textContent = Math.round((Date.now() - t0) / 1000) + " s"; }, 500);
  return () => clearInterval(timer);
}

function startWhere() { return { scenario: $("scenario-select").value }; }

const sleep = (ms) => new Promise((res) => setTimeout(res, ms));

const PHASES = {
  starting: "Starting",
  writing: "The AI is writing the program",
  checking: "Checking it against the table and compiling it to machine code",
  rehearsing: "Rehearsing it on a copy of the table",
};

// Only one writing job at a time; a newer one makes older loops stop drawing.
let writeGen = 0;
let writingNow = false;

// The writing card is built once per job and then only its texts change, so it
// does not flicker while the program streams in.
function writingCard(card, title) {
  clear(card);
  card.hidden = false;
  const w = {
    secs: h("span", { class: "muted small" }, "0 s"),
    phase: h("span", { class: "dots" }, PHASES.starting),
    hint: h("p", { class: "muted small" }, "It thinks first (usually 5–15 seconds), then the program appears here as it is written."),
    problems: h("div", {}),
    explain: h("p", { class: "explain" }),
    code: h("div", {}),
    program: null,
    t0: Date.now(),
  };
  card.append(h("div", { class: "prog-head" }, h("h2", {}, title), w.secs), h("div", { class: "writing" }, w.phase), w.hint, w.problems, w.explain, w.code);
  return w;
}

function updateWriting(w, st) {
  w.secs.textContent = Math.round((Date.now() - w.t0) / 1000) + " s";
  const repairing = st.phase === "writing" && st.problems && st.problems.length;
  const phase = st.phase === "writing" && st.cut_off ? "The answer was too long and got cut off; the AI is writing a shorter one"
    : repairing ? "The checker found problems; the AI is fixing them" : PHASES[st.phase] || st.phase;
  if (w.phase.textContent !== phase) w.phase.textContent = phase;
  w.hint.hidden = !(st.phase === "writing" && !st.explanation && !st.program);
  const probs = (st.problems || []).join("\n");
  if (w.problems.getAttribute("data-p") !== probs) {
    w.problems.setAttribute("data-p", probs);
    clear(w.problems);
    for (const p of st.problems || []) w.problems.append(h("p", { class: "diag" }, p));
  }
  if (st.explanation && w.explain.textContent !== st.explanation) w.explain.textContent = st.explanation;
  if (st.program && st.program !== w.program) {
    w.program = st.program;
    const pre = codeView(st.program);
    pre.classList.add("typing");
    clear(w.code);
    w.code.append(pre);
    pre.scrollTop = pre.scrollHeight;
  }
}

// Start a writing job and follow it until it is done (or a newer job replaces it).
async function followJob(body, onUpdate) {
  const gen = ++writeGen;
  const { job } = await api("POST", "/api/programs/write", body);
  for (;;) {
    await sleep(250);
    if (gen !== writeGen) throw new Error("replaced by a newer request");
    const st = await api("GET", "/api/programs/jobs/" + encodeURIComponent(job));
    if (gen !== writeGen) throw new Error("replaced by a newer request");
    if (st.phase === "done") return st.result;
    if (st.phase === "error") throw new Error(st.error);
    onUpdate(st);
  }
}

async function followWriteJob(body, card, title) {
  const w = writingCard(card, title);
  return followJob(body, (st) => updateWriting(w, st));
}

function setWriting(on) {
  writingNow = on;
  const b = $("start-btn");
  b.disabled = on;
  if (on) b.textContent = "Writing…";
  else syncStartButton();
  const d = $("do-it");
  d.disabled = on;
  d.textContent = on ? "Working…" : "Do it";
}

// ------------------------------------------------------------------ the simple view

const TERMINAL = ["completed", "partial", "stopped", "failed"];

function hud(status, detail, code, actions) {
  $("hud-status").textContent = status || "";
  $("hud-detail").textContent = detail || "";
  const c = $("hud-code");
  c.hidden = !code;
  if (code && c.textContent !== code) {
    c.textContent = code;
    c.scrollTop = c.scrollHeight;
  }
  $("hud-question").hidden = true;
  // Rebuild buttons only when they change, so a tap is never lost to a refresh.
  const a = $("hud-actions");
  const key = (actions || []).map((x) => x[0]).join("|");
  if (a.getAttribute("data-k") !== key) {
    a.setAttribute("data-k", key);
    clear(a);
    for (const [label, , cls] of actions || []) {
      const b = h("button", { type: "button", class: cls || "ghost" }, label);
      b.addEventListener("click", () => b._fn && b._fn());
      a.append(b);
    }
  }
  [...a.children].forEach((b, i) => { b._fn = actions[i][1]; });
}

// A message that stays until the person has seen it (why nothing moved): until
// OK, a new request, or the run needs them.
let hudNotice = null;

function showNotice(title, detail, code) {
  hudNotice = { title };
  hud(title, detail, code, [["OK", () => { hudNotice = null; if (view && view.program) renderHud(view); else hudIdle(); }]]);
}

function hudIdle() {
  hud("Tell the robot what to do.", "Type a request above and press Do it. The AI writes a program, it is compiled to machine code and the arm runs it here.");
}

// The overlay on the 3D cell for a program run.
function renderHud(v) {
  if (writingNow) return; // a writing job owns the overlay
  const st = v.state.status;
  const calls = programCalls(v);
  const last = calls[calls.length - 1];
  const src = ((v.program && v.program.source) || "").split("\n");
  const codeLine = last && last.line ? (src[last.line - 1] || "").trim() : "";
  const active = v.executor.active;
  const crashed = !active && v.executor.last_exit_code === v.executor.simulated_crash_code;
  const start = () => act("start", {}, true);
  const finished = v.events.find((e) => e.kind === "program_finished"); // events are newest first
  if (hudNotice) {
    if (!(st === "awaiting_human" || st === "blocked" || crashed)) return;
    hudNotice = null;
  }
  const checks = (rep) => ((rep && rep.expectations) || []).map((e) => (e.held ? "✓ " : "✗ ") + e.description).join("\n");
  // What the arm would not do (with the reason) and what the program said.
  const extra = (rep) => [
    ...((rep && rep.refusals) || []).map((x) => "✗ Not done: " + (x.why || "refused")),
    ...((rep && rep.messages) || []).slice(-3).map((m) => "💬 " + m),
  ].join("\n");
  const summary = (rep) => [checks(rep), extra(rep)].filter(Boolean).join("\n\n");
  if ((st === "running" || st === "paused") && crashed) {
    hud("The power went out (simulated)", "The arm stopped where it was. Continue: it first checks what the last move did, then carries on. Nothing is done twice.", "", [["Continue", start, "big"]]);
  } else if (st === "running" && active) {
    const refused = last && last.status === "failed";
    hud(last ? "Line " + last.line + " · " + (refused ? "Not done: " : "") + describeCall(last) : "Starting the program", refused ? last.error || "" : "", codeLine, [["Pause", () => act("pause", {}, true)]]);
  } else if (st === "running" || st === "created") {
    hud("Ready", "", "", [["Continue", start, "big"]]);
  } else if (st === "paused") {
    hud("Paused", "Nothing new will start until you continue. You can also type a new request.", codeLine, [["Continue", start, "big"]]);
  } else if (st === "awaiting_human") {
    const qn = v.questions.find((x) => x.status === "open" && x.goal_revision === v.goal_revision);
    hud("The program is asking you", "", codeLine);
    if (qn) {
      $("hud-question").hidden = false;
      $("hud-q").textContent = qn.prompt;
      const o = $("hud-opts");
      if (o.getAttribute("data-q") !== String(qn.question_id)) {
        o.setAttribute("data-q", String(qn.question_id));
        clear(o);
        for (const opt of qn.options) o.append(actionButton(opt.label, () => act("answer", { question_id: qn.question_id, choice: opt.id }, true)));
      }
    }
  } else if (st === "awaiting_planner" && finished) {
    const rep = finished.data || {};
    const exps = rep.expectations || [];
    const held = exps.filter((e) => e.held).length;
    hud(held === exps.length ? "Done ✓ " + held + " of " + exps.length + " checks passed" : "Done, but " + (exps.length - held) + " of " + exps.length + " checks did not pass",
      summary(rep) + "\n\nChecked by looking at the table. Tell it what to do next.");
  } else if (st === "awaiting_planner") {
    hud("Waiting for a request", v.state.status_reason || "");
  } else if (st === "blocked") {
    hud("Stopped to check", v.state.status_reason || "", codeLine, [["Continue", start, "big"]]);
  } else if (TERMINAL.includes(st)) {
    const rep = v.state.final_report || {};
    hud(st === "completed" ? "Done ✓" : st === "partial" ? "Done, but not every check passed" : st === "stopped" ? "Stopped" : "Could not finish", summary(rep) || v.state.status_reason || "");
  }
}

function hudWriting(st, t0) {
  const secs = Math.round((Date.now() - t0) / 1000) + " s";
  const repairing = st.phase === "writing" && st.problems && st.problems.length;
  const phase = st.phase === "writing" && st.cut_off ? "The answer was too long and got cut off; the AI is writing a shorter one"
    : repairing ? "The checker found problems; the AI is fixing them" : st.phase === "checking" ? "Checking and compiling to machine code" : "The AI is writing the program";
  const detail = st.explanation || (st.program ? "" : "It thinks first (usually 5–15 seconds), then the code appears here.");
  hud(phase + " · " + secs, detail, st.program || "");
}

// Continue on the table shown, or start a fresh one.
function sessionRun() {
  return current && view && view.program && !TERMINAL.includes(view.state.status) && view.meta.scenario_id === $("scenario-select").value ? current : null;
}

// Do it: the AI writes the program, it is checked and compiled, and it runs at once.
async function directRun(text, example) {
  if (writingNow) return;
  hudNotice = null;
  setWriting(true);
  if (cell3d) window.RevolveCell.stopPreview();
  const session = sessionRun();
  const t0 = Date.now();
  try {
    let r;
    if (example) {
      r = { accepted: true, source: example.source, explanation: example.explanation, by: "example (no AI)" };
    } else {
      hudWriting({ phase: "starting" }, t0);
      const body = Object.assign({ text, rehearse: false }, session ? { run_id: session } : { scenario: $("scenario-select").value });
      r = await followJob(body, (st) => hudWriting(st, t0));
    }
    if (!r.accepted) {
      const declined = r.understood === false;
      const why = declined ? r.not_possible || "The AI could not see how to do that here."
        : (r.diagnostics || []).map((d) => "line " + d.line + ": " + d.message).join("\n") || "The program did not pass the checker.";
      showNotice(declined ? "The robot won't do that · nothing moved" : "The program did not pass the checker · nothing moved",
        why + (r.paused_run ? "\n\nThe previous program carries on." : ""), declined ? "" : r.source || "");
      if (r.paused_run) await act("start", {}, true);
      return;
    }
    hud("Compiled to machine code · starting", r.explanation || "", r.source);
    const program = { source: r.source, explanation: r.explanation || "", request: text, author: r.by || "" };
    if (session) {
      await act("program", { program }, true);
    } else {
      const out = await api("POST", "/api/runs", { scenario: $("scenario-select").value, session: true, program });
      await loadRuns(out.run_id);
      showMode();
      history.replaceState(null, "", "#run=" + encodeURIComponent(out.run_id));
      await act("start", {}, true);
    }
    $("ask").value = "";
  } catch (e) {
    if (e.message !== "replaced by a newer request") showNotice("Couldn't do that · nothing moved", e.message);
  } finally {
    setWriting(false);
  }
}

function doIt() {
  const text = $("ask").value.trim();
  if (!text) { $("ask").focus(); return; }
  if (!isProgramStyle()) {
    // The earlier step-list flow lives in the Advanced view.
    setAdvanced(true);
    $("new-request").value = text;
    $("new-run").requestSubmit();
    return;
  }
  if ($("direct-run").checked) return directRun(text);
  return writeProgram(text);
}

function setAdvanced(on) {
  document.body.classList.toggle("simple-ui", !on);
  $("adv-toggle").textContent = on ? "Simple view" : "Advanced";
  try { localStorage.setItem("revolve-advanced", on ? "1" : "0"); } catch { /* not needed to work */ }
}

async function writeProgram(text) {
  if (writingNow) return; // one request at a time
  setWriting(true);
  if (cell3d) window.RevolveCell.stopPreview();
  const card = $("program-card");
  card.scrollIntoView({ behavior: "smooth", block: "nearest" });
  try {
    const r = await followWriteJob(Object.assign({ text }, startWhere()), card, "The program");
    reviewing = Object.assign(r, { request: text });
    renderReview(card, reviewing, { where: startWhere, onRun: runProgram, onChangeWords: () => { card.hidden = true; $("ask").focus(); } });
  } catch (e) {
    if (e.message !== "replaced by a newer request") {
      clear(card);
      card.append(h("p", { class: "diag" }, e.message), h("div", { class: "buttons" }, actionButton("Try again", () => { card.hidden = true; doIt(); }, "big"), actionButton("Change my words", () => { card.hidden = true; $("ask").focus(); }, "ghost")));
    }
  } finally {
    setWriting(false);
  }
}

async function checkExample(ex) {
  const card = $("program-card");
  const stop = writingState(card, "Checking the example “" + ex.title + "”");
  try {
    const r = await api("POST", "/api/programs/check", Object.assign({ source: ex.source }, startWhere()));
    stop();
    reviewing = Object.assign(r, { source: ex.source, explanation: ex.explanation || ex.title, by: "example (no AI)", request: ex.title });
    renderReview(card, reviewing, { where: startWhere, onRun: runProgram, onChangeWords: () => { card.hidden = true; } , changeLabel: "Close" });
    card.scrollIntoView({ behavior: "smooth", block: "start" });
  } catch (e) { stop(); toast(e.message); }
}

async function runProgram(r) {
  try {
    const out = await api("POST", "/api/runs", {
      scenario: $("scenario-select").value,
      program: { source: r.source, explanation: r.explanation || "", request: r.request || "", author: r.by || "", rehearsal: r.rehearsal || null },
    });
    $("program-card").hidden = true;
    $("new-request").value = "";
    await loadRuns(out.run_id);
    showMode();
    history.replaceState(null, "", "#run=" + encodeURIComponent(out.run_id));
    await act("start", {}, true);
    toast("Running.");
    $("now-card").scrollIntoView({ behavior: "smooth", block: "start" });
  } catch (e) { toast("Could not start: " + e.message); }
}

async function loadExamples() {
  try { examples = await api("GET", "/api/programs/examples"); } catch { examples = []; }
  const box = $("examples");
  clear(box);
  if (!examples.length) return;
  box.append(h("span", { class: "muted small" }, "Or try an example (no AI needed):"));
  for (const ex of examples) box.append(actionButton(ex.title, () => ($("direct-run").checked ? directRun(ex.title, ex) : checkExample(ex)), "ghost"));
}

// ---- a running program

function programCalls(v) { return (v.program && v.program.calls) || []; }

function describeCall(c) {
  const a = c.args || {};
  switch (c.capability) {
    case "workspace.observe": return "Looking at the table";
    case "workspace.pick": return "Picking up " + partLabel(a.object_id) + (a.source_id ? " from " + placeLabel(a.source_id) : "");
    case "workspace.place": return "Putting " + partLabel(a.object_id) + " in " + placeLabel(a.destination_id);
    case "human.ask": return "Asking you: " + (a.question || "");
    case "human.say": return "Says: " + (a.message || "");
    case "expect": return "Checking: " + (a.description || "");
    default: return c.capability;
  }
}

function renderRunningProgram(v) {
  const card = $("run-program-card");
  const p = v.program;
  if (!p || !p.source) { card.hidden = true; return; }
  card.hidden = false;
  const key = p.plan_seq + ":" + p.source.length;
  if (card.getAttribute("data-key") !== key) {
    card.setAttribute("data-key", key);
    clear(card);
    const built = p.built || {};
    const meta = ["program #" + p.plan_seq, p.author ? "by " + p.author : "", built.bytes ? "compiled: " + built.bytes + " bytes WebAssembly → native code" : ""].filter(Boolean).join(" · ");
    card.append(h("div", { class: "prog-head" }, h("h2", {}, "The program"), h("span", { class: "muted small" }, meta)));
    if (p.request) card.append(h("p", { class: "muted small" }, "You asked: “" + p.request + "”"));
    if (p.explanation) card.append(h("p", { class: "explain" }, p.explanation));
    const right = h("div", { id: "run-program-side" });
    card.append(h("div", { class: "prog-grid" }, h("div", {}, codeView(p.source)), right));
  }
  const pre = card.querySelector("pre.code");
  const calls = programCalls(v);
  const running = v.executor.active && v.state.status === "running";
  const last = calls[calls.length - 1];
  const ran = new Set(calls.filter((c) => c.status === "succeeded" || c.status === "failed").map((c) => c.line));
  const cur = last && (running || !["completed", "partial", "stopped"].includes(v.state.status)) ? last.line : null;
  markLines(pre, { ran, cur });
  // Side: the checks it states and what happened to them, then what was verified before it ran.
  const side = card.querySelector("#run-program-side");
  clear(side);
  const exps = calls.filter((c) => c.capability === "expect");
  const facts = (p.built && p.built.facts) || {};
  side.append(h("h3", {}, "How success is checked"));
  const ul = h("ul", { class: "checks" });
  for (const d of facts.expectations || []) {
    const got = exps.find((e) => e.args.description === d);
    ul.append(h("li", { class: got ? (got.args.held ? "ok" : "bad") : "warn" }, d, h("span", { class: "detail" }, got ? (got.args.held ? "Held when checked, by looking at the table." : "Did not hold when checked.") : "Not checked yet.")));
  }
  side.append(ul);
  const moves = calls.filter((c) => c.capability === "workspace.place" && c.status === "succeeded").length;
  const refused = calls.filter((c) => (c.capability === "workspace.place" || c.capability === "workspace.pick") && c.status === "failed").length;
  side.append(h("p", { class: "muted small" }, moves + " part" + (moves === 1 ? "" : "s") + " placed" + (refused ? ", " + refused + " move" + (refused === 1 ? "" : "s") + " could not be done" : "") + " · " + calls.length + " program steps logged"));
  if (p.built && p.built.facts) {
    const det = h("details", {}, h("summary", {}, "What was checked before it ran"));
    det.append(checksList({ facts: p.built.facts, lines: p.source.split("\n").filter((l) => l.trim()).length, wasm_bytes: p.built.bytes, imports: p.built.imports, sha256: p.built.sha256 }, ""));
    if (p.rehearsal) det.append(h("h3", {}, "Rehearsal"), rehearsalView(p.rehearsal));
    side.append(det);
  }
}

function renderNowProgram(v) {
  const st = v.state.status;
  const actions = $("now-actions");
  clear(actions);
  $("question-box").hidden = true;
  const title = (t, d) => { $("now-title").textContent = t; $("now-detail").textContent = d || ""; };
  const start = () => act("start", {}, true);
  const calls = programCalls(v);
  const last = calls[calls.length - 1];
  const moves = calls.filter((c) => c.capability === "workspace.place" && c.status === "succeeded").length;
  $("now-progress").textContent = moves + " part" + (moves === 1 ? "" : "s") + " placed";
  $("progress-bar").hidden = true;
  const active = v.executor.active;
  const crashed = !active && v.executor.last_exit_code === v.executor.simulated_crash_code;
  const report = v.state.final_report || {};
  const exps = report.expectations || [];
  const powerCut = () => {
    const counts = (v.workspace_truth && v.workspace_truth.effect_counts) || {};
    act("sim", { control: "arm_fault", fault: { kind: "crash_after_effect", on: { capability: "workspace.place", nth: (counts["workspace.place"] || 0) + 1 } } }, true)
      .then((ok) => { if (ok) toast("The power will fail right after the next part is put down."); });
  };
  $("change-box").hidden = ["completed", "partial", "stopped", "failed"].includes(st);
  setTimeout(() => { $("cell-hud").textContent = $("now-title").textContent; }, 0);
  if (st === "created") {
    setBadge("idle", "Ready");
    title("Ready to run the program", "Press Start.");
    actions.append(actionButton("Start", start, "big"));
  } else if ((st === "running" || st === "paused") && crashed) {
    setBadge("warn", "Power cut");
    title("The power went out (simulated)", "Press Continue. It first checks what the last move actually did, then the program picks up exactly where it was. Nothing is done twice.");
    actions.append(actionButton("Continue", start, "big"));
  } else if (st === "running" && !active) {
    setBadge("idle", "Not running");
    title("Nothing is running right now", "Press Continue to carry on from where it stopped.");
    actions.append(actionButton("Continue", start, "big"));
  } else if (st === "running") {
    setBadge("info", "Running");
    title(last ? "Line " + last.line + ": " + describeCall(last) : "Checking and compiling the program", last ? "The highlighted line in the program below is running now." : "");
    actions.append(actionButton("Simulate a power cut", powerCut, "ghost"), actionButton("Pause", () => act("pause"), "ghost"), actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "awaiting_human") {
    setBadge("info", "Needs your decision");
    const qn = v.questions.find((x) => x.status === "open" && x.goal_revision === v.goal_revision);
    title("The program is asking you", "Nothing more will move until you answer.");
    if (qn) {
      $("question-box").hidden = false;
      $("question-prompt").textContent = qn.prompt;
      const opts = $("question-options");
      clear(opts);
      for (const o of qn.options) opts.append(actionButton(o.label, () => act("answer", { question_id: qn.question_id, choice: o.id })));
    }
    actions.append(actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "paused") {
    setBadge("idle", "Paused");
    title("Paused: nothing new will start", "Anything that was moving has finished and been checked.");
    actions.append(actionButton("Continue", start, "big"), actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "blocked") {
    setBadge("bad", "Stopped to check");
    title("It stopped because it can't safely continue", v.state.status_reason || "");
    actions.append(actionButton("Continue", start, "big"), actionButton("Stop", () => act("cancel"), "danger"));
  } else if (st === "awaiting_planner") {
    setBadge("violet", "Waiting");
    title("Waiting for a program", v.state.status_reason || "");
  } else if (st === "completed") {
    setBadge("ok", "Done");
    title("Done: all " + exps.length + " check" + (exps.length === 1 ? "" : "s") + " passed", "Each check was made by looking at the table, not by trusting the program. " + moves + " parts were placed.");
  } else if (st === "partial") {
    const failed = exps.filter((e) => !e.held);
    setBadge("warn", "Done, but not everything");
    title(failed.length + " of " + exps.length + " checks did not pass", failed.map((e) => "✗ " + e.description).join("  "));
  } else if (st === "stopped") {
    setBadge("idle", "Stopped");
    title("Stopped as you asked", "Parts already moved stay where they are.");
  } else if (st === "failed") {
    setBadge("bad", "Could not finish");
    title("It could not finish", v.state.status_reason || "");
  }
  if (["completed", "partial", "stopped", "failed"].includes(st)) {
    actions.append(actionButton("New request", () => { newRequest(); window.scrollTo({ top: 0, behavior: "smooth" }); }, "ghost"));
  }
}

function programStory(v) {
  const events = v.events.slice().reverse();
  const base = events.length ? events[0].at_ms : 0;
  const lines = [];
  const add = (text, cls, e) => lines.push({ text, cls, t: e ? ((e.at_ms - base) / 1000).toFixed(0) + "s" : "" });
  for (const e of events) {
    const m = e.message || "";
    switch (e.kind) {
      case "program_submitted": add(m.includes("first program") ? "You approved the program and started it." : "You asked for a change; a new program was approved.", "you", e); break;
      case "program_built": { const b = (m.match(/\((\d+) bytes/) || [])[1]; add("Checked the program against the table and compiled it to machine code" + (b ? " (" + b + " bytes)." : "."), "", e); break; }
      case "plan_rejected": add("The program no longer passes its checks for the table as it is now; nothing ran.", "alert", e); break;
      case "plan_superseded": if (m.includes("goal revised")) add("Stopped the old program at a safe point; moves already made stay.", "", e); break;
      case "action_rejected": add("A move could not be done: " + m.replace(/^L\d+\.\d+: /, "").replace(/; no effect$/, "") + ". Nothing moved.", "alert", e); break;
      case "program_says": add("The program says: " + m, "", e); break;
      case "expectation_held": add(m.replace(" (checked by looking at the table)", " (checked by looking)"), "", e); break;
      case "expectation_failed": add(m, "alert", e); break;
      case "question_asked": add("The program asked you: " + m.replace(/^question #\d+: /, ""), "you", e); break;
      case "question_answered": add("You answered.", "you", e); break;
      case "recovery_started": if (/; [1-9]\d* action/.test(m)) add("Power back: it first checked what the interrupted move had done, then the program continued from the same line.", "", e); break;
      case "reconciled": if (m.includes("not repeated")) add("Checked: that move had already happened, so it was not repeated.", "", e); break;
      case "control_requested": if (m.endsWith("pause")) add("You paused it.", "you", e); else if (m.endsWith("cancel")) add("You asked it to stop.", "you", e); break;
      case "program_error": add("The program hit an error: " + m.replace(/^program #\d+ /, ""), "alert", e); break;
      case "run_finished": add(m.startsWith("run completed") ? "Finished: every check passed, by looking at the table." : m.startsWith("run partial") ? "Finished, but not every check passed." : "Finished.", m.startsWith("run completed") ? "" : "alert", e); break;
      default: break;
    }
  }
  return lines;
}

async function writeRevision() {
  const text = $("change-text").value.trim();
  if (!text || !current) return toast("Say what should change.");
  if (writingNow) return;
  setWriting(true);
  const card = $("revision-card");
  card.scrollIntoView({ behavior: "smooth", block: "nearest" });
  const where = () => ({ run_id: current });
  try {
    const r = await followWriteJob({ text, run_id: current }, card, "Paused. A new program for: “" + text + "”");
    revision = Object.assign(r, { request: text });
    renderReview(card, revision, {
      title: "A new program for: “" + text + "”",
      where,
      runLabel: "Switch to this program",
      onRun: async (rv) => {
        const ok = await act("program", { program: { source: rv.source, explanation: rv.explanation || "", request: rv.request || text, author: rv.by || "", rehearsal: rv.rehearsal || null } });
        if (ok) { card.hidden = true; $("change-text").value = ""; }
      },
      changeLabel: "Keep the current program",
      onChangeWords: async () => { card.hidden = true; if (r.paused_run) await act("start", {}, true); },
    });
  } catch (e) {
    if (e.message === "replaced by a newer request") return;
    clear(card);
    card.append(h("p", { class: "diag" }, e.message), h("div", { class: "buttons" }, actionButton("Close and continue", async () => { card.hidden = true; if (view && view.state.status === "paused") await act("start", {}, true); }, "ghost")));
  } finally {
    setWriting(false);
  }
}

// ---- the robot arm (two joints; reaches for the place of the current move)

const ARM = { bx: 470, by: 178, l1: 250, l2: 235 };
let armCur = null; // set on first draw
let armAnim = null;
let armTargetKey = "";
let layoutBoxes = {};

function ik(tx, ty) {
  const dx = tx - ARM.bx, dy = ty - ARM.by;
  const d = Math.min(Math.max(Math.hypot(dx, dy), 40), ARM.l1 + ARM.l2 - 2);
  const c2 = (d * d - ARM.l1 * ARM.l1 - ARM.l2 * ARM.l2) / (2 * ARM.l1 * ARM.l2);
  const a2 = Math.acos(Math.max(-1, Math.min(1, c2)));
  const a1 = Math.atan2(dy, dx) - Math.atan2(ARM.l2 * Math.sin(a2), ARM.l1 + ARM.l2 * Math.cos(a2));
  return { a1, a2 };
}

function armPoints(a) {
  const ex = ARM.bx + ARM.l1 * Math.cos(a.a1), ey = ARM.by + ARM.l1 * Math.sin(a.a1);
  const tx = ex + ARM.l2 * Math.cos(a.a1 + a.a2), ty = ey + ARM.l2 * Math.sin(a.a1 + a.a2);
  return { ex, ey, tx, ty };
}

function lastMove(v) {
  const calls = v.program ? programCalls(v) : (v.steps || []).map((a) => ({ capability: a.capability, args: a.arguments, status: a.status }));
  for (let i = calls.length - 1; i >= 0; i--) {
    const c = calls[i];
    if (c.capability === "workspace.pick" && c.status !== "failed") return { at: (c.args || {}).source_id, part: (c.args || {}).object_id };
    if (c.capability === "workspace.place" && c.status !== "failed") return { at: (c.args || {}).destination_id, part: (c.args || {}).object_id };
  }
  return null;
}

function drawArm(svg, v) {
  const mv = lastMove(v);
  const box = mv && layoutBoxes[mv.at];
  // Rest pose: folded above the base, inside the picture.
  const target = box ? { x: box.x + box.w / 2, y: box.y + box.h / 2 + (box.y < ARM.by ? 12 : -10) } : { x: ARM.bx + 150, y: ARM.by - 40 };
  const key = target.x + "," + target.y;
  if (!armCur) {
    armCur = ik(target.x, target.y); // first draw: no swing in from elsewhere
    armTargetKey = key;
  } else if (key !== armTargetKey) {
    armTargetKey = key;
    armAnim = { from: Object.assign({}, armCur), to: ik(target.x, target.y), t0: performance.now() };
  }
  for (const id of Object.keys(layoutBoxes)) {
    const el = svg.querySelector('[data-slot="' + id + '"]');
    if (el && mv && mv.at === id) el.classList.add("active");
  }
  const g = s("g", { id: "arm" });
  g.append(s("circle", { cx: ARM.bx, cy: ARM.by, r: 22, class: "arm-base" }));
  g.append(s("line", { id: "arm-l1", class: "arm-link" }), s("line", { id: "arm-l2", class: "arm-link" }), s("circle", { id: "arm-elbow", r: 9, class: "arm-joint" }));
  const tip = s("g", { id: "arm-tip" });
  tip.append(s("path", { d: "M -12 -14 L -12 6 M 12 -14 L 12 6 M -12 -14 L 12 -14", class: "arm-claw" }));
  const held = (v.workspace_truth.locations.find((l) => l.loc_type === "gripper") || { objects: [] }).objects[0];
  if (held) {
    const shape = partShape(held.observed_kind === "unknown" ? "unknown" : held.kind, -9, -8, 9);
    shape.append(s("title", {}, "in the arm: " + held.object_id));
    tip.append(shape);
  }
  g.append(tip);
  svg.append(g);
  placeArm();
}

function placeArm() {
  if (!armCur) return;
  const p = armPoints(armCur);
  const set = (id, attrs) => { const el = document.getElementById(id); if (el) for (const [k, val] of Object.entries(attrs)) el.setAttribute(k, val); };
  set("arm-l1", { x1: ARM.bx, y1: ARM.by, x2: p.ex, y2: p.ey });
  set("arm-l2", { x1: p.ex, y1: p.ey, x2: p.tx, y2: p.ty });
  set("arm-elbow", { cx: p.ex, cy: p.ey });
  set("arm-tip", { transform: "translate(" + p.tx.toFixed(1) + "," + p.ty.toFixed(1) + ")" });
}

function armLoop(now) {
  if (armAnim) {
    const t = Math.min(1, (now - armAnim.t0) / 550);
    const e = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
    armCur = { a1: armAnim.from.a1 + (armAnim.to.a1 - armAnim.from.a1) * e, a2: armAnim.from.a2 + (armAnim.to.a2 - armAnim.from.a2) * e };
    placeArm();
    if (t >= 1) armAnim = null;
  }
  requestAnimationFrame(armLoop);
}
requestAnimationFrame(armLoop);

// ------------------------------------------------------------------ the robot cell in 3D

let cell3d = false;
let cellFor = null; // what the 3D view shows: "run:<id>" or "scenario:<id>"
let cellBusy = false;
let lastRehearsal = null;

function mountCell() {
  if (cell3d || !window.RevolveCell) return;
  cell3d = window.RevolveCell.mount($("cell3d"));
  $("cell-card").hidden = !cell3d;
  $("workspace-card").hidden = cell3d;
}
window.addEventListener("revolve-cell-ready", () => { mountCell(); pollCell(); });
setTimeout(() => { if (!cell3d) { $("workspace-card").hidden = false; } }, 4000);

async function pollCell() {
  if (!cell3d || cellBusy) return;
  const want = current ? "run:" + current : "scenario:" + $("scenario-select").value;
  if (want !== cellFor) {
    window.RevolveCell.reset();
    cellFor = want;
  } else if (!current) {
    return; // a scenario's table does not change until a run starts
  }
  cellBusy = true;
  try {
    const path = current
      ? "/api/runs/" + encodeURIComponent(current) + "/scene?since=" + window.RevolveCell.lastSeq()
      : "/api/scenarios/" + encodeURIComponent($("scenario-select").value) + "/scene";
    const sc = await api("GET", path);
    if (want === cellFor) {
      window.RevolveCell.update(sc);
      renderSpeed(sc.speed);
    }
  } catch { /* the next poll retries */ }
  finally { cellBusy = false; }
}

const SPEEDS = [1, 1.5, 3, 6];
function renderSpeed(speed) {
  const box = $("speed-buttons");
  const live = !!current && view && ["running", "paused", "awaiting_human", "blocked", "created"].includes(view.state.status);
  box.hidden = !live;
  if (!live) return;
  if (box.getAttribute("data-speed") === String(speed)) return;
  box.setAttribute("data-speed", String(speed));
  clear(box);
  box.append(h("span", {}, "Speed:"));
  for (const s of SPEEDS) {
    box.append(h("button", { type: "button", class: "ghost" + (Math.abs(s - speed) < 0.01 ? " on" : ""), onclick: () => act("sim", { control: "speed", value: s }, true) }, s + "×"));
  }
  box.append(h("span", { class: "small" }, "real time"));
}

function previewRehearsal(r) {
  lastRehearsal = r && r.motions && r.motions.length ? r.motions : null;
  if (cell3d && lastRehearsal) window.RevolveCell.preview(lastRehearsal, 4);
}

// ------------------------------------------------------------------ technical view

const CHIP_LABEL = { dispatch_recorded: "in flight", acknowledged: "unverified", outcome_unknown: "outcome unknown", retry_pending: "retry (same key)", succeeded: "verified", satisfied: "already true" };

function renderTech(v) {
  $("run-meta").textContent = v.meta.run_id + " · scenario " + v.meta.scenario_id + " · arm " + v.meta.arm + " · planner " + v.meta.planner + " · human " + v.meta.human_mode + " · planner calls " + v.planner_calls.requests + " · status " + v.state.status;
  $("status-reason").textContent = v.state.status_reason || "";
  $("executor-line").textContent = v.executor.active ? "Executor process active (pid " + (v.state.owner_pid || "?") + ")." : "No executor process running (last exit code " + v.executor.last_exit_code + ").";
  const gh = $("goal-history");
  clear(gh);
  for (const r of v.goal_history.slice().reverse()) gh.append(h("li", {}, "r" + r.revision + " by " + r.author + ": " + goalSentence(r.spec) + (r.reason ? " (" + r.reason + ")" : "")));
  const el = $("report");
  clear(el);
  el.append(h("pre", { class: "small" }, JSON.stringify(v.state.final_report || "no report yet", null, 1).slice(0, 4000)));
  const t = $("inventory");
  clear(t);
  t.append(h("tr", {}, h("th", {}, "kind"), h("th", {}, "on hand"), h("th", {}, "reserved")));
  for (const k of Object.keys(v.inventory.on_hand)) t.append(h("tr", {}, h("td", {}, k), h("td", {}, v.inventory.on_hand[k]), h("td", {}, v.inventory.reserved[k] || 0)));
  const l = $("ledger");
  clear(l);
  for (const e of v.ledger) l.append(h("li", {}, e.entry_type + " " + e.kind + " Δon_hand " + e.delta_on_hand + " Δreserved " + e.delta_reserved + (e.evidence ? " · evidence " + e.evidence : "")));
  const plans = $("plans");
  clear(plans);
  const active = v.plans.find((p) => p.plan_seq === v.active_plan_seq);
  $("plan-label").textContent = active ? "#" + active.plan_seq + " · " + active.status + " · r" + active.goal_revision : "none";
  $("plan-explanation").textContent = active && active.explanation ? active.explanation : "";
  for (const p of v.plans.slice().reverse()) {
    const li = h("li", {}, h("strong", {}, "#" + p.plan_seq + " " + p.status), " · goal r" + p.goal_revision + " · " + p.planner + (p.steps != null ? " · " + p.steps + " steps" : ""));
    if (p.status === "rejected" && Array.isArray(p.diagnostics)) for (const d of p.diagnostics.slice(0, 3)) li.append(h("div", { class: "muted" }, "[" + d.category + "] " + (d.step_id ? d.step_id + ": " : "") + d.message));
    plans.append(li);
  }
  const steps = $("steps");
  clear(steps);
  for (const a of v.steps) {
    const args = a.arguments || {};
    const key = args.object_id ? args.object_id + " → " + (args.destination_id || args.source_id || "") : args.part_kind ? args.part_kind + " (" + (args.evidence_object_id || args.observed_count || "") + ")" : "";
    steps.append(h("li", {}, h("span", { class: "chip " + a.status }, CHIP_LABEL[a.status] || a.status),
      h("span", { class: "what" }, (a.origin !== "plan" ? "[" + a.origin + "] " : "") + a.capability + " " + key,
        a.op_key ? h("span", { class: "muted" }, " · " + a.op_key.slice(0, 11) + (a.attempts > 1 ? " ×" + a.attempts : "")) : null,
        a.error && a.status !== "succeeded" ? h("div", { class: "muted" }, a.error) : null)));
  }
  const ev = $("events");
  clear(ev);
  const base = v.events.length ? v.events[v.events.length - 1].at_ms : 0;
  for (const e of v.events) ev.append(h("li", {}, h("span", { class: "t" }, "+" + ((e.at_ms - base) / 1000).toFixed(1) + "s"), h("span", { class: "kind" }, e.kind), e.message));
  const tt = $("tray-toggles");
  clear(tt);
  for (const loc of v.workspace_truth.locations.filter((x) => x.loc_type === "tray")) {
    tt.append(h("button", { type: "button", class: "ghost", onclick: () => act("sim", { control: loc.available ? "tray_unavailable" : "tray_available", location_id: loc.location_id }) },
      loc.location_id + (loc.available ? ": make unavailable" : ": make available")));
  }
}

// ------------------------------------------------------------------ main render / poll

// The page shows one thing at a time: the request form (no run selected), or a
// run with a single "New request" button in place of the form.
function showMode() {
  const run = !!current;
  $("start-full").hidden = run;
  $("start-collapsed").hidden = !run;
  $("now-card").hidden = !run;
  if (!run) $("run-panels").hidden = true;
}

// Exactly one start button on screen: the form's own button is hidden while the
// "I understood ..." confirmation (which has its own) is shown.
function isProgramStyle() { return $("style-select").value === "program"; }

function syncStartButton() {
  const b = $("start-btn");
  const program = isProgramStyle();
  b.hidden = !program && !$("interpretation").hidden;
  b.textContent = writingNow ? "Writing…" : program ? "Write the program" : $("new-request").value.trim() ? "Read my request" : "Start";
  b.disabled = writingNow;
  $("examples").hidden = !program;
  $("start-note").textContent = program
    ? "The AI writes a program for your words (a few cents). Before you decide to run it, it is checked, compiled to machine code and rehearsed on a copy of the table."
    : "Leave the box empty for the situation's usual request. Your own words are read by the AI (about a cent); you confirm what it understood before anything moves.";
}
function showInterpretation(on) {
  $("interpretation").hidden = !on;
  syncStartButton();
}

function changeWords() {
  showInterpretation(false);
  $("ask").focus();
}

function newRequest() {
  hudNotice = null;
  $("cell-hud").textContent = "";
  hudIdle();
  current = null;
  view = null;
  $("run-select").value = "";
  history.replaceState(null, "", "#");
  showInterpretation(false);
  showMode();
  $("ask").focus();
}

function renderEmpty() {
  showMode();
}

function render(v) {
  if (v.meta.run_id !== current) return; // a reply for a run no longer shown
  view = v;
  showMode();
  $("run-panels").hidden = false;
  const trayKit = trayKitMap(v);
  const isProgram = !!v.program;
  $("kit-panels").hidden = isProgram;
  $("run-program-card").hidden = !isProgram;
  if (isProgram) {
    renderNowProgram(v);
    renderRunningProgram(v);
  } else {
    $("change-box").hidden = true;
    $("revision-card").hidden = true;
    renderNow(v, trayKit);
    renderGoal(v);
    renderKits(v, trayKit);
  }
  drawWorkspace(v, trayKit);
  if (isProgram) renderHud(v);
  else if (!writingNow && !hudNotice) hud($("now-title").textContent, $("now-detail").textContent);
  if (isProgram) renderStoryLines(programStory(v));
  else renderStory(v, trayKit);
  if (!$("tech").hidden) renderTech(v);
}

async function poll() {
  if (!current) {
    renderEmpty();
    return;
  }
  if (busy) return;
  busy = true;
  try { render(await api("GET", "/api/runs/" + encodeURIComponent(current))); }
  catch (e) { $("now-detail").textContent = "Could not load this run: " + e.message; }
  finally { busy = false; }
}

async function loadRuns(select) {
  const runs = await api("GET", "/api/runs");
  const sel = $("run-select");
  clear(sel);
  sel.append(h("option", { value: "" }, runs.length ? "—" : "none yet"));
  const label = (id) => { const f = FRIENDLY.find((x) => x[0] === id); return f ? f[1] : id; };
  for (const r of runs) {
    const when = new Date(r.created_at_ms).toLocaleTimeString();
    sel.append(h("option", { value: r.run_id }, when + " · " + label(r.scenario) + " · " + r.status.replace("_", " ")));
  }
  if (select) current = select;
  sel.value = current || "";
}

async function startRun(goal) {
  try {
    const r = await api("POST", "/api/runs", {
      scenario: $("scenario-select").value, arm: $("arm-select").value, planner: $("planner-select").value, budget: 5,
      scripted_human: $("scripted-human").checked, goal,
    });
    showInterpretation(false);
    $("new-request").value = "";
    syncStartButton();
    await loadRuns(r.run_id);
    showMode();
    history.replaceState(null, "", "#run=" + encodeURIComponent(r.run_id));
    await act("start", {}, true);
    toast("Started.");
    $("now-card").scrollIntoView({ behavior: "smooth", block: "start" });
  } catch (err) { toast("Could not start: " + err.message); }
}

function wire() {
  $("run-select").addEventListener("change", (e) => {
    if (!e.target.value) return newRequest();
    current = e.target.value;
    history.replaceState(null, "", "#run=" + encodeURIComponent(current));
    showMode();
    poll();
  });
  $("btn-new").addEventListener("click", newRequest);
  $("btn-refresh").addEventListener("click", () => loadRuns());
  $("btn-reload").addEventListener("click", () => location.reload());
  $("tech-toggle").addEventListener("change", (e) => { $("tech").hidden = !e.target.checked; if (view) render(view); });
  $("scenario-select").addEventListener("change", () => { describeScenario(); if (!current) pollCell(); });
  $("show-all-scenarios").addEventListener("change", fillScenarioSelect);
  $("new-run").addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = $("new-request").value.trim();
    if (isProgramStyle()) {
      if (!text) { toast("Say what you want the robot to do, or pick an example."); $("ask").focus(); return; }
      return writeProgram(text);
    }
    if (!text) return startRun(null);
    // Your own words: show what was understood first; nothing runs until you confirm.
    showInterpretation(true);
    $("interp-title").textContent = "Reading your request…";
    $("interp-detail").textContent = "";
    clear($("interp-actions"));
    try {
      const r = await api("POST", "/api/interpret", { text });
      const cost = r.cost_usd_estimate != null ? " · cost about $" + Number(r.cost_usd_estimate).toFixed(3) : "";
      // Never offer to start a request with a kit that has no parts.
      const usable = r.understood && r.goal && kitSpecs(r.goal).length > 0 && kitSpecs(r.goal).every((k) => sizeOf(k.components) > 0);
      if (usable) {
        $("interp-title").textContent = "I understood: " + goalSentence(r.goal) + ".";
        $("interp-detail").textContent = (r.assumptions ? "Assumed: " + r.assumptions + " " : "") + (r.not_supported ? "Not possible here: " + r.not_supported + " " : "") + "(read by " + r.by + cost + ")";
        $("interp-actions").append(
          actionButton("Yes, start", () => startRun(r.goal), "big"),
          actionButton("Change my words", changeWords, "ghost"));
      } else {
        $("interp-title").textContent = "I couldn't turn that into a job for this workspace.";
        $("interp-detail").textContent = (r.not_supported || "It only knows red, blue and green blocks, washers and trays.") + " (read by " + r.by + cost + ")";
        $("interp-actions").append(actionButton("Change my words", changeWords, "ghost"));
      }
    } catch (err) {
      $("interp-title").textContent = "I couldn't read that.";
      $("interp-detail").textContent = err.message;
      $("interp-actions").append(actionButton("Try again", changeWords, "ghost"));
    }
  });
  $("new-request").addEventListener("input", () => showInterpretation(false));
  $("new-request").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); $("new-run").requestSubmit(); }
  });
  $("style-select").addEventListener("change", () => { showInterpretation(false); syncStartButton(); });
  $("change-write").addEventListener("click", writeRevision);
  $("do-it").addEventListener("click", doIt);
  $("ask").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); doIt(); }
  });
  $("adv-toggle").addEventListener("click", () => setAdvanced(document.body.classList.contains("simple-ui")));
  $("btn-new-table").addEventListener("click", () => { newRequest(); hudIdle(); pollCell(); });
  $("correct-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const comps = {};
    for (const [id, k] of [["c-red", "red_block"], ["c-blue", "blue_block"], ["c-green", "green_block"], ["c-washer", "washer"]]) {
      const n = parseInt($(id).value, 10) || 0;
      if (n > 0) comps[k] = n;
    }
    act("correct", { goal: { kit_quantity: parseInt($("c-qty").value, 10) || 1, kit_components: comps } });
  });
  for (const b of document.querySelectorAll("[data-sim]")) {
    b.addEventListener("click", () => {
      const what = b.getAttribute("data-sim");
      const counts = (view && view.workspace_truth.effect_counts) || {};
      if (what === "crash_next_place") act("sim", { control: "arm_fault", fault: { kind: "crash_after_effect", on: { capability: "workspace.place", nth: (counts["workspace.place"] || 0) + 1 } } });
      else if (what === "timeout_blackout_next_pick") act("sim", { control: "arm_fault", fault: { kind: "timeout_after_effect", blackout: true, on: { capability: "workspace.pick", nth: (counts["workspace.pick"] || 0) + 1 } } });
      else act("sim", { control: what });
    });
  }
}

(async function main() {
  wire();
  let adv = false;
  try { adv = localStorage.getItem("revolve-advanced") === "1"; } catch { /* default: simple */ }
  setAdvanced(adv);
  hudIdle();
  mountCell();
  setInterval(pollCell, 250);
  const m = /run=([A-Za-z0-9_-]+)/.exec(location.hash);
  try { await loadScenarios(); await loadExamples(); await loadRuns(m ? m[1] : null); } catch (e) { toast("Server unavailable: " + e.message); }
  syncStartButton();
  // Demo links: #example=<id> opens that example's checked and rehearsed program.
  const ex = /example=([a-z0-9-]+)/.exec(location.hash);
  if (ex && !m) { const e = examples.find((x) => x.id === ex[1]); if (e) checkExample(e); }
  poll();
  setInterval(poll, 700);
})();
