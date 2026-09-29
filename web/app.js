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
  let j;
  try { j = await r.json(); } catch { j = { error: "HTTP " + r.status }; }
  if (!r.ok) throw new Error(j.error || ("HTTP " + r.status));
  return j;
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
function kitSize(goal) { return Object.values(goal.kit_components || {}).reduce((a, b) => a + b, 0); }

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
  sel.value = keep && [...sel.options].some((o) => o.value === keep) ? keep : "nominal";
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
  const size = kitSize(v.goal);
  const q = v.goal.kit_quantity;
  let placed = 0;
  for (const l of v.workspace_truth.locations.filter((x) => x.loc_type === "tray" && trayKit[x.location_id])) placed += Math.min(size, l.objects.length);
  return { placed, total: size * q };
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
    actions.append(actionButton("Start another run", () => { window.scrollTo({ top: 0, behavior: "smooth" }); }, "ghost"));
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
  return g.kit_quantity + " " + (g.kit_quantity === 1 ? "kit" : "kits") + ", each with " + countsText(g.kit_components);
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
}

function renderKits(v, trayKit) {
  const ul = $("kits");
  clear(ul);
  const size = kitSize(v.goal);
  const report = v.state.final_report;
  const verified = new Set((report && report.claims_source !== "agent" && report.kits_verified_complete) || []);
  const byKit = {};
  for (const [tray, kit] of Object.entries(trayKit)) byKit[kit] = tray;
  for (let k = 1; k <= v.goal.kit_quantity; k++) {
    const tray = byKit[k];
    const loc = tray && v.workspace_truth.locations.find((l) => l.location_id === tray);
    const n = loc ? loc.objects.length : 0;
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
    ul.append(h("li", {}, h("span", {}, "Kit " + k + (tray ? " · tray " + trayNum(tray) : "")), h("span", { class: "state " + cls }, state)));
  }
  const reserved = v.inventory && Object.values(v.inventory.reserved || {}).some((n) => n > 0);
  $("stock-line").textContent = stockSentence(v) + (reserved ? " Set aside for this job: " + countsText(v.inventory.reserved) + "." : "");
}

function storyLines(v, trayKit) {
  const events = v.events.slice().reverse();
  const base = events.length ? events[0].at_ms : 0;
  const size = kitSize(v.goal);
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
            if (perTray[dst] === size) add("All parts are in " + placeName(dst, trayKit) + ".", "", e);
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
  const ol = $("story");
  clear(ol);
  for (const l of storyLines(v, trayKit).slice(-40)) ol.append(h("li", { class: l.cls }, l.text, h("span", { class: "t" }, l.t)));
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
  const grip = t.locations.find((l) => l.loc_type === "gripper");
  const report = v.state.final_report;
  const verified = new Set((report && report.claims_source !== "agent" && report.kits_verified_complete) || []);
  const box = (l, x, y, w, hgt, label) => {
    const g = s("g", {});
    g.append(s("rect", { x, y, width: w, height: hgt, rx: 8, class: "slot" + (l.available ? "" : " off") + (verified.has(l.location_id) ? " done" : "") }));
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
    box(l, 10 + i * (bw + 10), 10, bw, 110, label);
  });
  if (grip) box(grip, 380, 140, 140, 70, "Robot hand");
  const tw = (880 - (trays.length - 1) * 10) / Math.max(1, trays.length);
  trays.forEach((l, i) => box(l, 10 + i * (tw + 10), 230, tw, 140,
    trayKit[l.location_id] ? "Kit " + trayKit[l.location_id] + " · tray " + trayNum(l.location_id) : "Tray " + trayNum(l.location_id) + (l.objects.length ? "" : " (spare)")));
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

function renderEmpty() {
  setBadge("idle", "Nothing running");
  $("now-title").textContent = "Choose what should happen above and press Start.";
  $("now-detail").textContent = "Or pick one of your previous runs at the top of the page.";
  $("now-progress").textContent = "";
  $("progress-bar").hidden = true;
  $("question-box").hidden = true;
  clear($("now-actions"));
}

function render(v) {
  view = v;
  const trayKit = trayKitMap(v);
  renderNow(v, trayKit);
  renderGoal(v);
  renderKits(v, trayKit);
  drawWorkspace(v, trayKit);
  renderStory(v, trayKit);
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

function wire() {
  $("run-select").addEventListener("change", (e) => {
    current = e.target.value || null;
    history.replaceState(null, "", current ? "#run=" + encodeURIComponent(current) : "#");
    poll();
  });
  $("btn-refresh").addEventListener("click", () => loadRuns());
  $("tech-toggle").addEventListener("change", (e) => { $("tech").hidden = !e.target.checked; if (view) render(view); });
  $("scenario-select").addEventListener("change", describeScenario);
  $("show-all-scenarios").addEventListener("change", fillScenarioSelect);
  $("new-run").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      const r = await api("POST", "/api/runs", {
        scenario: $("scenario-select").value, arm: $("arm-select").value,
        scripted_human: $("scripted-human").checked, request: $("new-request").value,
      });
      await loadRuns(r.run_id);
      history.replaceState(null, "", "#run=" + encodeURIComponent(r.run_id));
      await act("start", {}, true);
      toast("Started.");
      $("now-card").scrollIntoView({ behavior: "smooth", block: "start" });
    } catch (err) { toast("Could not start: " + err.message); }
  });
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
  const m = /run=([A-Za-z0-9_-]+)/.exec(location.hash);
  try { await loadScenarios(); await loadRuns(m ? m[1] : null); } catch (e) { toast("Server unavailable: " + e.message); }
  poll();
  setInterval(poll, 700);
})();
