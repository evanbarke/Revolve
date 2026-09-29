"use strict";
// Revolve UI. Every value shown comes from persisted state returned by the
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
  toastTimer = setTimeout(() => { t.hidden = true; }, 4000);
}

async function act(action, body) {
  if (!current) return;
  try {
    const r = await api("POST", "/api/runs/" + encodeURIComponent(current) + "/" + action, body);
    toast(r.message || "ok");
    await poll();
  } catch (e) { toast("Refused: " + e.message); }
}

// ------------------------------------------------------------------ status semantics

const TERMINAL = new Set(["completed", "partial", "stopped", "failed"]);

function statusInfo(st) {
  const reason = st.status_reason || "";
  switch (st.status) {
    case "completed": return ["ok", "Completed — verified by observation"];
    case "partial": return ["warn", "Partially completed — verified report"];
    case "awaiting_human": return ["info", "Awaiting your input"];
    case "awaiting_planner": return ["violet", "Waiting for the planner connection"];
    case "blocked": return ["bad", reason.includes("outcome unknown") ? "Blocked — outcome unknown" : "Blocked"];
    case "stopped": return ["idle", "Stopped by human"];
    case "failed": return ["bad", "Failed"];
    case "running": return ["info", "Running"];
    case "paused": return ["idle", "Paused"];
    case "created": return ["idle", "Not started"];
    default: return ["idle", st.status];
  }
}

const CHIP_LABEL = {
  dispatch_recorded: "in flight", acknowledged: "unverified", outcome_unknown: "outcome unknown",
  retry_pending: "retry (same key)", succeeded: "verified", satisfied: "already true",
};

// ------------------------------------------------------------------ workspace drawing

function partShape(kind, x, y, r) {
  if (kind === "washer") return s("circle", { cx: x + r, cy: y + r, r: r - 3, class: "washer" });
  if (kind === "red_block" || kind === "blue_block" || kind === "green_block") {
    return s("rect", { x, y, width: 2 * r, height: 2 * r, rx: 3, class: kind.split("_")[0] });
  }
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
    shape.append(s("title", {}, o.object_id + (o.observed_kind === "unknown" ? " (unclassified; truly " + o.kind + ")" : "")));
    g.append(shape);
  });
}

function drawWorkspace(v) {
  const svg = $("workspace");
  clear(svg);
  const defs = s("defs", {},
    s("pattern", { id: "hatch", width: 8, height: 8, patternUnits: "userSpaceOnUse", patternTransform: "rotate(45)" },
      s("rect", { width: 8, height: 8, fill: "rgba(0,0,0,0.25)" }),
      s("line", { x1: 0, y1: 0, x2: 0, y2: 8, stroke: "rgba(0,0,0,0.5)", "stroke-width": 3 })));
  svg.append(defs);
  const t = v.workspace_truth;
  const withheld = new Set(t.evidence_withheld_from_system || []);
  const locs = t.locations;
  const bins = locs.filter((l) => l.loc_type === "bin");
  const trays = locs.filter((l) => l.loc_type === "tray").sort((a, b) => parseInt(a.location_id.split("-")[1]) - parseInt(b.location_id.split("-")[1]));
  const grip = locs.find((l) => l.loc_type === "gripper");
  const verified = new Set((v.state.final_report && v.state.final_report.kits_verified_complete) || []);

  const box = (l, x, y, w, hgt, label) => {
    const g = s("g", {});
    g.append(s("rect", { x, y, width: w, height: hgt, rx: 8, class: "slot" + (l.available ? "" : " off") }));
    g.append(s("text", { x: x + 8, y: y + 16 }, label));
    drawObjects(g, l.objects, x, y, w, hgt);
    if (!l.available) g.append(s("text", { x: x + w - 90, y: y + 16, class: "off-label" }, "UNAVAILABLE"));
    if (verified.has(l.location_id)) g.append(s("text", { x: x + w - 64, y: y + hgt - 8, class: "check" }, "✓ verified"));
    if (withheld.has(l.location_id)) {
      g.append(s("rect", { x, y, width: w, height: hgt, rx: 8, class: "hidden-overlay" }));
      g.append(s("text", { x: x + 8, y: y + hgt - 8 }, "hidden from system"));
    }
    svg.append(g);
  };
  const bw = (880 - (bins.length - 1) * 10) / Math.max(1, bins.length);
  bins.forEach((l, i) => box(l, 10 + i * (bw + 10), 10, bw, 110, l.location_id + " (" + l.objects.length + ")"));
  if (grip) box(grip, 380, 140, 140, 70, "gripper");
  svg.append(s("path", { d: "M450 135 L450 128 M435 128 L465 128", class: "gripper" }));
  const tw = (880 - (trays.length - 1) * 10) / Math.max(1, trays.length);
  trays.forEach((l, i) => box(l, 10 + i * (tw + 10), 230, tw, 140, l.location_id));
  $("world-rev").textContent = "world revision " + t.world_revision + (t.removed.length ? " · removed by operator: " + t.removed.join(", ") : "") + (t.planner_offline ? " · planner offline" : "");
}

// ------------------------------------------------------------------ panels

function fmtCounts(obj) {
  const e = Object.entries(obj || {}).filter(([, n]) => n > 0);
  return e.length ? e.map(([k, n]) => n + " " + k.replace("_", " ")).join(", ") : "nothing";
}

function fmtTime(ms, base) {
  const d = (ms - base) / 1000;
  return "+" + d.toFixed(1) + "s";
}

function renderReport(v) {
  const el = $("report");
  clear(el);
  const r = v.state.final_report;
  if (!r) {
    el.append(h("p", { class: "muted" }, "No verified result yet. A kit is only reported complete after the verifier observes it."));
    return;
  }
  if (r.claims_source === "agent") {
    el.append(h("p", {}, h("strong", {}, "Agent's own claim (baseline; not verified by the runtime): "),
      (r.kits_claimed_complete || []).join(", ") || "no kits"));
    el.append(h("p", { class: "small" }, r.message || ""));
    return;
  }
  el.append(h("p", {}, h("strong", {}, (r.kits_verified_complete || []).length + " of " + r.kits_requested + " kit(s) verified complete"),
    " (goal r" + r.goal_revision + ": " + r.goal + ")"));
  const missing = fmtCounts(r.missing_to_finish);
  el.append(h("p", {}, "Missing to finish: " + (missing === "nothing" ? "nothing" : missing)));
  if ((r.inventory_discrepancies || []).length) el.append(h("p", { class: "small" }, "Inventory discrepancies: " + r.inventory_discrepancies.join("; ")));
  for (const n of r.notes || []) el.append(h("p", { class: "small muted" }, "Note: " + n));
  const grid = h("div", { class: "report-grid" });
  for (const t of r.trays || []) {
    if (!t.objects.length && !t.kit) continue;
    grid.append(h("div", { class: "tray-card" + (t.matches_composition ? " ok" : "") },
      h("strong", {}, t.tray_id + (t.kit ? " · kit " + t.kit : "")), h("br"),
      h("span", { class: "small" }, fmtCounts(t.contents)), h("br"),
      h("span", { class: "small muted" }, t.matches_composition ? "matches composition" : "missing: " + fmtCounts(t.missing) + (Object.keys(t.excess || {}).length ? "; extra: " + fmtCounts(t.excess) : ""))));
  }
  el.append(grid);
  el.append(h("p", { class: "small muted" }, "Evidence: " + r.evidence + " at world revision " + r.verified_world_revision));
}

function renderInventory(v) {
  const t = $("inventory");
  clear(t);
  t.append(h("tr", {}, h("th", {}, "part"), h("th", {}, "record on hand"), h("th", {}, "reserved")));
  for (const k of Object.keys(v.inventory.on_hand)) {
    t.append(h("tr", {}, h("td", {}, k.replace("_", " ")), h("td", {}, v.inventory.on_hand[k]), h("td", {}, v.inventory.reserved[k] || 0)));
  }
  const l = $("ledger");
  clear(l);
  for (const e of v.ledger) {
    l.append(h("li", {}, e.entry_type + " " + e.kind + " Δon_hand " + e.delta_on_hand + " Δreserved " + e.delta_reserved + (e.evidence ? " · evidence " + e.evidence : "")));
  }
}

function renderPlan(v) {
  const plans = $("plans");
  clear(plans);
  const active = v.plans.find((p) => p.plan_seq === v.active_plan_seq);
  $("plan-label").textContent = active ? "#" + active.plan_seq + " · " + active.status + " · r" + active.goal_revision : "none";
  $("plan-explanation").textContent = active && active.explanation ? active.explanation : "";
  for (const p of v.plans.slice().reverse()) {
    const li = h("li", {}, h("strong", {}, "#" + p.plan_seq + " " + p.status), " · goal r" + p.goal_revision + " · " + p.planner + (p.steps !== null && p.steps !== undefined ? " · " + p.steps + " steps" : ""));
    if (p.status === "rejected" && Array.isArray(p.diagnostics)) {
      for (const d of p.diagnostics.slice(0, 3)) li.append(h("div", { class: "muted" }, "[" + d.category + "] " + (d.step_id ? d.step_id + ": " : "") + d.message));
    }
    plans.append(li);
  }
  const steps = $("steps");
  clear(steps);
  for (const a of v.steps) {
    const args = a.arguments || {};
    const key = args.object_id ? args.object_id + " → " + (args.destination_id || args.source_id || "") : (args.part_kind ? args.part_kind + " (" + (args.evidence_object_id || args.observed_count || "") + ")" : "");
    steps.append(h("li", {},
      h("span", { class: "chip " + a.status }, CHIP_LABEL[a.status] || a.status),
      h("span", { class: "what" }, (a.origin !== "plan" ? "[" + a.origin + "] " : "") + a.capability + " " + key,
        a.op_key ? h("span", { class: "muted" }, " · " + a.op_key.slice(0, 11) + (a.attempts > 1 ? " ×" + a.attempts : "")) : null,
        a.error && a.status !== "succeeded" ? h("div", { class: "muted" }, a.error) : null)));
  }
}

function scrollToCurrentStep() {
  const list = $("steps");
  const cur = list.querySelector(".chip.dispatch_recorded, .chip.acknowledged, .chip.outcome_unknown, .chip.retry_pending, .chip.pending");
  if (cur) list.scrollTop = Math.max(0, cur.parentElement.offsetTop - list.offsetTop - 60);
}

function renderEvents(v) {
  const el = $("events");
  clear(el);
  const base = v.events.length ? v.events[v.events.length - 1].at_ms : 0;
  for (const e of v.events) {
    el.append(h("li", {}, h("span", { class: "t" }, fmtTime(e.at_ms, base)), h("span", { class: "kind" }, e.kind), e.message));
  }
}

function renderGoal(v) {
  $("goal-rev").textContent = "revision " + v.goal_revision;
  const g = v.goal;
  $("goal-summary").textContent = g.kit_quantity + " kit(s), each: " + fmtCounts(g.kit_components) +
    (g.approved_substitutions.length ? " · approved substitutions: " + g.approved_substitutions.map((x) => x.with + " for " + x.replace).join(", ") : "") +
    (g.rework_permitted === true ? " · rework permitted" : g.rework_permitted === false ? " · rework declined" : "");
  $("goal-text").textContent = g.source_text ? "“" + g.source_text + "”" : "";
  const hist = $("goal-history");
  clear(hist);
  for (const r of v.goal_history.slice().reverse()) {
    hist.append(h("li", {}, "r" + r.revision + " by " + r.author + ": " + r.spec.kit_quantity + " × [" + fmtCounts(r.spec.kit_components) + "]" + (r.reason ? " — " + r.reason : "")));
  }
  if (!$("correct-form").contains(document.activeElement)) {
    $("c-qty").value = g.kit_quantity;
    $("c-red").value = g.kit_components.red_block || 0;
    $("c-blue").value = g.kit_components.blue_block || 0;
    $("c-green").value = g.kit_components.green_block || 0;
    $("c-washer").value = g.kit_components.washer || 0;
  }
}

function renderQuestion(v) {
  const q = v.questions.find((x) => x.status === "open" && x.goal_revision === v.goal_revision);
  $("question-card").hidden = !q;
  if (!q) return;
  $("question-prompt").textContent = q.prompt;
  const opts = $("question-options");
  clear(opts);
  for (const o of q.options) {
    opts.append(h("button", { type: "button", onclick: () => act("answer", { question_id: q.question_id, choice: o.id }) }, o.label));
  }
}

function renderControls(v) {
  const terminal = TERMINAL.has(v.state.status);
  const active = v.executor.active;
  $("btn-start").disabled = terminal || active;
  $("btn-pause").disabled = terminal || !active;
  $("btn-cancel").disabled = terminal;
  const line = $("executor-line");
  if (active) line.textContent = "Executor process active (pid " + (v.state.owner_pid || "?") + ").";
  else if (v.executor.last_exit_code === v.executor.simulated_crash_code) line.textContent = "The executor process died (simulated crash, exit " + v.executor.last_exit_code + "). Press Start / Resume: a new process will reconcile before doing anything else.";
  else if (terminal) line.textContent = "Run finished.";
  else line.textContent = "No executor running.";
  const tt = $("tray-toggles");
  clear(tt);
  for (const l of v.workspace_truth.locations.filter((x) => x.loc_type === "tray")) {
    tt.append(h("button", { type: "button", class: "ghost", onclick: () => act("sim", { control: l.available ? "tray_unavailable" : "tray_available", location_id: l.location_id }) },
      l.location_id + (l.available ? ": make unavailable" : ": make available")));
  }
}

function render(v) {
  view = v;
  const [cls, label] = statusInfo(v.state);
  const b = $("status-badge");
  b.className = "badge " + cls;
  b.textContent = label;
  $("status-reason").textContent = v.state.status_reason || "";
  $("run-meta").textContent = v.meta.run_id + " · scenario " + v.meta.scenario_id + " · " + (v.meta.arm === "revolve" ? "Revolve" : "direct tool-calling baseline") +
    " · planner " + v.meta.planner + " · human " + v.meta.human_mode + " · planner calls " + v.planner_calls.requests;
  drawWorkspace(v);
  renderReport(v);
  renderInventory(v);
  renderPlan(v);
  scrollToCurrentStep();
  renderEvents(v);
  renderGoal(v);
  renderQuestion(v);
  renderControls(v);
}

async function poll() {
  if (!current || busy) return;
  busy = true;
  try { render(await api("GET", "/api/runs/" + encodeURIComponent(current))); }
  catch (e) { $("status-reason").textContent = "Could not load run: " + e.message; }
  finally { busy = false; }
}

async function loadRuns(select) {
  const runs = await api("GET", "/api/runs");
  const sel = $("run-select");
  clear(sel);
  sel.append(h("option", { value: "" }, runs.length ? "— choose a run —" : "no runs yet"));
  for (const r of runs) sel.append(h("option", { value: r.run_id }, r.scenario + " · " + r.arm + " · " + r.status + " · " + r.run_id.slice(-6)));
  if (select) current = select;
  sel.value = current || "";
}

async function loadScenarios() {
  scenarios = await api("GET", "/api/scenarios");
  const sel = $("scenario-select");
  clear(sel);
  for (const set of ["demo", "development", "held-out"]) {
    const og = h("optgroup", { label: set });
    for (const sc of scenarios.filter((x) => x.set === set)) og.append(h("option", { value: sc.id }, sc.id));
    sel.append(og);
  }
  sel.value = "nominal";
  describeScenario();
}

function describeScenario() {
  const sc = scenarios.find((x) => x.id === $("scenario-select").value);
  $("scenario-desc").textContent = sc ? "[" + sc.family + "] " + sc.description : "";
}

function wire() {
  $("run-select").addEventListener("change", (e) => {
    current = e.target.value || null;
    history.replaceState(null, "", current ? "#run=" + encodeURIComponent(current) : "#");
    poll();
  });
  $("btn-refresh").addEventListener("click", () => loadRuns());
  $("scenario-select").addEventListener("change", describeScenario);
  $("new-run").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      const r = await api("POST", "/api/runs", {
        scenario: $("scenario-select").value, arm: $("arm-select").value,
        scripted_human: $("scripted-human").checked, request: $("new-request").value,
      });
      await loadRuns(r.run_id);
      history.replaceState(null, "", "#run=" + encodeURIComponent(r.run_id));
      toast("Run created. Press Start / Resume to execute.");
      await poll();
    } catch (err) { toast("Refused: " + err.message); }
  });
  $("btn-start").addEventListener("click", () => act("start"));
  $("btn-pause").addEventListener("click", () => act("pause"));
  $("btn-cancel").addEventListener("click", () => act("cancel"));
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
      if (what === "crash_next_place") {
        act("sim", { control: "arm_fault", fault: { kind: "crash_after_effect", on: { capability: "workspace.place", nth: (counts["workspace.place"] || 0) + 1 } } });
      } else if (what === "timeout_blackout_next_pick") {
        act("sim", { control: "arm_fault", fault: { kind: "timeout_after_effect", blackout: true, on: { capability: "workspace.pick", nth: (counts["workspace.pick"] || 0) + 1 } } });
      } else {
        act("sim", { control: what });
      }
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
