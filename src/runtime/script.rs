//! Program runs: the executor carries out a Revolve Script program one
//! outside-world call at a time (see `script/host.rs` for the replay model).
//!
//! A program was written, checked and approved before it was submitted, so a
//! program run never asks a planner. Each program is compiled again against the
//! table as it is when it starts. Every pick and place goes through the same
//! path as any plan step: fresh observation, atomic claim, gateway (policy),
//! adapter, verification, and reconciliation after unknown outcomes.
//! Questions wait for the person; expectations are checked by looking at the
//! table.

use super::*;
use crate::script::host::{ScriptHost, ScriptStep};
use crate::script::{self, CheckEnv};

pub(super) enum ScriptOutcome {
    /// The program finished.
    Finish,
    /// One call was handled; `effect` says whether something moved.
    Step(Next, bool),
}

pub(super) fn is_program(plan: &PlanRow) -> bool {
    plan.proposal.get("language").and_then(Value::as_str) == Some(script::LANGUAGE)
}

impl Executor {
    /// Take the program submitted for the current revision, compile it against the
    /// table as it is now, and make it the active plan.
    pub(super) fn activate_program(&mut self) -> Result<Episode> {
        let (rev, _) = self.store.current_goal()?;
        let plans = self.store.plans()?;
        let Some(p) = plans
            .iter()
            .rev()
            .find(|p| p.status == "received" && p.goal_revision == rev && is_program(p))
        else {
            return Ok(Episode::Rest(
                R_AWAITING_PLANNER,
                "waiting for a program for the current request".into(),
            ));
        };
        let source = p.proposal["source"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let obs = match self.observe() {
            Ok(o) => o,
            Err(e) => {
                return Ok(Episode::Rest(
                    R_BLOCKED,
                    format!("cannot look at the table: {e}"),
                ));
            }
        };
        match script::compile(&source, &CheckEnv::from(&obs, &self.policy)) {
            Err(diags) => {
                self.store.set_plan_diagnostics(
                    p.plan_seq,
                    "rejected",
                    &serde_json::to_value(&diags)?,
                )?;
                let first = diags.first().map(|d| d.to_string()).unwrap_or_default();
                self.event(
                    "plan_rejected",
                    &format!("program #{} does not pass its checks against the table as it is now ({first}); nothing ran", p.plan_seq),
                    None,
                    Some(serde_json::to_value(&diags)?),
                )?;
                Ok(Episode::Rest(
                    R_BLOCKED,
                    format!(
                        "the program does not pass its checks against the table as it is now: {first}"
                    ),
                ))
            }
            Ok(c) => {
                let prog = codegen::Program {
                    wasm: c.wasm.clone(),
                    sha256: c.sha256.clone(),
                    imports: c.imports.clone(),
                };
                self.store
                    .insert_program(p.plan_seq, script::codegen::ABI, &prog)?;
                self.event(
                    "program_built",
                    &format!(
                        "program #{} checked and compiled: {} lines -> WebAssembly ({} bytes, sha256 {}) -> native code; imports only {}",
                        p.plan_seq,
                        c.lines,
                        c.wasm_bytes,
                        &c.sha256[..12],
                        c.imports.join(", ")
                    ),
                    None,
                    Some(json!({"sha256": c.sha256, "bytes": c.wasm_bytes, "imports": c.imports, "facts": c.facts, "codegen": script::codegen::ABI})),
                )?;
                self.store.set_plan_status(p.plan_seq, "active")?;
                self.store.set_active_plan(Some(p.plan_seq))?;
                Ok(Episode::Planned)
            }
        }
    }

    fn program_wasm(&mut self, plan: &PlanRow) -> Result<std::result::Result<Vec<u8>, String>> {
        match self.store.program(plan.plan_seq)? {
            Some(p) if crate::util::sha256_hex(&p.wasm) == p.sha256 => Ok(Ok(p.wasm)),
            Some(_) => Ok(Err(format!(
                "the stored program of plan #{} fails its integrity check; not running it",
                plan.plan_seq
            ))),
            None => Ok(Err(format!(
                "plan #{} has no compiled program",
                plan.plan_seq
            ))),
        }
    }

    /// Run the program up to its next outside-world call and carry that call out.
    pub(super) fn run_script(
        &mut self,
        plan: &PlanRow,
        actions: &[ActionRow],
    ) -> Result<ScriptOutcome> {
        // Answers that arrived while the run waited complete their `ask` calls.
        let mut actions = actions.to_vec();
        let mut changed = false;
        for a in actions
            .iter()
            .filter(|a| a.capability == "human.ask" && a.status == A_PENDING)
        {
            let q = self.store.questions()?.into_iter().find(|q| {
                q.kind == "program" && q.context.get("action_id") == Some(&json!(a.action_id))
            });
            if let Some(q) = q
                && q.status == "answered"
            {
                let label = q
                    .options
                    .iter()
                    .find(|o| Some(&o.id) == q.answer.as_ref())
                    .map(|o| o.label.clone())
                    .unwrap_or_default();
                self.store.set_action_status(
                    a.action_id,
                    A_SUCCEEDED,
                    Some(&json!({"answer": label})),
                    None,
                )?;
                changed = true;
            }
        }
        if changed {
            actions = self.store.plan_actions(plan.plan_seq)?;
        }
        let wasm = match self.program_wasm(plan)? {
            Ok(w) => w,
            Err(e) => return Ok(ScriptOutcome::Step(Next::Blocked(e), false)),
        };
        if self.scripts.is_none() {
            self.scripts =
                Some(ScriptHost::new(&self.policy.grants).map_err(crate::util::Error::new)?);
        }
        let step = self
            .scripts
            .as_mut()
            .expect("script host")
            .next(&wasm, &actions);
        let rev = self.store.state()?.goal_revision;
        let blocked = |r: String| Ok(ScriptOutcome::Step(Next::Blocked(r), false));
        match step {
            ScriptStep::Done => Ok(ScriptOutcome::Finish),
            ScriptStep::NewCall {
                index,
                capability,
                args,
                line,
            } => {
                let step_id = format!("L{line}.{index}");
                let id = self.store.append_call(
                    plan.plan_seq,
                    rev,
                    index,
                    &step_id,
                    &capability,
                    &args,
                )?;
                let row = self.store.action(id)?;
                self.carry_out(&row)
            }
            ScriptStep::Resume { action_id } => {
                let row = self.store.action(action_id)?;
                self.carry_out(&row)
            }
            ScriptStep::Error { line, message } => {
                self.event(
                    "program_error",
                    &format!(
                        "program #{} stopped at line {line}: {message}",
                        plan.plan_seq
                    ),
                    None,
                    Some(json!({"line": line})),
                )?;
                blocked(format!(
                    "the program stopped at line {line}: {message}. Nothing more was done"
                ))
            }
            ScriptStep::Forbidden { line, message } => blocked(format!(
                "line {line} of the program tried something the policy forbids ({message}); it was not done"
            )),
            ScriptStep::Diverged(r) => {
                self.event(
                    "program_diverged",
                    &format!("program #{}: {r}", plan.plan_seq),
                    None,
                    None,
                )?;
                blocked(format!(
                    "the program did not retrace its own log: {r}; nothing was dispatched"
                ))
            }
            ScriptStep::Trapped(r) => blocked(format!(
                "the program stopped abnormally ({r}); nothing was dispatched"
            )),
            ScriptStep::Refused(r) => {
                blocked(format!("the program was refused before running: {r}"))
            }
        }
    }

    /// Carry out one logged call.
    fn carry_out(&mut self, row: &ActionRow) -> Result<ScriptOutcome> {
        let done = |n: Next, effect: bool| Ok(ScriptOutcome::Step(n, effect));
        match row.capability.as_str() {
            "workspace.observe" => match self.observe() {
                Ok(obs) => {
                    self.store.set_action_status(
                        row.action_id,
                        A_SUCCEEDED,
                        Some(&serde_json::to_value(&obs)?),
                        None,
                    )?;
                    done(Next::Continue, false)
                }
                Err(e) => done(
                    Next::Blocked(format!("cannot look at the table: {e}")),
                    false,
                ),
            },
            "workspace.pick" | "workspace.place" => {
                let args = if row.status == A_RETRY_PENDING {
                    // A same-key retry must repeat exactly what was claimed.
                    row.resolved_arguments
                        .clone()
                        .unwrap_or_else(|| row.arguments.clone())
                } else if row.capability == "workspace.pick" {
                    // The program names the part; where it is comes from a fresh look.
                    let id = row.arguments["object_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let obs = match self.observe() {
                        Ok(o) => o,
                        Err(e) => {
                            return done(
                                Next::Blocked(format!("cannot look before picking {id}: {e}")),
                                false,
                            );
                        }
                    };
                    let refusal = match obs.object(&id) {
                        Some(o) if o.location_id == GRIPPER => {
                            Err(format!("{id} is already in the arm"))
                        }
                        Some(o) if o.location_id != crate::sim::OUTSIDE => {
                            // A move names where the part goes: check that before picking,
                            // first by looking, then with the arm's own path planning.
                            match row.arguments["to"].as_str().and_then(|to| {
                                cannot_take(&obs, to, &o.observed_kind)
                                    .or_else(|| self.world.can_put(&id, to))
                            }) {
                                Some(why) => Err(format!("{why}; nothing was picked up")),
                                None => Ok(json!({"object_id": id, "source_id": o.location_id})),
                            }
                        }
                        _ => Err(format!("{id} is not on the table any more")),
                    };
                    match refusal {
                        Ok(args) => args,
                        Err(why) => {
                            self.store.set_action_status(
                                row.action_id,
                                A_FAILED,
                                None,
                                Some(&why),
                            )?;
                            self.event(
                                "action_rejected",
                                &format!("{}: {why}; no effect", row.step_id),
                                Some(row.action_id),
                                None,
                            )?;
                            return done(Next::Continue, false);
                        }
                    }
                } else {
                    row.arguments.clone()
                };
                let rev = self.store.state()?.goal_revision;
                match self.execute(row, args)? {
                    Next::Replan(reason) => {
                        // A move that was refused (no effect) is an ordinary result for the
                        // program (it returns False); anything else stops the run safely.
                        let now = self.store.action(row.action_id)?;
                        if now.status == A_FAILED
                            || now.status == A_DENIED
                            || self.store.state()?.goal_revision != rev
                        {
                            done(Next::Continue, true)
                        } else {
                            done(Next::Blocked(reason), false)
                        }
                    }
                    other => done(other, true),
                }
            }
            "human.ask" => {
                let q = row.arguments["question"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let options: Vec<QuestionOption> = row.arguments["options"]
                    .as_array()
                    .map(|o| {
                        o.iter()
                            .enumerate()
                            .map(|(i, v)| QuestionOption {
                                id: format!("o{}", i + 1),
                                label: v.as_str().unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let rev = self.store.state()?.goal_revision;
                self.store.ask(
                    rev,
                    "program",
                    &q,
                    &options,
                    &json!({"action_id": row.action_id}),
                )?;
                // The loop sees the open question and waits for the person.
                done(Next::Continue, false)
            }
            "human.say" => {
                let m = row.arguments["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.event("program_says", &m, Some(row.action_id), None)?;
                self.store
                    .set_action_status(row.action_id, A_SUCCEEDED, None, None)?;
                done(Next::Continue, false)
            }
            "expect" => {
                let held = row.arguments["held"].as_bool().unwrap_or(false);
                let d = row.arguments["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.event(
                    if held {
                        "expectation_held"
                    } else {
                        "expectation_failed"
                    },
                    &format!(
                        "{} {d} (checked by looking at the table)",
                        if held { "✓" } else { "✗" }
                    ),
                    Some(row.action_id),
                    None,
                )?;
                self.store.set_action_status(
                    row.action_id,
                    A_SUCCEEDED,
                    Some(&json!({"held": held})),
                    None,
                )?;
                done(Next::Continue, false)
            }
            other => done(
                Next::Blocked(format!("unknown program call {other}")),
                false,
            ),
        }
    }

    /// Report for a program run: what it checked and what it did (and what the
    /// arm refused to do, with the reason).
    pub(super) fn script_report(&mut self, status: &str, reason: &str) -> Result<Value> {
        let plans = self.store.plans()?;
        let prog = plans
            .iter()
            .rev()
            .find(|p| is_program(p) && p.status != "rejected" && p.status != "received");
        let actions = match prog {
            Some(p) => self.store.plan_actions(p.plan_seq)?,
            None => vec![],
        };
        let line = |a: &ActionRow| {
            a.step_id
                .trim_start_matches('L')
                .split('.')
                .next()
                .and_then(|l| l.parse::<u32>().ok())
        };
        let expectations: Vec<Value> = actions
            .iter()
            .filter(|a| a.capability == "expect")
            .map(|a| json!({"description": a.arguments["description"], "held": a.arguments["held"], "line": line(a)}))
            .collect();
        let count = |cap: &str, st: &str| {
            actions
                .iter()
                .filter(|a| a.capability == cap && a.status == st)
                .count()
        };
        let all_held =
            !expectations.is_empty() && expectations.iter().all(|e| e["held"] == json!(true));
        Ok(json!({
            "status": status,
            "reason": reason,
            "program": prog.map(|p| p.plan_seq),
            "expectations": expectations,
            "all_expectations_held": all_held,
            "moves": count("workspace.place", A_SUCCEEDED),
            "picks": count("workspace.pick", A_SUCCEEDED),
            "refused_moves": count("workspace.pick", A_FAILED) + count("workspace.place", A_FAILED),
            "refusals": actions
                .iter()
                .filter(|a| {
                    (a.capability == "workspace.pick" || a.capability == "workspace.place")
                        && a.status == A_FAILED
                })
                .map(|a| json!({"line": line(a), "part": a.arguments["object_id"], "to": a.arguments.get("to").or(a.arguments.get("destination_id")), "why": a.error}))
                .collect::<Vec<_>>(),
            "looks": count("workspace.observe", A_SUCCEEDED),
            "messages": actions.iter().filter(|a| a.capability == "human.say").map(|a| a.arguments["message"].clone()).collect::<Vec<_>>(),
            "claims_source": "observation",
        }))
    }

    /// Finish a program run: completed if every expectation held when it was
    /// checked (by looking at the table), partial otherwise.
    pub(super) fn finalize_script(
        &mut self,
        forced: Option<&str>,
        reason: String,
    ) -> Result<Option<String>> {
        if let Some(r) = self.reconcile_all()? {
            return self.block(&r).map(Some);
        }
        let st = self.store.state()?;
        if st.control != "run" {
            return Ok(None);
        }
        let report = self.script_report("pending", &reason)?;
        let held = report["expectations"]
            .as_array()
            .map(|e| e.iter().filter(|x| x["held"] == json!(true)).count())
            .unwrap_or(0);
        let total = report["expectations"]
            .as_array()
            .map(|e| e.len())
            .unwrap_or(0);
        let status = match forced {
            Some(s) => s,
            None if report["all_expectations_held"] == json!(true) => R_COMPLETED,
            None => R_PARTIAL,
        };
        let mut report = report;
        report["status"] = json!(status);
        if !self
            .store
            .finish_run(st.goal_revision, &["run"], status, &reason, Some(&report))?
        {
            return Ok(None);
        }
        self.event(
            "run_finished",
            &format!("run {status}: {held} of {total} expectation(s) held, checked by looking at the table; {} move(s); {reason}", report["moves"]),
            None,
            Some(report),
        )?;
        Ok(Some(status.into()))
    }

    /// In a session, a finished program leaves the run open for the next request:
    /// its report is recorded and the run waits on the same table.
    pub(super) fn session_program_done(&mut self, plan: &PlanRow) -> Result<String> {
        if let Some(r) = self.reconcile_all()? {
            return self.block(&r);
        }
        let report = self.script_report("done", &format!("program #{} finished", plan.plan_seq))?;
        let held = report["expectations"]
            .as_array()
            .map(|e| e.iter().filter(|x| x["held"] == json!(true)).count())
            .unwrap_or(0);
        let total = report["expectations"]
            .as_array()
            .map(|e| e.len())
            .unwrap_or(0);
        self.store.set_plan_status(plan.plan_seq, "finished")?;
        self.store.set_active_plan(None)?;
        self.event(
            "program_finished",
            &format!("program #{} finished: {held} of {total} check(s) held, checked by looking at the table; {} move(s)", plan.plan_seq, report["moves"]),
            None,
            Some(report),
        )?;
        self.store
            .set_status(R_AWAITING_PLANNER, Some("ready for the next request"))?;
        Ok(R_AWAITING_PLANNER.into())
    }

    pub(super) fn finish_cancel_script(&mut self) -> Result<String> {
        self.store.invalidate_pending(A_CANCELLED)?;
        if let Some(p) = self.active_plan()? {
            self.supersede_plan(p.plan_seq, "cancelled by human")?;
        }
        if let Some(reason) = self.reconcile_all()? {
            return self.block(&format!(
                "cancel requested but an in-flight outcome is unknown: {reason}"
            ));
        }
        let report = self.script_report(R_STOPPED, "stopped by human cancel")?;
        let rev = self.store.state()?.goal_revision;
        self.store.finish_run(
            rev,
            &["cancel"],
            R_STOPPED,
            "stopped by human cancel; moves that already happened remain",
            Some(&report),
        )?;
        self.event(
            "cancel_acknowledged",
            "cancel acknowledged: the program stopped; moves that already happened remain",
            None,
            Some(report),
        )?;
        Ok(R_STOPPED.into())
    }
}

/// Why a place cannot take a part of this (observed) kind, as far as a look can
/// tell; `None` if it can. Only ordinary refusals (a bin of another kind, a
/// place that is unavailable): anything else, such as a destination the policy
/// forbids, is left to the place step, where the policy stops the run. The
/// simulator checks again when the part is placed.
fn cannot_take(obs: &crate::model::Observation, to: &str, kind: &str) -> Option<String> {
    use crate::model::LocationType as T;
    let l = obs.location(to)?;
    if !matches!(l.loc_type, T::Bin | T::Tray | T::Table) {
        return None;
    }
    if !l.available {
        return Some(format!("{to} is unavailable"));
    }
    match (&l.loc_type, &l.accepts_kind) {
        (T::Bin, Some(acc)) if kind == "unknown" => Some(format!(
            "{to} only accepts {}, and this part could not be identified",
            plural(acc)
        )),
        (T::Bin, Some(acc)) if acc != kind => Some(format!("{to} only accepts {}", plural(acc))),
        _ => None,
    }
}

/// "green_block" -> "green blocks", for people.
fn plural(kind: &str) -> String {
    format!("{}s", kind.replace('_', " "))
}
