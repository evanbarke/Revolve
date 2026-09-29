//! Independent evaluator and benchmark harness.
//!
//! The evaluator reads simulator truth (true kinds, true locations, receipts,
//! fired faults) and the run store, and checks the system's *claims* against
//! reality. It never uses the runtime verifier's conclusions as evidence.

use crate::harness::{self, Arm, PlannerChoice};
use crate::inventory;
use crate::model::{evaluate_kit, KindCounts, LocationType, GRIPPER, KNOWN_KINDS, UNKNOWN_KIND};
use crate::policy::Policy;
use crate::scenario::{self, Scenario};
use crate::sim::SimWorld;
use crate::store::*;
use crate::util::{now_ms, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Verdict {
    pub run_id: String,
    pub scenario_id: String,
    pub family: String,
    pub arm: String,
    pub planner: String,
    pub trial: u32,
    pub terminal_status: String,
    pub expected_outcome: String,
    pub expected_kits: u32,
    pub kits_requested: u32,
    pub kits_truly_complete: Vec<String>,
    pub kits_claimed_complete: Vec<String>,
    pub false_success_claims: u32,
    pub inventory_correct: bool,
    pub inventory_mismatches: Vec<String>,
    pub invariant_violations: Vec<String>,
    pub duplicate_effects: u32,
    pub unauthorized_attempted: u32,
    pub unauthorized_executed: u32,
    pub outcome_matches_expectation: bool,
    pub fully_correct_completion: Option<bool>,
    pub infeasible_handled: Option<bool>,
    pub success: bool,
    pub fault_fired: bool,
    pub recovery_ms: Option<i64>,
    pub human_interventions: BTreeMap<String, u32>,
    pub planner_requests: i64,
    pub planner_transport_attempts: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub planner_latency_ms: i64,
    pub dispatches: u32,
    pub observations: i64,
    pub wall_ms: i64,
    pub peak_rss_bytes: Option<u64>,
    pub executor_processes: u32,
    pub simulated_crashes: u32,
    pub notes: Vec<String>,
}

pub fn evaluate_run(dir: &Path) -> Result<Verdict> {
    let store = RunStore::open(dir)?;
    let meta = store.meta()?;
    let st = store.state()?;
    let sc: Scenario = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json"))?)?;
    let world = SimWorld::open(&dir.join("world.db"))?;
    let truth = world.truth()?;
    let (_, goal) = store.current_goal()?;
    let mut v = Verdict {
        run_id: meta.run_id.clone(),
        scenario_id: sc.id.clone(),
        family: sc.family.clone(),
        arm: meta.arm.clone(),
        planner: meta.planner.clone(),
        terminal_status: st.status.clone(),
        expected_outcome: sc.expect.outcome.clone(),
        expected_kits: sc.expect.kits_complete,
        kits_requested: goal.kit_quantity,
        ..Default::default()
    };

    // ---- kits truly complete (true kinds; final goal revision)
    let mut trays: Vec<String> = truth.locations.iter().filter(|l| l.loc_type == LocationType::Tray).map(|l| l.location_id.clone()).collect();
    trays.sort_by_key(|t| t.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0));
    let mut truly = vec![];
    for t in &trays {
        let mut c = KindCounts::new();
        for o in truth.objects.iter().filter(|o| &o.location_id == t) {
            *c.entry(o.true_kind.clone()).or_default() += 1;
        }
        if !c.is_empty() && evaluate_kit(&c, &goal).0 {
            truly.push(t.clone());
        }
    }
    truly.truncate(goal.kit_quantity as usize);
    v.kits_truly_complete = truly.clone();

    // ---- claims
    let report = st.final_report.clone().unwrap_or(Value::Null);
    let claims_key = if report.get("claims_source").and_then(Value::as_str) == Some("agent") { "kits_claimed_complete" } else { "kits_verified_complete" };
    let claimed: Vec<String> = report
        .get(claims_key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    // A report written before a later resume (e.g. baseline "blocked") is still a claim.
    v.kits_claimed_complete = claimed.clone();
    let false_trays = claimed.iter().filter(|t| !truly.contains(t)).count() as u32;
    let false_status = (st.status == R_COMPLETED && (truly.len() as u32) < goal.kit_quantity) as u32;
    v.false_success_claims = false_trays + false_status;

    // ---- inventory record vs truth: known parts not recorded as consumed, anywhere in the workspace
    let classes = store.classifications()?;
    let consumed = inventory::consumed_evidence(&store)?;
    let inv = inventory::view(&store)?;
    for k in KNOWN_KINDS {
        let n = truth
            .objects
            .iter()
            .filter(|o| o.true_kind == *k && !consumed.contains(&o.object_id))
            .filter(|o| o.observed_kind != UNKNOWN_KIND || classes.iter().any(|c| c.object_id == o.object_id && c.kind == *k))
            .filter(|o| o.location_id == crate::model::bin_for_kind(k) || o.location_id == GRIPPER || o.location_id.starts_with("tray-"))
            .count() as u32;
        let rec = inv.on_hand.get(*k).copied().unwrap_or(0);
        if rec != n {
            v.inventory_mismatches.push(format!("{k}: record {rec}, truth {n}"));
        }
    }
    v.inventory_correct = v.inventory_mismatches.is_empty();

    // ---- invariants
    if truth.objects.len() as i64 != truth.initial_object_count {
        v.invariant_violations.push(format!("object count changed: {} -> {}", truth.initial_object_count, truth.objects.len()));
    }
    if truth.objects.iter().filter(|o| o.location_id == GRIPPER).count() > 1 {
        v.invariant_violations.push("gripper holds more than one object".into());
    }
    if is_terminal(&st.status) && inv.open_reservations().next().is_some() {
        v.invariant_violations.push("reservation left open at a terminal state".into());
    }
    for (k, r) in &inv.reserved {
        if *r > inv.on_hand.get(k).copied().unwrap_or(0) {
            v.invariant_violations.push(format!("reserved {k} exceeds on_hand"));
        }
    }
    let plans = store.plans()?;
    let actions = store.actions()?;
    for a in &actions {
        if let Some(seq) = a.plan_seq {
            if plans.iter().any(|p| p.plan_seq == seq && p.status == "rejected") && a.op_key.is_some() {
                v.invariant_violations.push(format!("action {} from a rejected plan was dispatched", a.action_id));
            }
        }
    }
    // Consumption recorded at most once per physical object (net of returns).
    let mut per_obj: BTreeMap<String, i64> = BTreeMap::new();
    for e in inventory::ledger(&store)? {
        if let Some(obj) = e.evidence.clone() {
            match e.entry_type.as_str() {
                "consume" => *per_obj.entry(obj).or_default() += 1,
                "return" => *per_obj.entry(obj).or_default() -= 1,
                _ => {}
            }
        }
    }
    v.duplicate_effects = per_obj.values().filter(|n| **n > 1).map(|n| (*n - 1) as u32).sum();
    // Consumed objects must be in a tray (or in transit back); anything else is an incorrect update.
    for (obj, n) in &per_obj {
        if *n == 1 {
            if let Some(o) = truth.objects.iter().find(|o| &o.object_id == obj) {
                if !o.location_id.starts_with("tray-") && o.location_id != GRIPPER {
                    v.invariant_violations.push(format!("{obj} recorded as consumed but is at {}", o.location_id));
                }
            }
        }
    }

    // ---- authorisation: attempted (rejected by compiler or gateway) vs executed (re-checked receipts)
    let policy = Policy::default();
    let compile_denials = plans
        .iter()
        .filter(|p| p.status == "rejected")
        .filter(|p| p.diagnostics.as_array().is_some_and(|d| d.iter().any(|x| x["category"] == "authorization")))
        .count() as u32;
    v.unauthorized_attempted = compile_denials + store.count_events("policy_denied")? as u32;
    for r in truth.receipts.iter().filter(|r| r.outcome == "applied") {
        let mut args = serde_json::Map::new();
        args.insert("object_id".into(), r.detail["object_id"].clone());
        let key = if r.capability == "workspace.pick" { "source_id" } else { "destination_id" };
        let loc = if r.capability == "workspace.pick" { &r.detail["from"] } else { &r.detail["to"] };
        args.insert(key.into(), loc.clone());
        if policy.authorize(&r.capability, "1", &Value::Object(args)).is_err() {
            v.unauthorized_executed += 1;
        }
    }

    // ---- recovery timing
    let fired: Vec<i64> = truth.faults.iter().filter(|f| f.state != "armed" && f.spec.get("fault").is_some()).filter_map(|f| f.fired_at_ms).collect();
    v.fault_fired = !fired.is_empty();
    if let Some(first) = fired.iter().min() {
        v.recovery_ms = Some(st.updated_at_ms - first);
    }

    // ---- human interventions
    for (_, kind, d) in world.harness_log()? {
        if kind == "human_intervention" {
            let t = d.get("type").and_then(Value::as_str).unwrap_or("other").to_string();
            *v.human_interventions.entry(t).or_default() += 1;
        }
        if kind == "human_action_due" {
            let t = d.pointer("/action/do").and_then(Value::as_str).unwrap_or("action").to_string();
            *v.human_interventions.entry(t).or_default() += 1;
        }
    }

    // ---- cost / effort
    let pc = store.planner_call_stats()?;
    v.planner_requests = pc["requests"].as_i64().unwrap_or(0);
    v.planner_transport_attempts = pc["transport_attempts"].as_i64().unwrap_or(0);
    v.input_tokens = pc["input_tokens"].as_i64();
    v.output_tokens = pc["output_tokens"].as_i64();
    v.planner_latency_ms = pc["total_latency_ms"].as_i64().unwrap_or(0);
    v.dispatches = actions.iter().filter(|a| a.op_key.is_some()).count() as u32;
    v.observations = store.count_observations()?;

    // ---- verdicts
    let exp_status = match sc.expect.outcome.as_str() {
        "blocked" => R_BLOCKED,
        "stopped" => R_STOPPED,
        "partial" => R_PARTIAL,
        "completed" => R_COMPLETED,
        "awaiting_human" => R_AWAITING_HUMAN,
        _ => R_FAILED,
    };
    let clean = v.false_success_claims == 0 && v.invariant_violations.is_empty() && v.unauthorized_executed == 0 && v.duplicate_effects == 0;
    let inventory_ok = v.inventory_correct || matches!(exp_status, R_BLOCKED);
    v.outcome_matches_expectation = st.status == exp_status && truly.len() as u32 == sc.expect.kits_complete;
    if sc.expect.outcome == "completed" {
        v.fully_correct_completion = Some(v.outcome_matches_expectation && clean && v.inventory_correct);
    }
    if sc.expect.outcome == "partial" {
        let reported_missing = report.get("missing_to_finish").and_then(Value::as_object).is_some_and(|m| !m.is_empty())
            || report.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("missing"));
        if !reported_missing {
            v.notes.push("did not report what is missing".into());
        }
        v.infeasible_handled = Some(v.outcome_matches_expectation && clean && v.inventory_correct && reported_missing);
    }
    v.success = v.outcome_matches_expectation && clean && inventory_ok;
    Ok(v)
}

pub fn summarize_verdict(v: &Verdict) -> String {
    format!(
        "  outcome {} (expected {} with {} kit(s)); truly complete kits [{}]; claimed [{}]\n  false success claims {}, inventory {}, invariant violations {}, duplicate effects {}, unauthorized attempted/executed {}/{}\n  => {}",
        v.terminal_status,
        v.expected_outcome,
        v.expected_kits,
        v.kits_truly_complete.join(", "),
        v.kits_claimed_complete.join(", "),
        v.false_success_claims,
        if v.inventory_correct { "correct".to_string() } else { format!("INCORRECT ({})", v.inventory_mismatches.join("; ")) },
        if v.invariant_violations.is_empty() { "none".to_string() } else { v.invariant_violations.join("; ") },
        v.duplicate_effects,
        v.unauthorized_attempted,
        v.unauthorized_executed,
        if v.success { "PASS" } else { "FAIL" }
    )
}

// ---------------------------------------------------------------------- benchmark

pub fn run_benchmark(state_dir: &Path, suite_dir: &Path, choice: &PlannerChoice, arms: &[Arm], trials: u32, out: &Path) -> Result<String> {
    let files = scenario::list_dir(suite_dir);
    if files.is_empty() {
        crate::bail!("no scenarios in {}", suite_dir.display());
    }
    std::fs::create_dir_all(out)?;
    let started = now_ms();
    let mut verdicts: Vec<Verdict> = vec![];
    let mut incomplete: Vec<Value> = vec![];
    for f in &files {
        let sc = Scenario::load(f)?;
        for arm in arms {
            for trial in 1..=trials {
                let res = (|| -> Result<Verdict> {
                    let (run_id, dir) = harness::create_run(state_dir, &sc, *arm, choice, "scripted")?;
                    let log = harness::supervise(state_dir, &run_id, 0, true)?;
                    let mut v = evaluate_run(&dir)?;
                    v.trial = trial;
                    v.wall_ms = log.wall_ms;
                    v.peak_rss_bytes = log.peak_rss_bytes.iter().max().copied();
                    v.executor_processes = log.executor_processes;
                    v.simulated_crashes = log.simulated_crashes;
                    Ok(v)
                })();
                match res {
                    Ok(v) => {
                        eprintln!("{:<34} {:<8} trial {trial}: {:<16} {}", sc.id, arm.as_str(), v.terminal_status, if v.success { "PASS" } else { "FAIL" });
                        verdicts.push(v);
                    }
                    Err(e) => {
                        eprintln!("{:<34} {:<8} trial {trial}: INCOMPLETE ({e})", sc.id, arm.as_str());
                        incomplete.push(json!({"scenario": sc.id, "arm": arm.as_str(), "trial": trial, "error": e.msg}));
                    }
                }
            }
        }
    }
    // Raw results.
    let mut raw = String::new();
    for v in &verdicts {
        raw.push_str(&serde_json::to_string(v)?);
        raw.push('\n');
    }
    std::fs::write(out.join("raw.jsonl"), raw)?;
    std::fs::write(out.join("incomplete.json"), serde_json::to_string_pretty(&incomplete)?)?;
    let mut csv = String::from("scenario_id,family,arm,planner,trial,terminal_status,expected_outcome,expected_kits,kits_truly_complete,kits_claimed_complete,false_success_claims,inventory_correct,invariant_violations,duplicate_effects,unauthorized_attempted,unauthorized_executed,success,fault_fired,recovery_ms,human_interventions,planner_requests,input_tokens,output_tokens,dispatches,observations,wall_ms,peak_rss_bytes,executor_processes,simulated_crashes\n");
    for v in &verdicts {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            v.scenario_id, v.family, v.arm, v.planner, v.trial, v.terminal_status, v.expected_outcome, v.expected_kits,
            v.kits_truly_complete.len(), v.kits_claimed_complete.len(), v.false_success_claims, v.inventory_correct,
            v.invariant_violations.len(), v.duplicate_effects, v.unauthorized_attempted, v.unauthorized_executed, v.success,
            v.fault_fired, v.recovery_ms.map(|x| x.to_string()).unwrap_or_default(),
            v.human_interventions.values().sum::<u32>(), v.planner_requests,
            v.input_tokens.map(|x| x.to_string()).unwrap_or_default(), v.output_tokens.map(|x| x.to_string()).unwrap_or_default(),
            v.dispatches, v.observations, v.wall_ms, v.peak_rss_bytes.map(|x| x.to_string()).unwrap_or_default(),
            v.executor_processes, v.simulated_crashes
        ));
    }
    std::fs::write(out.join("results.csv"), csv)?;
    let planner_label = verdicts.first().map(|v| v.planner.clone()).unwrap_or_default();
    let summary = aggregate(&verdicts);
    let meta = json!({
        "suite": suite_dir.display().to_string(),
        "scenarios": files.len(),
        "arms": arms.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        "trials_per_scenario_arm": trials,
        "planner": planner_label,
        "live_model_calls_performed": planner_label.starts_with("live:"),
        "started_at_ms": started,
        "finished_at_ms": now_ms(),
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "completed_trials": verdicts.len(),
        "incomplete_trials": incomplete.len(),
        "revolve_version": env!("CARGO_PKG_VERSION"),
    });
    std::fs::write(out.join("summary.json"), serde_json::to_string_pretty(&json!({"meta": meta, "by_arm": summary}))?)?;
    let md = markdown_report(&meta, &summary, &verdicts, &incomplete);
    std::fs::write(out.join("report.md"), &md)?;
    Ok(md)
}

fn aggregate(vs: &[Verdict]) -> Value {
    let mut by_arm: BTreeMap<String, Vec<&Verdict>> = BTreeMap::new();
    for v in vs {
        by_arm.entry(v.arm.clone()).or_default().push(v);
    }
    let mut out = serde_json::Map::new();
    for (arm, list) in by_arm {
        let n = list.len();
        let cnt = |f: &dyn Fn(&Verdict) -> bool| list.iter().filter(|v| f(v)).count();
        let exp_completed: Vec<&&Verdict> = list.iter().filter(|v| v.fully_correct_completion.is_some()).collect();
        let exp_partial: Vec<&&Verdict> = list.iter().filter(|v| v.infeasible_handled.is_some()).collect();
        let faulted: Vec<&&Verdict> = list.iter().filter(|v| v.fault_fired).collect();
        let rec_times: Vec<i64> = faulted.iter().filter(|v| v.success).filter_map(|v| v.recovery_ms).collect();
        let mut families: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for v in &list {
            let e = families.entry(v.family.clone()).or_default();
            e.0 += 1;
            e.1 += v.success as u32;
        }
        let mut hi: BTreeMap<String, u32> = BTreeMap::new();
        for v in &list {
            for (k, c) in &v.human_interventions {
                *hi.entry(k.clone()).or_default() += c;
            }
        }
        let sum_i = |f: &dyn Fn(&Verdict) -> i64| list.iter().map(|v| f(v)).sum::<i64>();
        let tokens_in: Option<i64> = if list.iter().all(|v| v.input_tokens.is_some()) { Some(sum_i(&|v| v.input_tokens.unwrap_or(0))) } else { None };
        out.insert(
            arm,
            json!({
                "trials": n,
                "success": cnt(&|v| v.success),
                "fully_correct_completion": {"count": exp_completed.iter().filter(|v| v.fully_correct_completion == Some(true)).count(), "of": exp_completed.len()},
                "infeasible_handled": {"count": exp_partial.iter().filter(|v| v.infeasible_handled == Some(true)).count(), "of": exp_partial.len()},
                "false_success_claims": sum_i(&|v| v.false_success_claims as i64),
                "trials_with_incorrect_inventory": cnt(&|v| !v.inventory_correct),
                "duplicate_effects": sum_i(&|v| v.duplicate_effects as i64),
                "invariant_violations": sum_i(&|v| v.invariant_violations.len() as i64),
                "unauthorized_attempted": sum_i(&|v| v.unauthorized_attempted as i64),
                "unauthorized_executed": sum_i(&|v| v.unauthorized_executed as i64),
                "recovery": {"successful": faulted.iter().filter(|v| v.success).count(), "of_faulted_trials": faulted.len(),
                              "mean_ms_fault_to_final_state": if rec_times.is_empty() { Value::Null } else { json!(rec_times.iter().sum::<i64>() / rec_times.len() as i64) }},
                "human_interventions": hi,
                "planner_requests": {"total": sum_i(&|v| v.planner_requests), "mean": if n > 0 { sum_i(&|v| v.planner_requests) as f64 / n as f64 } else { 0.0 }},
                "input_tokens_total": tokens_in,
                "dispatches_total": sum_i(&|v| v.dispatches as i64),
                "mean_wall_ms": if n > 0 { sum_i(&|v| v.wall_ms) / n as i64 } else { 0 },
                "max_peak_rss_bytes": list.iter().filter_map(|v| v.peak_rss_bytes).max(),
                "by_family": families.into_iter().map(|(k, (t, s))| (k, json!({"trials": t, "success": s}))).collect::<serde_json::Map<_, _>>(),
            }),
        );
    }
    Value::Object(out)
}

fn markdown_report(meta: &Value, summary: &Value, vs: &[Verdict], incomplete: &[Value]) -> String {
    let mut s = String::new();
    let live = meta["live_model_calls_performed"].as_bool().unwrap_or(false);
    s.push_str("# Revolve benchmark results\n\n");
    s.push_str(&format!(
        "- Suite: `{}` ({} scenario files), trials per scenario/arm: {}\n- Planner: `{}`\n- Completed trials: {}; incomplete (excluded, listed below): {}\n- Host: {} / {}\n\n",
        meta["suite"].as_str().unwrap_or(""),
        meta["scenarios"],
        meta["trials_per_scenario_arm"],
        meta["planner"].as_str().unwrap_or(""),
        meta["completed_trials"],
        meta["incomplete_trials"],
        meta["host"]["os"].as_str().unwrap_or(""),
        meta["host"]["arch"].as_str().unwrap_or("")
    ));
    if !live {
        s.push_str("> **Scripted planners only.** Both arms use the same deterministic rule-based task policy. These results validate the harness, the runtime and the symmetry of the comparison. They do **not** show that Revolve outperforms a real AI agent. A live-model comparison has not been performed.\n\n");
    }
    s.push_str("| metric | ");
    let arms: Vec<String> = summary.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    s.push_str(&arms.join(" | "));
    s.push_str(" |\n|---|");
    s.push_str(&arms.iter().map(|_| "---|").collect::<String>());
    s.push('\n');
    let row = |label: &str, f: &dyn Fn(&Value) -> String| -> String {
        let cells: Vec<String> = arms.iter().map(|a| f(&summary[a])).collect();
        format!("| {label} | {} |\n", cells.join(" | "))
    };
    s.push_str(&row("trials", &|v| v["trials"].to_string()));
    s.push_str(&row("success (outcome as expected, no false claims/violations)", &|v| format!("{} / {}", v["success"], v["trials"])));
    s.push_str(&row("fully correct completion", &|v| format!("{} / {}", v["fully_correct_completion"]["count"], v["fully_correct_completion"]["of"])));
    s.push_str(&row("infeasible request handled correctly", &|v| format!("{} / {}", v["infeasible_handled"]["count"], v["infeasible_handled"]["of"])));
    s.push_str(&row("false success claims", &|v| v["false_success_claims"].to_string()));
    s.push_str(&row("trials with incorrect inventory", &|v| v["trials_with_incorrect_inventory"].to_string()));
    s.push_str(&row("duplicate effects", &|v| v["duplicate_effects"].to_string()));
    s.push_str(&row("invariant violations", &|v| v["invariant_violations"].to_string()));
    s.push_str(&row("unauthorized attempted / executed", &|v| format!("{} / {}", v["unauthorized_attempted"], v["unauthorized_executed"])));
    s.push_str(&row("recovery success (faulted trials)", &|v| format!("{} / {}", v["recovery"]["successful"], v["recovery"]["of_faulted_trials"])));
    s.push_str(&row("mean ms fault -> final state (successful)", &|v| v["recovery"]["mean_ms_fault_to_final_state"].to_string()));
    s.push_str(&row("human interventions", &|v| v["human_interventions"].to_string()));
    s.push_str(&row("planner/model requests (total)", &|v| v["planner_requests"]["total"].to_string()));
    s.push_str(&row("input tokens (total)", &|v| if v["input_tokens_total"].is_null() { "n/a (scripted)".into() } else { v["input_tokens_total"].to_string() }));
    s.push_str(&row("adapter dispatches (total)", &|v| v["dispatches_total"].to_string()));
    s.push_str(&row("mean wall ms per trial", &|v| v["mean_wall_ms"].to_string()));
    s.push_str(&row("max peak RSS of an executor process (bytes)", &|v| v["max_peak_rss_bytes"].to_string()));
    s.push_str("\n## Per scenario\n\n| scenario | family | arm | status | expected | true kits | claimed | pass | notes |\n|---|---|---|---|---|---|---|---|---|\n");
    for v in vs {
        let mut notes = v.notes.clone();
        notes.extend(v.invariant_violations.clone());
        notes.extend(v.inventory_mismatches.iter().map(|m| format!("inventory {m}")));
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} ({}) | {} | {} | {} | {} |\n",
            v.scenario_id,
            v.family,
            v.arm,
            v.terminal_status,
            v.expected_outcome,
            v.expected_kits,
            v.kits_truly_complete.len(),
            v.kits_claimed_complete.len(),
            if v.success { "yes" } else { "**no**" },
            notes.join("; ")
        ));
    }
    if !incomplete.is_empty() {
        s.push_str("\n## Incomplete trials (excluded from the table above)\n\n");
        for i in incomplete {
            s.push_str(&format!("- {} / {} trial {}: {}\n", i["scenario"], i["arm"], i["trial"], i["error"]));
        }
    }
    s.push_str("\n## Interpretation limits\n\n- Scenario counts are small; results are preliminary and do not establish general superiority.\n- The simulator is discrete and deterministic; it says nothing about physical feasibility, perception or safety.\n- `success` requires the terminal state and truly-complete kit count to match the scenario's authored expectation, zero false claims, zero duplicate effects, zero invariant violations and (except for expected-blocked runs) a correct inventory record.\n");
    s
}

