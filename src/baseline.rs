//! Direct tool-calling baseline arm.
//!
//! Shared with Revolve: run store, dispatch-intent persistence (claim before call),
//! the capability gateway (policy, argument types, evidence checks), adapters,
//! observations, human questions/corrections, cancel cleanup, budgets.
//!
//! NOT given to the baseline (Revolve's experimental layer): whole-plan
//! compilation/validation, automatic scheduling, logical-effect operation keys,
//! automatic reconciliation, runtime inventory sync and the runtime verifier.
//! The agent decides every action, including recovery (it can look up operation
//! status and explicitly retry under the same key), and writes its own final claim.

use crate::capability;
use crate::compiler::Limits;
use crate::gateway::{DispatchOutcome, Gateway, HumanHook};
use crate::inventory;
use crate::planner::{
    AgentDecision, AgentRequest, AnsweredQuestion, Planner, PlannerError, ToolRecord,
    capability_descriptions,
};
use crate::policy::Policy;
use crate::runtime::ExecOptions;
use crate::sim::{AdapterOutcome, SimWorld, StatusResult};
use crate::store::*;
use crate::util::{Result, now_ms};
use crate::verifier;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

/// The baseline needs one decision per action, so its turn budget is a multiple of the action budget.
pub fn max_agent_turns(l: &Limits) -> u32 {
    l.max_actions * 3
}

pub fn drive(
    dir: &Path,
    planner: Box<dyn Planner>,
    hook: Option<Box<dyn HumanHook>>,
    opts: ExecOptions,
) -> Result<String> {
    let _lock = OwnerLock::acquire(dir)?;
    let mut store = RunStore::open(dir)?;
    let mut world = SimWorld::open(&dir.join("world.db"))?;
    store.set_owner(Some(std::process::id()))?;
    let mut b = Baseline {
        policy: Policy::default(),
        planner,
        hook,
        opts,
    };
    let started = Instant::now();
    let r = b.run(&mut store, &mut world);
    let _ = store.add_active_ms(started.elapsed().as_millis() as i64);
    let _ = store.set_owner(None);
    r
}

struct Baseline {
    policy: Policy,
    planner: Box<dyn Planner>,
    hook: Option<Box<dyn HumanHook>>,
    opts: ExecOptions,
}

fn record_of(a: &ActionRow) -> ToolRecord {
    let outcome = if a.capability == "ops.status" {
        format!(
            "status:{}",
            a.result
                .as_ref()
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        )
    } else {
        match a.status.as_str() {
            A_SUCCEEDED => "applied".into(),
            A_DENIED => "denied".into(),
            A_OUTCOME_UNKNOWN if a.error.as_deref() == Some("timeout") => "timeout".into(),
            A_OUTCOME_UNKNOWN | A_DISPATCH_RECORDED => "unknown_after_restart".into(),
            A_FAILED
                if a.error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("unavailable")) =>
            {
                "unavailable".into()
            }
            A_FAILED if a.error.as_deref().is_some_and(|e| e.starts_with("invalid")) => {
                "invalid".into()
            }
            A_FAILED => "rejected".into(),
            other => other.to_string(),
        }
    };
    ToolRecord {
        action_id: a.action_id,
        capability: a.capability.clone(),
        arguments: a
            .resolved_arguments
            .clone()
            .unwrap_or_else(|| a.arguments.clone()),
        op_key: if a.capability == "ops.status" {
            a.arguments
                .get("op_key")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            a.op_key.clone()
        },
        outcome,
        detail: json!({"result": a.result, "error": a.error}),
    }
}

impl Baseline {
    fn history(store: &RunStore) -> Result<Vec<ToolRecord>> {
        let mut rows: Vec<ActionRow> = store
            .actions()?
            .into_iter()
            .filter(|a| a.origin == "agent")
            .collect();
        rows.sort_by_key(|a| {
            (
                a.finished_at_ms.or(a.claimed_at_ms).unwrap_or(0),
                a.action_id,
            )
        });
        Ok(rows.iter().map(record_of).collect())
    }

    fn run(&mut self, store: &mut RunStore, world: &mut SimWorld) -> Result<String> {
        let st = store.state()?;
        if is_terminal(&st.status) {
            return Ok(st.status);
        }
        // After a restart the agent learns that in-flight calls have unknown outcomes; it decides what to do.
        for a in store.unsettled_actions()? {
            if a.status == A_DISPATCH_RECORDED {
                store.set_action_status(
                    a.action_id,
                    A_OUTCOME_UNKNOWN,
                    None,
                    Some("executor restarted before the result was recorded"),
                )?;
            }
        }
        store.event(
            if st.status == R_CREATED {
                "executor_started"
            } else {
                "recovery_started"
            },
            &format!(
                "baseline agent executor pid {} (planner: {})",
                std::process::id(),
                self.planner.label()
            ),
            None,
            None,
        )?;
        if let Some(h) = self.hook.as_deref_mut() {
            crate::gateway::deliver_pending_human_actions(world, store, h)?;
        }
        // Resuming clears a pause, never a cancel.
        store.resume_from_pause()?;
        store.set_status(R_RUNNING, None)?;
        let started = Instant::now();
        loop {
            let st = store.state()?;
            if st.control == "cancel" {
                return self.cancel(store, world);
            }
            let dispatched = store
                .actions()?
                .iter()
                .filter(|a| a.op_key.is_some())
                .count() as u32;
            let budget = if dispatched >= self.opts.limits.max_actions
                || st.planner_decisions >= max_agent_turns(&self.opts.limits)
            {
                Some("action or agent-turn budget exhausted")
            } else if (st.active_ms + started.elapsed().as_millis() as i64) as u64
                > self.opts.limits.max_elapsed_ms
            {
                Some("elapsed-time budget exhausted")
            } else {
                None
            };
            if let Some(why) = budget {
                match self.finish(
                    store,
                    world,
                    st.goal_revision,
                    &["run"],
                    R_FAILED,
                    vec![],
                    why.into(),
                )? {
                    Some(s) => return Ok(s),
                    None => continue,
                }
            }
            if st.control == "pause" {
                store.set_status(R_PAUSED, Some("paused by human"))?;
                return Ok(R_PAUSED.into());
            }
            let (rev, goal) = store.current_goal()?;
            if let Some(q) = store
                .open_questions()?
                .into_iter()
                .find(|q| q.goal_revision == rev)
            {
                store.set_status(
                    R_AWAITING_HUMAN,
                    Some(&format!("question #{}: {}", q.question_id, q.prompt)),
                )?;
                return Ok(R_AWAITING_HUMAN.into());
            }
            let obs = {
                let mut gw = Gateway {
                    world,
                    policy: &self.policy,
                    hook: None,
                };
                gw.observe(store).ok()
            };
            let req = AgentRequest {
                run_id: store.run_id.clone(),
                goal_revision: rev,
                goal: goal.clone(),
                tools: capability_descriptions(true),
                observation: obs,
                classifications: store.classifications()?,
                inventory: inventory::view(store)?,
                history: Self::history(store)?,
                answered_questions: store
                    .questions()?
                    .into_iter()
                    .filter(|q| q.status == "answered")
                    .map(|q| AnsweredQuestion {
                        kind: q.kind,
                        context: q.context,
                        answer: q.answer.unwrap_or_default(),
                    })
                    .collect(),
                consumed_evidence: inventory::consumed_evidence(store)?,
                call_format: crate::ir::call_format(),
            };
            store.bump_planner_decisions()?;
            let (decision, info) = self.planner.next_action(&req);
            // Tools listed to the agent that are served without dispatch: a status lookup
            // expressed as a call, and an explicit observe (a fresh observation is already
            // attached to every turn).
            let decision = match decision {
                Ok(AgentDecision::Call {
                    capability,
                    arguments,
                    ..
                }) if capability == "ops.status" => Ok(AgentDecision::Status {
                    op_key: arguments
                        .get("op_key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                Ok(AgentDecision::Call { capability, .. }) if capability == "workspace.observe" => {
                    let note = json!({"note": "a fresh observation is attached to every turn (see `observation`)", "world_revision": req.observation.as_ref().map(|o| o.world_revision)});
                    let ids = store.materialize(
                        None,
                        "agent",
                        rev,
                        &[(
                            "agent-observe".into(),
                            "workspace.observe".into(),
                            "1".into(),
                            json!({}),
                            json!({}),
                        )],
                    )?;
                    store.set_action_status(ids[0], A_SUCCEEDED, Some(&note), None)?;
                    store.record_planner_call(
                        "baseline",
                        &self.planner.label(),
                        rev,
                        "observe",
                        info.latency_ms,
                        info.transport_attempts,
                        info.usage,
                        info.detail.as_ref(),
                    )?;
                    continue;
                }
                other => other,
            };
            let outcome = match &decision {
                Ok(AgentDecision::Call { .. }) => "call",
                Ok(AgentDecision::Status { .. }) => "status",
                Ok(AgentDecision::Ask(_)) => "ask",
                Ok(AgentDecision::Finish(_)) => "finish",
                Err(PlannerError::Unavailable { .. } | PlannerError::Timeout) => "unavailable",
                Err(PlannerError::Malformed { .. }) => "malformed",
                Err(_) => "not_configured",
            };
            store.record_planner_call(
                "baseline",
                &self.planner.label(),
                rev,
                outcome,
                info.latency_ms,
                info.transport_attempts,
                info.usage,
                info.detail.as_ref(),
            )?;
            if store.state()?.goal_revision != rev {
                store.event(
                    "stale_decision_rejected",
                    &format!("agent decision for goal r{rev} discarded: goal was revised"),
                    None,
                    None,
                )?;
                continue;
            }
            match decision {
                Err(
                    e @ (PlannerError::Unavailable { .. }
                    | PlannerError::Timeout
                    | PlannerError::NotConfigured { .. }
                    | PlannerError::BudgetExhausted { .. }),
                ) => {
                    store.event(
                        "planner_unavailable",
                        &format!("{e}; the agent cannot choose further actions"),
                        None,
                        None,
                    )?;
                    store.set_status(R_AWAITING_PLANNER, Some(&e.to_string()))?;
                    return Ok(R_AWAITING_PLANNER.into());
                }
                Err(PlannerError::Malformed { message }) => {
                    store.event(
                        "agent_malformed",
                        &format!("malformed agent decision: {message}"),
                        None,
                        None,
                    )?;
                }
                Ok(AgentDecision::Ask(c)) => {
                    // None: the goal changed meanwhile; re-evaluate instead of asking a stale question.
                    if let Some(qid) = store.ask(rev, &c.kind, &c.prompt, &c.options, &c.context)? {
                        store.set_status(
                            R_AWAITING_HUMAN,
                            Some(&format!("question #{qid}: {}", c.prompt)),
                        )?;
                        return Ok(R_AWAITING_HUMAN.into());
                    }
                }
                Ok(AgentDecision::Finish(report)) => {
                    let status = match report.status.as_str() {
                        "completed" => R_COMPLETED,
                        "partial" => R_PARTIAL,
                        "blocked" => R_BLOCKED,
                        _ => R_FAILED,
                    };
                    match self.finish(
                        store,
                        world,
                        rev,
                        &["run"],
                        status,
                        report.kits_complete,
                        report.message,
                    )? {
                        Some(s) => return Ok(s),
                        None => continue,
                    }
                }
                Ok(AgentDecision::Status { op_key }) => {
                    let cap = store
                        .actions()?
                        .into_iter()
                        .find(|a| a.op_key.as_deref() == Some(&op_key))
                        .map(|a| a.capability)
                        .unwrap_or_else(|| "workspace.pick".into());
                    let args = json!({"op_key": op_key});
                    let ids = store.materialize(
                        None,
                        "agent",
                        rev,
                        &[(
                            "agent-status".into(),
                            "ops.status".into(),
                            "1".into(),
                            args.clone(),
                            json!({}),
                        )],
                    )?;
                    let res = if let Err(d) = self.policy.authorize("ops.status", "1", &args) {
                        json!({"status": "denied", "message": d.to_string()})
                    } else {
                        let mut gw = Gateway {
                            world,
                            policy: &self.policy,
                            hook: None,
                        };
                        match gw.status(store, &cap, &op_key) {
                            StatusResult::Applied { receipt } => {
                                if let Some(orig) = store
                                    .actions()?
                                    .into_iter()
                                    .find(|a| a.op_key.as_deref() == Some(&op_key))
                                {
                                    store.set_action_status(
                                        orig.action_id,
                                        A_SUCCEEDED,
                                        Some(&receipt),
                                        None,
                                    )?;
                                }
                                json!({"status": "applied", "receipt": receipt})
                            }
                            StatusResult::Rejected { category, message } => {
                                json!({"status": "rejected", "category": category, "message": message})
                            }
                            StatusResult::NotFound => json!({"status": "not_found"}),
                            StatusResult::Unavailable { message } => {
                                json!({"status": "unavailable", "message": message})
                            }
                        }
                    };
                    store.set_action_status(ids[0], A_SUCCEEDED, Some(&res), None)?;
                }
                Ok(AgentDecision::Call {
                    capability,
                    version,
                    arguments,
                    retry_of,
                }) => {
                    let id = match retry_of {
                        Some(orig) => {
                            let row = store.action(orig)?;
                            if row.origin != "agent" || row.op_key.is_none() {
                                store.event(
                                    "agent_invalid",
                                    &format!("retry_of {orig} does not name an earlier agent call"),
                                    None,
                                    None,
                                )?;
                                continue;
                            }
                            store.set_action_status(orig, A_RETRY_PENDING, None, None)?;
                            orig
                        }
                        None => {
                            let spec = json!({"step_id": "agent-call", "capability": capability, "capability_version": version, "arguments": arguments});
                            store.materialize(
                                None,
                                "agent",
                                rev,
                                &[(
                                    "agent-call".into(),
                                    capability.clone(),
                                    version.clone(),
                                    arguments.clone(),
                                    spec,
                                )],
                            )?[0]
                        }
                    };
                    let descriptor = capability::logical_descriptor(&capability, &arguments);
                    let op_key = match store.claim(
                        id,
                        &arguments,
                        &descriptor,
                        KeyScheme::PerCall,
                        false,
                    )? {
                        ClaimResult::Claimed { op_key, .. } => op_key,
                        ClaimResult::ControlHold(_) => continue,
                        other => {
                            store.set_action_status(
                                id,
                                A_SUPERSEDED,
                                None,
                                Some(&format!("{other:?}")),
                            )?;
                            continue;
                        }
                    };
                    let outcome = {
                        let mut gw = Gateway {
                            world,
                            policy: &self.policy,
                            hook: self.hook.as_deref_mut(),
                        };
                        gw.dispatch(store, &op_key, &capability, &version, &arguments)
                    };
                    let (status, result, error): (&str, Option<Value>, Option<String>) =
                        match outcome {
                            DispatchOutcome::Adapter(AdapterOutcome::Applied {
                                receipt, ..
                            }) => (A_SUCCEEDED, Some(receipt), None),
                            DispatchOutcome::Adapter(AdapterOutcome::Rejected {
                                category,
                                message,
                            }) => (A_FAILED, None, Some(format!("{category}: {message}"))),
                            DispatchOutcome::Adapter(AdapterOutcome::Timeout) => {
                                (A_OUTCOME_UNKNOWN, None, Some("timeout".into()))
                            }
                            DispatchOutcome::Adapter(AdapterOutcome::Unavailable { message }) => {
                                (A_FAILED, None, Some(format!("unavailable: {message}")))
                            }
                            DispatchOutcome::Denied(d) => (A_DENIED, None, Some(d.to_string())),
                            DispatchOutcome::Invalid(m) => {
                                (A_FAILED, None, Some(format!("invalid: {m}")))
                            }
                        };
                    store.set_action_status(id, status, result.as_ref(), error.as_deref())?;
                    store.event(
                        "agent_call",
                        &format!(
                            "agent called {capability} {} -> {status}{}",
                            crate::util::canonical_json(&arguments),
                            error
                                .as_ref()
                                .map(|e| format!(" ({e})"))
                                .unwrap_or_default()
                        ),
                        Some(id),
                        None,
                    )?;
                }
            }
            if self.opts.step_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.opts.step_delay_ms));
            }
        }
    }

    fn cancel(&mut self, store: &mut RunStore, world: &mut SimWorld) -> Result<String> {
        // Shared runner cleanup on cancel (same as Revolve): record observed consumption, release reservations.
        if let Some(e) = crate::bookkeeping::settle_inventory(store, world, &self.policy, true)? {
            store.event("cleanup_failed", &e, None, None)?;
        }
        let rev = store.state()?.goal_revision;
        let r = self
            .finish(
                store,
                world,
                rev,
                &["cancel"],
                R_STOPPED,
                vec![],
                "stopped by human cancel".into(),
            )?
            .unwrap_or_else(|| R_STOPPED.into());
        store.event(
            "cancel_acknowledged",
            "cancel acknowledged: agent stopped; completed effects remain",
            None,
            None,
        )?;
        Ok(r)
    }

    /// Record the agent's claim as the terminal state, atomically with respect to
    /// late corrections/pauses/cancels. Returns None if one arrived (re-evaluate).
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        store: &mut RunStore,
        world: &mut SimWorld,
        expected_rev: u32,
        allowed_controls: &[&str],
        status: &str,
        claimed: Vec<String>,
        message: String,
    ) -> Result<Option<String>> {
        let (rev, goal) = store.current_goal()?;
        // Observation-based view for the UI, clearly separated from the agent's own claim.
        let obs = {
            let mut gw = Gateway {
                world,
                policy: &self.policy,
                hook: None,
            };
            gw.observe(store).ok()
        };
        let mut report = json!({
            "status": status,
            "arm": "baseline",
            "planner": self.planner.label(),
            "claims_source": "agent",
            "kits_claimed_complete": claimed,
            "message": message,
            "goal_revision": rev,
            "goal": goal.summary(),
            "kits_requested": goal.kit_quantity,
            "finished_at_ms": now_ms(),
        });
        if let Some(o) = obs {
            let v = verifier::verify(
                &goal,
                rev,
                &o,
                &store.classifications()?,
                &inventory::view(store)?,
                &inventory::consumed_evidence(store)?,
                &[],
            );
            report["observed_view_not_used_for_claims"] = serde_json::to_value(v)?;
        }
        if !store.finish_run(
            expected_rev,
            allowed_controls,
            status,
            &message,
            Some(&report),
        )? {
            store.event(
                "finalize_deferred",
                "a correction, pause or cancel arrived while finishing; re-evaluating",
                None,
                None,
            )?;
            return Ok(None);
        }
        store.event(
            "run_finished",
            &format!(
                "baseline agent finished: {status}; agent claims complete kits in [{}]; {message}",
                report["kits_claimed_complete"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            ),
            None,
            None,
        )?;
        Ok(Some(status.into()))
    }
}
