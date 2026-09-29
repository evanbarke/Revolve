//! Scripted (offline, deterministic, rule-based) planner for the kit task family.
//!
//! `decide` is the shared task-selection policy. Revolve's scripted planner turns
//! its output into a whole Action IR plan; the scripted direct-tool agent takes
//! only the first step each turn from a fresh observation. Sharing it keeps the
//! baseline from being a deliberately weaker fixed sequence.
//!
//! This is NOT an AI interpretation of free text. It supports exactly the
//! structured goal and the restricted request grammar in `model.rs`.

use super::{
    AgentDecision, AgentReport, AgentRequest, AnsweredQuestion, CallInfo, Clarification, Planner, PlannerError, PlannerResponse,
    PlanningRequest,
};
use crate::inventory::InventoryView;
use crate::ir::{
    make_ref, Budgets, Condition, ExpectedOutcome, IncompleteKit, KitAssignment, PlanBasis, PlanHeader, RetryPolicy, Step,
    UnknownOutcomePolicy, ACTION_IR_VERSION,
};
use crate::model::{
    bin_for_kind, effective_kind, evaluate_kit, substitution_candidates, Classification, GoalSpec, KindCounts,
    Observation, UNKNOWN_KIND,
};
use crate::store::QuestionOption;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub struct PolicyInput<'a> {
    pub goal: &'a GoalSpec,
    pub goal_revision: u32,
    pub obs: &'a Observation,
    pub classes: &'a [Classification],
    pub inventory: &'a InventoryView,
    pub consumed: &'a [String],
    pub previous_assignments: &'a [KitAssignment],
    pub answered: &'a [AnsweredQuestion],
}

pub struct PolicyPlan {
    pub steps: Vec<Step>,
    pub assignments: Vec<KitAssignment>,
    pub outcome: ExpectedOutcome,
    pub explanation: String,
}

pub enum PolicyOutput {
    Plan(PolicyPlan),
    Clarify(Clarification),
    Unable(String),
}

fn tray_num(id: &str) -> u32 {
    id.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(u32::MAX)
}

fn opt(id: &str, label: &str) -> QuestionOption {
    QuestionOption { id: id.into(), label: label.into() }
}

struct StepBuilder {
    steps: Vec<Step>,
    revision: u32,
}

impl StepBuilder {
    fn push(&mut self, step_id: String, capability: &str, arguments: Value, pre: Vec<Condition>, post: Vec<Condition>) {
        let depends_on = self.steps.last().map(|s| vec![s.step_id.clone()]).unwrap_or_default();
        let arguments = arguments.as_object().cloned().unwrap_or_default().into_iter().collect();
        self.steps.push(Step {
            step_id,
            capability: capability.into(),
            capability_version: "1".into(),
            depends_on,
            arguments,
            preconditions: pre,
            postconditions: post,
            on_unknown_outcome: UnknownOutcomePolicy::ReconcileBeforeRetry,
            retry: RetryPolicy { max_attempts: 2 },
            note: None,
        });
    }

    fn pick(&mut self, id: String, obj: &str, src: &str) {
        let pre = vec![
            Condition::GoalRevisionEquals { revision: self.revision },
            Condition::GripperEmpty,
            Condition::ObjectAt { object_id: obj.into(), location_id: src.into() },
            Condition::LocationAvailable { location_id: src.into() },
        ];
        let post = vec![Condition::GripperHolds { object_id: obj.into() }];
        self.push(id, "workspace.pick", json!({"object_id": obj, "source_id": src}), pre, post);
    }

    fn place(&mut self, id: String, obj: &str, dst: &str) {
        let pre = vec![Condition::GripperHolds { object_id: obj.into() }, Condition::LocationAvailable { location_id: dst.into() }];
        let post = vec![Condition::ObjectAt { object_id: obj.into(), location_id: dst.into() }, Condition::GripperEmpty];
        self.push(id, "workspace.place", json!({"object_id": obj, "destination_id": dst}), pre, post);
    }
}

/// The shared task-selection policy.
pub fn decide(inp: &PolicyInput) -> PolicyOutput {
    let obs = inp.obs;
    let goal = inp.goal;
    if !obs.withheld.is_empty() {
        return PolicyOutput::Unable(format!(
            "observation is incomplete (withheld: {}); cannot plan safely until evidence is available",
            obs.withheld.join(", ")
        ));
    }
    let kind_of = |o: &str| obs.object(o).map(|v| effective_kind(v, inp.classes)).unwrap_or_default();
    let consumed: BTreeSet<&str> = inp.consumed.iter().map(String::as_str).collect();
    let mut trays: Vec<(&str, bool)> = obs.trays().iter().map(|l| (l.location_id.as_str(), l.available)).collect();
    trays.sort_by_key(|(t, _)| tray_num(t));
    let contents = |t: &str| -> Vec<String> { obs.objects_at(t).map(|o| o.object_id.clone()).collect() };
    let counts_of = |objs: &[String]| -> KindCounts {
        let mut c = KindCounts::new();
        for o in objs {
            *c.entry(kind_of(o)).or_default() += 1;
        }
        c
    };

    // ---- 1. kit -> tray assignments
    let q = goal.kit_quantity;
    let mut assign: BTreeMap<u32, String> = BTreeMap::new();
    let mut stranded = vec![];
    for ka in inp.previous_assignments {
        if ka.kit > q {
            continue;
        }
        match trays.iter().find(|(t, _)| *t == ka.tray_id) {
            Some((_, true)) => {
                assign.insert(ka.kit, ka.tray_id.clone());
            }
            Some((t, false)) if !contents(t).is_empty() => stranded.push(format!("{t} (unavailable; holds {})", contents(t).join(", "))),
            _ => {}
        }
    }
    let mut free: Vec<&str> = trays
        .iter()
        .filter(|(t, a)| *a && !assign.values().any(|v| v == t))
        .map(|(t, _)| *t)
        .collect();
    // Prefer trays that already hold parts (continue partial kits), then empty trays.
    free.sort_by_key(|t| (contents(t).is_empty(), tray_num(t)));
    let mut incomplete: Vec<IncompleteKit> = vec![];
    for k in 1..=q {
        if assign.contains_key(&k) {
            continue;
        }
        if free.is_empty() {
            incomplete.push(IncompleteKit { kit: k, tray_id: None, missing: goal.kit_components.clone(), reason: "no available tray".into() });
        } else {
            assign.insert(k, free.remove(0).to_string());
        }
    }
    // Available trays with parts that are not assigned to any kit (e.g. after a quantity reduction).
    let extra_trays: Vec<&str> = trays
        .iter()
        .filter(|(t, a)| *a && !assign.values().any(|v| v == t) && !contents(t).is_empty())
        .map(|(t, _)| *t)
        .collect();

    // ---- 2. excess parts -> rework decision
    let mut excess_objs: Vec<(String, String)> = vec![]; // (object, tray)
    for tray in assign.values() {
        let objs = contents(tray);
        let (_, _, excess) = evaluate_kit(&counts_of(&objs), goal);
        let mut ex = excess.clone();
        // Choose which physical objects are excess: last ones of each excess kind.
        for o in objs.iter().rev() {
            let k = kind_of(o);
            if let Some(n) = ex.get_mut(&k) {
                if *n > 0 {
                    *n -= 1;
                    excess_objs.push((o.clone(), tray.clone()));
                }
            }
        }
    }
    for t in &extra_trays {
        for o in contents(t) {
            excess_objs.push((o, t.to_string()));
        }
    }
    let mut nonconforming: BTreeSet<String> = BTreeSet::new();
    if !excess_objs.is_empty() {
        match goal.rework_permitted {
            None => {
                let desc: Vec<String> = excess_objs.iter().map(|(o, t)| format!("{o} ({}) in {t}", kind_of(o))).collect();
                return PolicyOutput::Clarify(Clarification {
                    kind: "rework".into(),
                    prompt: format!(
                        "Under the current goal these parts are not needed where they are: {}. May I move them back to stock (rework)?",
                        desc.join("; ")
                    ),
                    options: vec![opt("approve", "Yes, return them to stock"), opt("decline", "No, leave them; report those trays as non-conforming")],
                    context: json!({"excess": excess_objs.iter().map(|(o, t)| json!({"object_id": o, "tray_id": t})).collect::<Vec<_>>()}),
                });
            }
            Some(false) => {
                for (_, t) in &excess_objs {
                    nonconforming.insert(t.clone());
                }
                excess_objs.clear();
            }
            Some(true) => {}
        }
    }
    let excess_set: BTreeSet<&str> = excess_objs.iter().map(|(o, _)| o.as_str()).collect();

    // ---- 3. stock pool (after rework): bins by kind, plus the held object if it is stock
    let held = obs.gripper_contents().map(|o| o.object_id.clone());
    let mut pool: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for bin in obs.bins() {
        for o in obs.objects_at(&bin.location_id) {
            let k = effective_kind(o, inp.classes);
            if k != UNKNOWN_KIND && k != "not_usable" && bin_for_kind(&k) == bin.location_id {
                pool.entry(k).or_default().push(o.object_id.clone());
            }
        }
    }
    for (o, _) in &excess_objs {
        pool.entry(kind_of(o)).or_default().push(o.clone());
    }
    let held_is_stock = held.as_ref().is_some_and(|h| !consumed.contains(h.as_str()));
    if let Some(h) = &held {
        let k = kind_of(h);
        if k == UNKNOWN_KIND {
            return PolicyOutput::Unable(format!("gripper holds {h}, which is unclassified; a human must classify or remove it"));
        }
        // Held objects are placed first; if stock, prefer them in allocation.
        pool.entry(k).or_default().insert(0, h.clone());
    }

    // ---- 4. allocation per kit
    let mut kit_plan: Vec<(u32, String, Vec<(String, String)>)> = vec![]; // (kit, tray, [(object, kind)])
    let mut complete_trays: Vec<String> = vec![];
    let mut short_kinds: BTreeSet<String> = BTreeSet::new();
    for (&k, tray) in &assign {
        if nonconforming.contains(tray) {
            let (_, missing, _) = evaluate_kit(&counts_of(&contents(tray)), goal);
            incomplete.push(IncompleteKit { kit: k, tray_id: Some(tray.clone()), missing, reason: "tray holds parts not in the composition and rework was declined".into() });
            continue;
        }
        let kept: Vec<String> = contents(tray).into_iter().filter(|o| !excess_set.contains(o.as_str())).collect();
        let (done, missing, _) = evaluate_kit(&counts_of(&kept), goal);
        if done {
            complete_trays.push(tray.clone());
            continue;
        }
        // Try to allocate the missing parts (exact kind first, then approved substitutes).
        let mut trial = pool.clone();
        let mut picks = vec![];
        let mut ok = true;
        let mut kit_missing = KindCounts::new();
        for (kind, n) in &missing {
            for _ in 0..*n {
                let mut got = None;
                let mut cands = vec![kind.clone()];
                cands.extend(goal.substitute_for(kind));
                for c in cands {
                    if let Some(v) = trial.get_mut(&c) {
                        if !v.is_empty() {
                            got = Some((v.remove(0), c));
                            break;
                        }
                    }
                }
                match got {
                    Some(p) => picks.push(p),
                    None => {
                        ok = false;
                        *kit_missing.entry(kind.clone()).or_default() += 1;
                        short_kinds.insert(kind.clone());
                    }
                }
            }
        }
        if ok {
            pool = trial;
            complete_trays.push(tray.clone());
            kit_plan.push((k, tray.clone(), picks));
        } else {
            incomplete.push(IncompleteKit { kit: k, tray_id: Some(tray.clone()), missing: kit_missing, reason: "insufficient stock".into() });
        }
    }

    // ---- 5. questions that could resolve a shortage
    if !short_kinds.is_empty() {
        let answered_obj = |o: &str| inp.classes.iter().any(|c| c.object_id == o);
        for bin in obs.bins() {
            let Some(acc) = &bin.accepts_kind else { continue };
            if !short_kinds.contains(acc) {
                continue;
            }
            if let Some(u) = obs.objects_at(&bin.location_id).find(|o| o.observed_kind == UNKNOWN_KIND && !answered_obj(&o.object_id)) {
                return PolicyOutput::Clarify(Clarification {
                    kind: "classification".into(),
                    prompt: format!(
                        "Stock is short of {acc}. Item {} in {} could not be classified. Is it a usable {acc}?",
                        u.object_id, bin.location_id
                    ),
                    options: vec![opt(acc, &format!("Yes, it is a {acc}")), opt("not_usable", "No, do not use it")],
                    context: json!({"object_id": u.object_id, "bin": bin.location_id}),
                });
            }
        }
        for kind in &short_kinds {
            for cand in substitution_candidates(kind) {
                if goal.is_approved(kind, cand) || goal.is_declined(kind, cand) {
                    continue;
                }
                let spare = pool.get(*cand).map(|v| v.len()).unwrap_or(0);
                if spare > 0 {
                    return PolicyOutput::Clarify(Clarification {
                        kind: "substitution".into(),
                        prompt: format!(
                            "Not enough {kind} to finish every kit. {spare} spare {cand}(s) are in stock. Substitute {cand} for {kind}?"
                        ),
                        options: vec![opt("approve", &format!("Approve {cand} in place of {kind}")), opt("decline", "Decline; report what is missing")],
                        context: json!({"replace": kind, "with": cand, "spare": spare}),
                    });
                }
            }
        }
    }

    // ---- 6. steps
    let mut b = StepBuilder { steps: vec![], revision: inp.goal_revision };
    // Reservation needs: allocated stock parts + parts in kit trays not yet recorded as consumed.
    let mut needed = KindCounts::new();
    let mut to_consume_in_place: Vec<(String, String)> = vec![]; // (object, tray)
    for (_, tray, picks) in &kit_plan {
        for (o, k) in picks {
            if !(held.as_deref() == Some(o.as_str()) && !held_is_stock) {
                *needed.entry(k.clone()).or_default() += 1;
            }
        }
        let _ = tray;
    }
    for tray in assign.values() {
        if nonconforming.contains(tray) {
            continue;
        }
        for o in contents(tray) {
            if !excess_set.contains(o.as_str()) && !consumed.contains(o.as_str()) {
                *needed.entry(kind_of(&o)).or_default() += 1;
                to_consume_in_place.push((o, tray.clone()));
            }
        }
    }
    let open: Vec<_> = inp.inventory.open_reservations().collect();
    let mut res_ref: Option<Value> = None;
    let covers = |rem: &KindCounts| needed.iter().all(|(k, n)| rem.get(k).copied().unwrap_or(0) >= *n);
    if let Some(r) = open.iter().find(|r| covers(&r.remaining)) {
        res_ref = Some(Value::String(r.reservation_id.clone()));
    } else {
        for r in &open {
            b.push(format!("release-{}", r.reservation_id), "inventory.release", json!({"reservation_id": r.reservation_id}), vec![], vec![]);
        }
    }
    let needs_reservation = needed.values().any(|n| *n > 0);
    if res_ref.is_none() && needs_reservation {
        let counts: KindCounts = needed.iter().filter(|(_, n)| **n > 0).map(|(k, n)| (k.clone(), *n)).collect();
        let sid = format!("reserve-r{}", inp.goal_revision);
        b.push(sid.clone(), "inventory.reserve", json!({"counts": counts}), vec![], vec![]);
        res_ref = Some(make_ref(&sid, "reservation_id"));
    }
    // Held object: place into its kit if allocated, otherwise return to stock.
    let mut held_done = false;
    if let Some(h) = &held {
        if let Some((_, tray, _)) = kit_plan.iter().find(|(_, _, p)| p.iter().any(|(o, _)| o == h)) {
            b.place(format!("place-{h}-{tray}"), h, tray);
            if held_is_stock {
                b.push(format!("consume-{h}"), "inventory.consume", json!({"reservation_id": res_ref.clone().unwrap_or(Value::Null), "part_kind": kind_of(h), "quantity": 1, "evidence_object_id": h}), vec![Condition::ObjectAt { object_id: h.clone(), location_id: tray.clone() }], vec![]);
            }
        } else {
            let bin = bin_for_kind(&kind_of(h));
            b.place(format!("return-place-{h}"), h, &bin);
            if !held_is_stock {
                b.push(format!("return-{h}"), "inventory.return", json!({"part_kind": kind_of(h), "quantity": 1, "evidence_object_id": h}), vec![], vec![]);
            }
        }
        held_done = true;
    }
    // Consumption for parts already in kit trays but not yet recorded.
    for (o, tray) in &to_consume_in_place {
        b.push(format!("consume-{o}"), "inventory.consume", json!({"reservation_id": res_ref.clone().unwrap_or(Value::Null), "part_kind": kind_of(o), "quantity": 1, "evidence_object_id": o}), vec![Condition::ObjectAt { object_id: o.clone(), location_id: tray.clone() }], vec![]);
    }
    // Rework: return excess parts to stock.
    for (o, tray) in &excess_objs {
        let k = kind_of(o);
        let bin = bin_for_kind(&k);
        b.pick(format!("rw-pick-{o}"), o, tray);
        b.place(format!("rw-place-{o}"), o, &bin);
        if consumed.contains(o.as_str()) {
            b.push(format!("rw-return-{o}"), "inventory.return", json!({"part_kind": k, "quantity": 1, "evidence_object_id": o}), vec![], vec![]);
        }
    }
    // Fill kits.
    for (_, tray, picks) in &kit_plan {
        for (o, k) in picks {
            if held_done && held.as_deref() == Some(o.as_str()) {
                continue;
            }
            let src = obs
                .object(o)
                .map(|v| v.location_id.clone())
                .unwrap_or_else(|| excess_objs.iter().find(|(x, _)| x == o).map(|_| bin_for_kind(k)).unwrap_or_default());
            // Excess objects will have been returned to their bin by the rework steps.
            let src = if excess_set.contains(o.as_str()) { bin_for_kind(k) } else { src };
            b.pick(format!("pick-{o}"), o, &src);
            b.place(format!("place-{o}-{tray}"), o, tray);
            b.push(format!("consume-{o}"), "inventory.consume", json!({"reservation_id": res_ref.clone().unwrap_or(Value::Null), "part_kind": k, "quantity": 1, "evidence_object_id": o}), vec![Condition::ObjectAt { object_id: o.clone(), location_id: tray.clone() }], vec![]);
        }
    }
    if let Some(r) = &res_ref {
        if needs_reservation || r.is_string() {
            let id = match r.as_str() {
                Some(s) => s.to_string(),
                None => format!("r{}", inp.goal_revision),
            };
            b.push(format!("release-final-{id}"), "inventory.release", json!({"reservation_id": r}), vec![], vec![]);
        }
    }

    complete_trays.sort_by_key(|t| tray_num(t));
    let assignments: Vec<KitAssignment> = assign.iter().map(|(k, t)| KitAssignment { kit: *k, tray_id: t.clone() }).collect();
    incomplete.sort_by_key(|i| i.kit);
    let mut explanation = format!(
        "Goal r{}: {}. {} kit(s) expected complete ({}).",
        inp.goal_revision,
        goal.summary(),
        complete_trays.len(),
        if complete_trays.is_empty() { "none".into() } else { complete_trays.join(", ") }
    );
    if !incomplete.is_empty() {
        let parts: Vec<String> = incomplete
            .iter()
            .map(|i| {
                let m: Vec<String> = i.missing.iter().map(|(k, n)| format!("{n} {k}")).collect();
                format!("kit {} not completable ({}; missing {})", i.kit, i.reason, m.join(", "))
            })
            .collect();
        explanation.push_str(&format!(" {}.", parts.join("; ")));
    }
    if !stranded.is_empty() {
        explanation.push_str(&format!(" Stranded: {}.", stranded.join("; ")));
    }
    if !excess_objs.is_empty() {
        explanation.push_str(&format!(" Rework: return {} part(s) to stock.", excess_objs.len()));
    }
    PolicyOutput::Plan(PolicyPlan {
        steps: b.steps,
        assignments,
        outcome: ExpectedOutcome { kits_complete: complete_trays, kits_incomplete: incomplete },
        explanation,
    })
}

pub struct ScriptedPlanner;

impl ScriptedPlanner {
    fn proposal(req: &PlanningRequest, p: PolicyPlan) -> Value {
        let mut caps = BTreeMap::new();
        for s in &p.steps {
            caps.insert(s.capability.clone(), s.capability_version.clone());
        }
        let perms: Vec<String> = caps.keys().cloned().collect();
        let header = PlanHeader {
            schema_version: ACTION_IR_VERSION.into(),
            plan_id: format!("scripted-r{}-w{}", req.goal_revision, req.observation.world_revision),
            goal_id: req.goal_id.clone(),
            goal_revision: req.goal_revision,
            basis: PlanBasis { world_revision: req.observation.world_revision, max_observation_age_ms: Some(5_000) },
            capabilities: caps,
            requested_permissions: perms,
            budgets: Budgets {
                max_actions: req.limits.max_actions,
                max_planner_rounds: req.limits.max_planner_decisions,
                max_elapsed_ms: req.limits.max_elapsed_ms,
            },
            kit_assignments: p.assignments,
            explanation: p.explanation,
            expected_outcome: p.outcome,
        };
        let mut v = serde_json::to_value(&header).unwrap_or(Value::Null);
        v["steps"] = serde_json::to_value(&p.steps).unwrap_or(Value::Null);
        v
    }
}

impl Planner for ScriptedPlanner {
    fn label(&self) -> String {
        "scripted".into()
    }

    fn plan(&mut self, req: &PlanningRequest) -> (Result<PlannerResponse, PlannerError>, CallInfo) {
        let t = std::time::Instant::now();
        let inp = PolicyInput {
            goal: &req.goal,
            goal_revision: req.goal_revision,
            obs: &req.observation,
            classes: &req.classifications,
            inventory: &req.inventory,
            consumed: &req.summary.consumed_evidence,
            previous_assignments: &req.summary.previous_kit_assignments,
            answered: &req.summary.answered_questions,
        };
        let r = match decide(&inp) {
            PolicyOutput::Plan(p) => PlannerResponse::Plan(Self::proposal(req, p)),
            PolicyOutput::Clarify(c) => PlannerResponse::Clarify(c),
            PolicyOutput::Unable(reason) => PlannerResponse::Unable { reason },
        };
        (Ok(r), CallInfo { latency_ms: t.elapsed().as_millis() as i64, transport_attempts: 1, usage: None, detail: None })
    }

    fn next_action(&mut self, req: &AgentRequest) -> (Result<AgentDecision, PlannerError>, CallInfo) {
        let t = std::time::Instant::now();
        let d = scripted_agent_decision(req);
        (Ok(d), CallInfo { latency_ms: t.elapsed().as_millis() as i64, transport_attempts: 1, usage: None, detail: None })
    }
}

/// The scripted direct-tool agent: recover from the last tool result if needed,
/// keep the inventory record in line with observation, then take the first step
/// of the shared policy's plan from the current observation.
pub fn scripted_agent_decision(req: &AgentRequest) -> AgentDecision {
    // 1. Recovery from an unknown outcome.
    if let Some(last) = req.history.last() {
        let pending_key = last.op_key.clone().unwrap_or_default();
        match last.outcome.as_str() {
            "timeout" | "unknown_after_restart" => return AgentDecision::Status { op_key: pending_key },
            "status:unavailable" => {
                let tries = req.history.iter().rev().take_while(|h| h.outcome == "status:unavailable").count();
                let evidence_back = req.observation.as_ref().is_some_and(|o| o.withheld.is_empty());
                if tries < 3 || evidence_back {
                    return AgentDecision::Status { op_key: pending_key };
                }
                return AgentDecision::Finish(AgentReport {
                    status: "blocked".into(),
                    kits_complete: vec![],
                    message: format!("cannot establish whether operation {pending_key} took effect: status and evidence unavailable"),
                });
            }
            "status:not_found" => {
                // The original request had no effect; retry it under the same operation key.
                if let Some(orig) = req.history.iter().rev().find(|h| h.op_key.as_deref() == Some(&pending_key) && !h.outcome.starts_with("status:")) {
                    return AgentDecision::Call {
                        capability: orig.capability.clone(),
                        version: "1".into(),
                        arguments: orig.arguments.clone(),
                        retry_of: Some(orig.action_id),
                    };
                }
            }
            _ => {}
        }
    }
    let Some(obs) = &req.observation else {
        return AgentDecision::Finish(AgentReport { status: "blocked".into(), kits_complete: vec![], message: "no observation available".into() });
    };
    // 2. Inventory record vs observation.
    let observed = crate::store::unconsumed_counts(obs, &req.classifications, &req.consumed_evidence);
    if obs.withheld.is_empty() {
        for (k, n) in &observed {
            let rec = req.inventory.on_hand.get(k).copied().unwrap_or(0);
            if rec != *n {
                if req.inventory.reserved.get(k).copied().unwrap_or(0) > *n {
                    if let Some(r) = req.inventory.open_reservations().next() {
                        return AgentDecision::Call {
                            capability: "inventory.release".into(),
                            version: "1".into(),
                            arguments: json!({"reservation_id": r.reservation_id}),
                            retry_of: None,
                        };
                    }
                }
                return AgentDecision::Call {
                    capability: "inventory.adjust_count".into(),
                    version: "1".into(),
                    arguments: json!({"part_kind": k, "observed_count": n, "observation_revision": obs.world_revision}),
                    retry_of: None,
                };
            }
        }
    }
    // 3. Shared policy; the agent tracks kit assignments through the trays it has used.
    let prev: Vec<KitAssignment> = previous_assignments_from_history(&req.history, req.goal.kit_quantity);
    let inp = PolicyInput {
        goal: &req.goal,
        goal_revision: req.goal_revision,
        obs,
        classes: &req.classifications,
        inventory: &req.inventory,
        consumed: &req.consumed_evidence,
        previous_assignments: &prev,
        answered: &req.answered_questions,
    };
    match decide(&inp) {
        PolicyOutput::Clarify(c) => AgentDecision::Ask(c),
        PolicyOutput::Unable(reason) => AgentDecision::Finish(AgentReport { status: "blocked".into(), kits_complete: vec![], message: reason }),
        PolicyOutput::Plan(p) => match p.steps.first() {
            None => {
                let n = p.outcome.kits_complete.len() as u32;
                AgentDecision::Finish(AgentReport {
                    status: if n >= req.goal.kit_quantity { "completed".into() } else { "partial".into() },
                    kits_complete: p.outcome.kits_complete.clone(),
                    message: p.explanation,
                })
            }
            Some(s) => {
                let mut args = serde_json::Map::new();
                for (k, v) in &s.arguments {
                    // The first step never references another step, except a reservation
                    // opened earlier, which the agent reads from the inventory view.
                    let v = if v.get("$ref").is_some() {
                        req.inventory.open_reservations().next().map(|r| Value::String(r.reservation_id.clone())).unwrap_or(Value::Null)
                    } else {
                        v.clone()
                    };
                    args.insert(k.clone(), v);
                }
                AgentDecision::Call { capability: s.capability.clone(), version: s.capability_version.clone(), arguments: Value::Object(args), retry_of: None }
            }
        },
    }
}

/// Reconstruct kit->tray assignments from the trays the agent has placed parts into, in first-use order.
fn previous_assignments_from_history(history: &[super::ToolRecord], q: u32) -> Vec<KitAssignment> {
    let mut trays: Vec<String> = vec![];
    for h in history {
        if h.capability == "workspace.place" && h.outcome == "applied" {
            if let Some(d) = h.arguments.get("destination_id").and_then(Value::as_str) {
                if d.starts_with("tray-") && !trays.iter().any(|t| t == d) {
                    trays.push(d.to_string());
                }
            }
        }
    }
    trays.into_iter().take(q as usize).enumerate().map(|(i, t)| KitAssignment { kit: i as u32 + 1, tray_id: t }).collect()
}
