//! Revolve executor: the single authoritative owner of a run.
//!
//! Loop (each iteration re-reads persisted state; nothing is trusted from memory
//! across a crash):
//!   control (cancel/pause) -> reconcile unsettled actions -> open questions ->
//!   goal revision check -> plan (sync inventory, observe, ask planner, compile)
//!   -> next action: fresh observation, precondition check, atomic claim,
//!   dispatch through the gateway, settle (verify effects by observation).
//!
//! Recovery is the same loop: on start it takes the owner lock, then reconciles
//! every action whose outcome was not established (status lookup first, then
//! observation) before anything new is dispatched.

use crate::capability::{self, Completion, EffectClass};
use crate::compiler::{self, CompileContext, DiagCategory, Diagnostic, Limits};
use crate::gateway::{DispatchOutcome, Gateway, HumanHook};
use crate::inventory;
use crate::ir::{ArgValue, Condition, KitAssignment, Step};
use crate::model::{Classification, GRIPPER, Observation, effective_kind};
use crate::planner::{
    AnsweredQuestion, ExecutionSummary, Planner, PlannerError, PlannerResponse, PlanningRequest,
    capability_descriptions,
};
use crate::policy::Policy;
use crate::sim::{AdapterOutcome, SimWorld, StatusResult};
use crate::store::*;
use crate::util::Result;
use crate::verifier;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub step_delay_ms: u64,
    pub limits: Limits,
    pub quiet: bool,
}

enum Next {
    Continue,
    Replan(String),
    Blocked(String),
    /// A control request (pause/cancel) prevented the claim.
    Hold,
}

enum Recon {
    Settled,
    Unresolved(String),
}

enum Episode {
    Planned,
    Rest(&'static str, String),
    Finish(Option<&'static str>, String),
    Restart,
}

enum Tri {
    Yes,
    No(String),
    Unknown(String),
}

pub struct Executor {
    pub store: RunStore,
    world: SimWorld,
    policy: Policy,
    planner: Box<dyn Planner>,
    hook: Option<Box<dyn HumanHook>>,
    opts: ExecOptions,
}

/// Run the executor for a run directory until it reaches a terminal or resting state.
pub fn drive(
    dir: &Path,
    planner: Box<dyn Planner>,
    hook: Option<Box<dyn HumanHook>>,
    opts: ExecOptions,
) -> Result<String> {
    let _lock = OwnerLock::acquire(dir)?;
    let store = RunStore::open(dir)?;
    let world = SimWorld::open(&dir.join("world.db"))?;
    store.set_owner(Some(std::process::id()))?;
    let mut ex = Executor {
        store,
        world,
        policy: Policy::default(),
        planner,
        hook,
        opts,
    };
    let started = Instant::now();
    let r = ex.run();
    let _ = ex.store.add_active_ms(started.elapsed().as_millis() as i64);
    let _ = ex.store.set_owner(None);
    if let Err(e) = &r {
        let _ = ex.store.event(
            "executor_error",
            &format!("executor error: {e}"),
            None,
            None,
        );
    }
    r
}

fn holds_in(obs: &Observation, classes: &[Classification], c: &Condition, rev: u32) -> Tri {
    let hidden = |l: &str| obs.is_withheld(l);
    match c {
        Condition::ObjectAt {
            object_id,
            location_id,
        } => {
            if hidden(location_id) {
                return Tri::Unknown(format!("{location_id} is not observable"));
            }
            match obs.object(object_id) {
                Some(o) if &o.location_id == location_id => Tri::Yes,
                Some(o) => Tri::No(format!("{object_id} is at {}", o.location_id)),
                None if !obs.withheld.is_empty() => {
                    Tri::Unknown(format!("{object_id} is not visible (evidence withheld)"))
                }
                None => Tri::No(format!("{object_id} is not in the workspace")),
            }
        }
        Condition::GripperEmpty => {
            if hidden(GRIPPER) {
                return Tri::Unknown("gripper is not observable".into());
            }
            match obs.gripper_contents() {
                None => Tri::Yes,
                Some(o) => Tri::No(format!("gripper holds {}", o.object_id)),
            }
        }
        Condition::GripperHolds { object_id } => {
            if hidden(GRIPPER) {
                return Tri::Unknown("gripper is not observable".into());
            }
            match obs.gripper_contents() {
                Some(o) if &o.object_id == object_id => Tri::Yes,
                Some(o) => Tri::No(format!("gripper holds {}", o.object_id)),
                None => Tri::No("gripper is empty".into()),
            }
        }
        Condition::LocationAvailable { location_id } => match obs.location(location_id) {
            Some(l) if l.available => Tri::Yes,
            Some(_) => Tri::No(format!("{location_id} is unavailable")),
            None => Tri::No(format!("{location_id} is not a known location")),
        },
        Condition::QuantityAtLeast {
            location_id,
            part_kind,
            quantity,
        } => {
            if hidden(location_id) {
                return Tri::Unknown(format!("{location_id} is not observable"));
            }
            let n = obs
                .objects_at(location_id)
                .filter(|o| effective_kind(o, classes) == *part_kind)
                .count() as u32;
            if n >= *quantity {
                Tri::Yes
            } else {
                Tri::No(format!("{location_id} has {n} {part_kind}"))
            }
        }
        Condition::GoalRevisionEquals { revision } => {
            if *revision == rev {
                Tri::Yes
            } else {
                Tri::No(format!("goal revision is now {rev}"))
            }
        }
    }
}

/// Postconditions implied by a contract (in addition to any the plan declares).
fn implied_postconditions(capability: &str, args: &Value) -> Vec<Condition> {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match capability {
        "workspace.pick" => vec![Condition::GripperHolds {
            object_id: s("object_id"),
        }],
        "workspace.place" => vec![Condition::ObjectAt {
            object_id: s("object_id"),
            location_id: s("destination_id"),
        }],
        _ => vec![],
    }
}

fn all_hold(obs: &Observation, classes: &[Classification], conds: &[Condition], rev: u32) -> Tri {
    for c in conds {
        match holds_in(obs, classes, c, rev) {
            Tri::Yes => {}
            other => return other,
        }
    }
    Tri::Yes
}

impl Executor {
    fn say(&self, msg: &str) {
        if !self.opts.quiet {
            eprintln!("[revolve] {msg}");
        }
    }

    fn event(&self, kind: &str, msg: &str, action: Option<i64>, data: Option<Value>) -> Result<()> {
        self.say(msg);
        self.store.event(kind, msg, action, data)?;
        Ok(())
    }

    fn observe(&mut self) -> std::result::Result<Observation, String> {
        let mut gw = Gateway {
            world: &mut self.world,
            policy: &self.policy,
            hook: None,
        };
        gw.observe(&self.store)
    }

    fn status_of(&mut self, capability: &str, op_key: &str) -> StatusResult {
        let mut gw = Gateway {
            world: &mut self.world,
            policy: &self.policy,
            hook: None,
        };
        gw.status(&self.store, capability, op_key)
    }

    fn dispatch(&mut self, op_key: &str, cap: &str, ver: &str, args: &Value) -> DispatchOutcome {
        let mut gw = Gateway {
            world: &mut self.world,
            policy: &self.policy,
            hook: self.hook.as_deref_mut(),
        };
        gw.dispatch(&mut self.store, op_key, cap, ver, args)
    }

    fn classes(&self) -> Vec<Classification> {
        self.store.classifications().unwrap_or_default()
    }

    // ------------------------------------------------------------------ main loop

    pub fn run(&mut self) -> Result<String> {
        let st = self.store.state()?;
        if is_terminal(&st.status) {
            self.say(&format!("run already finished: {}", st.status));
            return Ok(st.status);
        }
        let meta = self.store.meta()?;
        if st.status == R_CREATED {
            self.event(
                "executor_started",
                &format!(
                    "executor pid {} started run (planner: {})",
                    std::process::id(),
                    self.planner.label()
                ),
                None,
                None,
            )?;
        } else {
            let unsettled = self.store.unsettled_actions()?.len();
            self.event(
                "recovery_started",
                &format!("executor pid {} took ownership from persisted state `{}`; {unsettled} action(s) with unestablished outcome", std::process::id(), st.status),
                None,
                None,
            )?;
        }
        if st.control == "pause" {
            self.store.request_control("run")?;
        }
        self.store.set_status(R_RUNNING, None)?;
        let _ = meta;
        let run_started = Instant::now();
        let prior_active = st.active_ms;
        loop {
            let st = self.store.state()?;
            // Budgets (runtime-owned; plans can only lower them).
            let dispatched = self
                .store
                .actions()?
                .iter()
                .filter(|a| a.op_key.is_some())
                .count() as u32;
            if dispatched >= self.opts.limits.max_actions {
                return self.finalize(
                    Some(R_FAILED),
                    format!("action budget exhausted ({dispatched} dispatches)"),
                );
            }
            if (prior_active + run_started.elapsed().as_millis() as i64) as u64
                > self.opts.limits.max_elapsed_ms
            {
                return self.finalize(Some(R_FAILED), "elapsed-time budget exhausted".into());
            }
            if st.control == "cancel" {
                return self.finish_cancel();
            }
            // Establish the outcome of anything in flight before doing anything else.
            if let Some(reason) = self.reconcile_all()? {
                self.store.set_status(R_BLOCKED, Some(&reason))?;
                self.event("blocked", &format!("blocked: {reason}"), None, None)?;
                return Ok(R_BLOCKED.into());
            }
            if st.control == "pause" {
                self.store.set_status(
                    R_PAUSED,
                    Some("paused by human; no new actions will be dispatched"),
                )?;
                return Ok(R_PAUSED.into());
            }
            let (rev, _goal) = self.store.current_goal()?;
            if let Some(q) = self
                .store
                .open_questions()?
                .into_iter()
                .find(|q| q.goal_revision == rev)
            {
                self.store.set_status(
                    R_AWAITING_HUMAN,
                    Some(&format!("question #{}: {}", q.question_id, q.prompt)),
                )?;
                return Ok(R_AWAITING_HUMAN.into());
            }
            // Goal revision: stop using the old plan; completed effects stay in the world.
            let active = self.active_plan()?;
            if let Some(p) = &active
                && p.goal_revision != rev
            {
                self.supersede_plan(p.plan_seq, &format!("goal revised to r{rev}; pending steps of plan #{} (r{}) invalidated, completed effects preserved", p.plan_seq, p.goal_revision))?;
                continue;
            }
            let Some(plan) = active else {
                match self.plan_episode()? {
                    Episode::Planned | Episode::Restart => continue,
                    Episode::Rest(status, reason) => {
                        self.store.set_status(status, Some(&reason))?;
                        return Ok(status.into());
                    }
                    Episode::Finish(forced, reason) => return self.finalize(forced, reason),
                }
            };
            let actions = self.store.plan_actions(plan.plan_seq)?;
            let Some(next) = actions
                .iter()
                .find(|a| a.status == A_PENDING || a.status == A_RETRY_PENDING)
                .cloned()
            else {
                self.store.set_plan_status(plan.plan_seq, "finished")?;
                return self.finalize(None, format!("plan #{} finished", plan.plan_seq));
            };
            match self.execute(&next, &actions)? {
                Next::Continue => {}
                Next::Hold => continue,
                Next::Replan(reason) => self.supersede_plan(plan.plan_seq, &reason)?,
                Next::Blocked(reason) => {
                    self.store.set_status(R_BLOCKED, Some(&reason))?;
                    self.event("blocked", &format!("blocked: {reason}"), None, None)?;
                    return Ok(R_BLOCKED.into());
                }
            }
            if self.opts.step_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.opts.step_delay_ms));
            }
        }
    }

    fn active_plan(&self) -> Result<Option<PlanRow>> {
        let st = self.store.state()?;
        let Some(seq) = st.active_plan_seq else {
            return Ok(None);
        };
        Ok(self
            .store
            .plans()?
            .into_iter()
            .find(|p| p.plan_seq == seq && p.status == "active"))
    }

    fn supersede_plan(&mut self, plan_seq: i64, reason: &str) -> Result<()> {
        let mut n = 0;
        for a in self.store.plan_actions(plan_seq)? {
            if a.status == A_PENDING {
                self.store
                    .set_action_status(a.action_id, A_SUPERSEDED, None, Some(reason))?;
                n += 1;
            } else if a.status == A_RETRY_PENDING {
                // No effect was established for this key; v0 adapters are synchronous, so the key is retired.
                self.store.set_action_status(
                    a.action_id,
                    A_FAILED,
                    None,
                    Some("no effect established; superseded before retry"),
                )?;
                n += 1;
            }
        }
        self.store.set_plan_status(plan_seq, "superseded")?;
        self.store.set_active_plan(None)?;
        self.event(
            "plan_superseded",
            &format!(
                "plan #{plan_seq} superseded ({n} undispatched step(s) invalidated): {reason}"
            ),
            None,
            None,
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------ reconciliation

    fn reconcile_all(&mut self) -> Result<Option<String>> {
        let mut unresolved = vec![];
        for a in self.store.unsettled_actions()? {
            if a.status == A_RETRY_PENDING {
                continue;
            }
            if let Recon::Unresolved(r) = self.reconcile_one(&a)? {
                unresolved.push(r);
            }
        }
        // A failure established during reconciliation invalidates the rest of its plan.
        if let Some(p) = self.active_plan()?
            && self
                .store
                .plan_actions(p.plan_seq)?
                .iter()
                .any(|a| a.status == A_FAILED || a.status == A_DENIED)
        {
            self.supersede_plan(p.plan_seq, "a step of this plan is known to have failed")?;
        }
        Ok(if unresolved.is_empty() {
            None
        } else {
            Some(unresolved.join("; "))
        })
    }

    fn reconcile_one(&mut self, a: &ActionRow) -> Result<Recon> {
        let Some(contract) = capability::lookup(&a.capability) else {
            return Ok(Recon::Unresolved(format!(
                "action {} uses unknown capability {}",
                a.action_id, a.capability
            )));
        };
        let op_key = a.op_key.clone().unwrap_or_default();
        let args = a.resolved_arguments.clone().unwrap_or(Value::Null);
        let step: Option<Step> = serde_json::from_value(a.step_spec.clone()).ok();
        let max_attempts = step
            .as_ref()
            .map(|s| s.retry.max_attempts)
            .unwrap_or(1)
            .clamp(1, 3) as i64;
        let block_on_unknown = step
            .as_ref()
            .is_some_and(|s| s.on_unknown_outcome == crate::ir::UnknownOutcomePolicy::Block);
        let label = format!(
            "action {} ({} {}, key {op_key})",
            a.action_id,
            a.capability,
            crate::util::canonical_json(&args)
        );
        let mut posts = implied_postconditions(&a.capability, &args);
        if let Some(s) = &step {
            posts.extend(s.postconditions.iter().cloned());
        }
        let rev = self.store.state()?.goal_revision;

        let status = if contract.status_lookup {
            self.status_of(&a.capability, &op_key)
        } else {
            StatusResult::Unavailable {
                message: "no status lookup".into(),
            }
        };
        match status {
            StatusResult::Applied { receipt } => {
                if contract.completion == Completion::AcknowledgedThenObserve {
                    match self.observe() {
                        Err(e) => {
                            self.store.set_action_status(
                                a.action_id,
                                A_ACKNOWLEDGED,
                                Some(&receipt),
                                None,
                            )?;
                            return Ok(Recon::Unresolved(format!(
                                "{label}: receipt says applied but observation failed: {e}"
                            )));
                        }
                        Ok(obs) => match all_hold(&obs, &self.classes(), &posts, rev) {
                            Tri::Yes => {
                                self.store.set_action_status(
                                    a.action_id,
                                    A_SUCCEEDED,
                                    Some(&receipt),
                                    None,
                                )?;
                                self.event("reconciled", &format!("reconciled {label}: effect receipt found and confirmed by observation; not repeated"), Some(a.action_id), Some(receipt))?;
                                return Ok(Recon::Settled);
                            }
                            Tri::Unknown(why) => {
                                self.store.set_action_status(
                                    a.action_id,
                                    A_ACKNOWLEDGED,
                                    Some(&receipt),
                                    None,
                                )?;
                                return Ok(Recon::Unresolved(format!(
                                    "{label}: receipt says applied, observation cannot confirm ({why})"
                                )));
                            }
                            Tri::No(why) => {
                                self.store.set_action_status(
                                    a.action_id,
                                    A_OUTCOME_UNKNOWN,
                                    Some(&receipt),
                                    Some(&why),
                                )?;
                                return Ok(Recon::Unresolved(format!(
                                    "{label}: receipt says applied but observation disagrees ({why}); needs inspection"
                                )));
                            }
                        },
                    }
                }
                self.store
                    .set_action_status(a.action_id, A_SUCCEEDED, Some(&receipt), None)?;
                self.event(
                    "reconciled",
                    &format!("reconciled {label}: applied (status lookup); not repeated"),
                    Some(a.action_id),
                    Some(receipt),
                )?;
                Ok(Recon::Settled)
            }
            StatusResult::Rejected { category, message } => {
                self.store.set_action_status(
                    a.action_id,
                    A_FAILED,
                    None,
                    Some(&format!("{category}: {message}")),
                )?;
                self.event(
                    "reconciled",
                    &format!("reconciled {label}: rejected by adapter, no effect ({message})"),
                    Some(a.action_id),
                    None,
                )?;
                Ok(Recon::Settled)
            }
            StatusResult::NotFound => {
                if block_on_unknown {
                    self.store.set_action_status(
                        a.action_id,
                        A_OUTCOME_UNKNOWN,
                        None,
                        Some("no receipt; step policy is block"),
                    )?;
                    return Ok(Recon::Unresolved(format!(
                        "{label}: no receipt, and the step requires a human decision before any retry"
                    )));
                }
                if contract.safe_to_redispatch_same_key() && a.attempts < max_attempts {
                    self.store.set_action_status(
                        a.action_id,
                        A_RETRY_PENDING,
                        None,
                        Some("no receipt under this key; will re-dispatch with the same key"),
                    )?;
                    self.event("reconciled", &format!("reconciled {label}: no receipt, so no effect under this key; will retry with the same key"), Some(a.action_id), None)?;
                } else {
                    self.store.set_action_status(
                        a.action_id,
                        A_FAILED,
                        None,
                        Some("no receipt; retry budget exhausted"),
                    )?;
                    self.event(
                        "reconciled",
                        &format!("reconciled {label}: no effect; retry budget exhausted"),
                        Some(a.action_id),
                        None,
                    )?;
                }
                Ok(Recon::Settled)
            }
            StatusResult::Unavailable { message } => {
                if contract.effect_class == EffectClass::PhysicalUncertain {
                    // Fall back to observation.
                    let obs = match self.observe() {
                        Ok(o) => o,
                        Err(e) => {
                            return Ok(Recon::Unresolved(format!(
                                "{label}: status unavailable ({message}) and observation failed ({e})"
                            )));
                        }
                    };
                    match all_hold(&obs, &self.classes(), &posts, rev) {
                        Tri::Yes => {
                            self.store.set_action_status(a.action_id, A_SUCCEEDED, Some(&json!({"evidence": "observation", "world_revision": obs.world_revision})), None)?;
                            self.event("reconciled", &format!("reconciled {label}: status unavailable, effect confirmed by observation; not repeated"), Some(a.action_id), None)?;
                            return Ok(Recon::Settled);
                        }
                        Tri::No(_)
                            if contract.safe_to_redispatch_same_key()
                                && !block_on_unknown
                                && a.attempts < max_attempts =>
                        {
                            self.store.set_action_status(a.action_id, A_RETRY_PENDING, None, Some("effect not observed; same-key retry is safe (idempotent adapter)"))?;
                            self.event("reconciled", &format!("reconciled {label}: status unavailable, effect not observed; same-key retry is safe"), Some(a.action_id), None)?;
                            return Ok(Recon::Settled);
                        }
                        Tri::No(why) | Tri::Unknown(why) => {
                            self.store.set_action_status(
                                a.action_id,
                                A_OUTCOME_UNKNOWN,
                                None,
                                Some(&why),
                            )?;
                            return Ok(Recon::Unresolved(format!(
                                "{label}: outcome unknown — status unavailable ({message}) and observation insufficient ({why})"
                            )));
                        }
                    }
                }
                self.store.set_action_status(
                    a.action_id,
                    A_OUTCOME_UNKNOWN,
                    None,
                    Some(&message),
                )?;
                Ok(Recon::Unresolved(format!(
                    "{label}: status unavailable: {message}"
                )))
            }
        }
    }

    // ------------------------------------------------------------------ bookkeeping

    fn sync_inventory(&mut self, obs: &Observation) -> Result<Option<String>> {
        crate::bookkeeping::sync_inventory(
            &mut self.store,
            &mut self.world,
            &self.policy,
            obs,
            false,
        )
    }

    // ------------------------------------------------------------------ planning

    fn summary(&self) -> Result<ExecutionSummary> {
        let actions = self.store.actions()?;
        let previous = self
            .store
            .plans()?
            .into_iter()
            .rev()
            .find(|p| matches!(p.status.as_str(), "active" | "finished" | "superseded"))
            .and_then(|p| {
                serde_json::from_value::<Vec<KitAssignment>>(p.proposal["kit_assignments"].clone())
                    .ok()
            })
            .unwrap_or_default();
        let answered = self
            .store
            .questions()?
            .into_iter()
            .filter(|q| q.status == "answered")
            .map(|q| AnsweredQuestion {
                kind: q.kind,
                context: q.context,
                answer: q.answer.unwrap_or_default(),
            })
            .collect();
        Ok(ExecutionSummary {
            consumed_evidence: inventory::consumed_evidence(&self.store)?,
            previous_kit_assignments: previous,
            answered_questions: answered,
            actions_succeeded: actions.iter().filter(|a| a.status == A_SUCCEEDED).count() as u32,
            actions_failed: actions
                .iter()
                .filter(|a| a.status == A_FAILED || a.status == A_DENIED)
                .count() as u32,
            notes: vec![],
        })
    }

    fn plan_episode(&mut self) -> Result<Episode> {
        let obs = match self.observe() {
            Ok(o) => o,
            Err(e) => {
                return Ok(Episode::Rest(
                    R_BLOCKED,
                    format!("cannot observe the workspace: {e}"),
                ));
            }
        };
        if let Some(reason) = self.sync_inventory(&obs)? {
            return Ok(Episode::Rest(R_BLOCKED, reason));
        }
        // Re-observe after bookkeeping so the plan's basis is current.
        let obs = match self.observe() {
            Ok(o) => o,
            Err(e) => {
                return Ok(Episode::Rest(
                    R_BLOCKED,
                    format!("cannot observe the workspace: {e}"),
                ));
            }
        };
        let (rev, goal) = self.store.current_goal()?;
        let goal_id = self.store.state()?.goal_id;
        let mut feedback: Vec<Diagnostic> = vec![];
        for _attempt in 0..=self.opts.limits.max_repairs_per_episode {
            let st = self.store.state()?;
            if st.planner_decisions >= self.opts.limits.max_planner_decisions {
                return Ok(Episode::Finish(
                    Some(R_FAILED),
                    format!(
                        "planner decision budget exhausted ({} decisions)",
                        st.planner_decisions
                    ),
                ));
            }
            let classes = self.classes();
            let inv = inventory::view(&self.store)?;
            let req = PlanningRequest {
                run_id: self.store.run_id.clone(),
                goal_id: goal_id.clone(),
                goal_revision: rev,
                goal: goal.clone(),
                capabilities: capability_descriptions(false),
                observation: obs.clone(),
                classifications: classes.clone(),
                inventory: inv.clone(),
                summary: self.summary()?,
                feedback: feedback.clone(),
                limits: self.opts.limits,
            };
            self.store.bump_planner_decisions()?;
            let (resp, info) = self.planner.plan(&req);
            let outcome = match &resp {
                Ok(PlannerResponse::Plan(_)) => "plan",
                Ok(PlannerResponse::Clarify(_)) => "clarify",
                Ok(PlannerResponse::Unable { .. }) => "unable",
                Err(PlannerError::Unavailable { .. }) => "unavailable",
                Err(PlannerError::Timeout) => "timeout",
                Err(PlannerError::Malformed { .. }) => "malformed",
                Err(PlannerError::NotConfigured { .. }) => "not_configured",
                Err(PlannerError::BudgetExhausted { .. }) => "budget_exhausted",
            };
            self.store.record_planner_call(
                "revolve",
                &self.planner.label(),
                rev,
                outcome,
                info.latency_ms,
                info.transport_attempts,
                info.usage,
                info.detail.as_ref(),
            )?;
            // A correction committed while the planner was thinking makes this response stale.
            let now_rev = self.store.state()?.goal_revision;
            if now_rev != rev {
                if let Ok(PlannerResponse::Plan(p)) = &resp {
                    self.store.insert_plan(
                        rev,
                        Some(obs.world_revision),
                        p,
                        &self.planner.label(),
                        "stale",
                        &json!([]),
                    )?;
                }
                self.event(
                    "stale_plan_rejected",
                    &format!("planner response for goal r{rev} discarded: goal is now r{now_rev}"),
                    None,
                    None,
                )?;
                return Ok(Episode::Restart);
            }
            match resp {
                Err(e @ (PlannerError::Unavailable { .. } | PlannerError::Timeout)) => {
                    self.event("planner_unavailable", &format!("{e}; validated work is complete or none is pending, so the run waits for the planner"), None, None)?;
                    return Ok(Episode::Rest(R_AWAITING_PLANNER, e.to_string()));
                }
                Err(
                    e @ (PlannerError::NotConfigured { .. } | PlannerError::BudgetExhausted { .. }),
                ) => {
                    self.event("planner_unavailable", &e.to_string(), None, None)?;
                    return Ok(Episode::Rest(R_AWAITING_PLANNER, e.to_string()));
                }
                Err(PlannerError::Malformed { message }) => {
                    self.store.insert_plan(
                        rev,
                        Some(obs.world_revision),
                        &json!({"malformed": message}),
                        &self.planner.label(),
                        "rejected",
                        &json!([]),
                    )?;
                    feedback = vec![Diagnostic {
                        category: DiagCategory::Syntax,
                        step_id: None,
                        path: "$".into(),
                        message,
                    }];
                    self.event(
                        "plan_rejected",
                        "planner response malformed; asking again with feedback",
                        None,
                        None,
                    )?;
                    continue;
                }
                Ok(PlannerResponse::Unable { reason }) => {
                    self.event(
                        "planner_unable",
                        &format!("planner cannot proceed: {reason}"),
                        None,
                        None,
                    )?;
                    // After a rejected proposal, "unable" means no valid plan exists for this episode.
                    let forced = if feedback.is_empty() {
                        None
                    } else {
                        Some(R_FAILED)
                    };
                    return Ok(Episode::Finish(forced, format!("planner unable: {reason}")));
                }
                Ok(PlannerResponse::Clarify(c)) => {
                    if c.options.is_empty()
                        || !matches!(
                            c.kind.as_str(),
                            "classification" | "substitution" | "rework" | "other"
                        )
                    {
                        feedback = vec![Diagnostic {
                            category: DiagCategory::Syntax,
                            step_id: None,
                            path: "$".into(),
                            message: "clarification needs a known kind and options".into(),
                        }];
                        continue;
                    }
                    let qid = self
                        .store
                        .ask(rev, &c.kind, &c.prompt, &c.options, &c.context)?;
                    return Ok(Episode::Rest(
                        R_AWAITING_HUMAN,
                        format!("question #{qid}: {}", c.prompt),
                    ));
                }
                Ok(PlannerResponse::Plan(raw)) => {
                    let parsed = compiler::parse_proposal(&raw);
                    let compiled = parsed.and_then(|p| {
                        let consumed =
                            inventory::consumed_evidence(&self.store).unwrap_or_default();
                        let ctx = CompileContext {
                            goal_id: &goal_id,
                            goal_revision: rev,
                            goal: &goal,
                            observation: &obs,
                            classifications: &classes,
                            inventory: &inv,
                            consumed_evidence: &consumed,
                            policy: &self.policy,
                            limits: self.opts.limits,
                        };
                        compiler::compile(&p, &ctx)
                    });
                    match compiled {
                        Err(diags) => {
                            let cats: Vec<String> = {
                                let mut c: Vec<DiagCategory> =
                                    diags.iter().map(|d| d.category).collect();
                                c.sort();
                                c.dedup();
                                c.iter().map(|c| format!("{c:?}")).collect()
                            };
                            self.store.insert_plan(
                                rev,
                                Some(obs.world_revision),
                                &raw,
                                &self.planner.label(),
                                "rejected",
                                &serde_json::to_value(&diags)?,
                            )?;
                            let first = diags.first().map(|d| d.to_string()).unwrap_or_default();
                            self.event(
                                "plan_rejected",
                                &format!("plan rejected before any effect ({} diagnostic(s): {}); first: {first}", diags.len(), cats.join(", ")),
                                None,
                                Some(serde_json::to_value(&diags)?),
                            )?;
                            feedback = diags;
                        }
                        Ok(cp) => {
                            let seq = self.store.insert_plan(
                                rev,
                                Some(obs.world_revision),
                                &raw,
                                &self.planner.label(),
                                "active",
                                &json!([]),
                            )?;
                            let steps: Vec<(String, String, String, Value, Value)> = cp
                                .steps
                                .iter()
                                .map(|s| {
                                    let args =
                                        serde_json::to_value(&s.arguments).unwrap_or(Value::Null);
                                    (
                                        s.step_id.clone(),
                                        s.capability.clone(),
                                        s.capability_version.clone(),
                                        args,
                                        serde_json::to_value(s).unwrap_or(Value::Null),
                                    )
                                })
                                .collect();
                            self.store.materialize(Some(seq), "plan", rev, &steps)?;
                            self.store.set_active_plan(Some(seq))?;
                            self.event(
                                "plan_accepted",
                                &format!(
                                    "plan #{seq} validated and authorised: {} step(s), predicts complete kits in [{}]. {}",
                                    cp.steps.len(),
                                    cp.predicted_complete_trays.join(", "),
                                    cp.header.explanation
                                ),
                                None,
                                None,
                            )?;
                            return Ok(Episode::Planned);
                        }
                    }
                }
            }
        }
        Ok(Episode::Finish(
            Some(R_FAILED),
            format!(
                "planner did not produce a valid plan after {} attempt(s)",
                self.opts.limits.max_repairs_per_episode + 1
            ),
        ))
    }

    // ------------------------------------------------------------------ execution

    fn resolve_args(
        &self,
        a: &ActionRow,
        plan_actions: &[ActionRow],
    ) -> std::result::Result<Value, String> {
        let mut out = serde_json::Map::new();
        let args = a.arguments.as_object().cloned().unwrap_or_default();
        for (k, v) in args {
            match ArgValue::parse(&v) {
                Ok(ArgValue::Ref { step_id, output }) => {
                    let src = plan_actions
                        .iter()
                        .find(|x| x.step_id == step_id)
                        .ok_or(format!("reference to unknown step {step_id}"))?;
                    let val = match src.status.as_str() {
                        A_SUCCEEDED => src
                            .result
                            .as_ref()
                            .and_then(|r| r.get(&output))
                            .cloned()
                            .or_else(|| {
                                src.resolved_arguments
                                    .as_ref()
                                    .and_then(|r| r.get(&output))
                                    .cloned()
                            }),
                        A_SATISFIED => src.arguments.get(&output).cloned(),
                        _ => None,
                    };
                    out.insert(
                        k,
                        val.ok_or(format!(
                            "output {step_id}.{output} is not available (step status {})",
                            src.status
                        ))?,
                    );
                }
                _ => {
                    out.insert(k, v);
                }
            }
        }
        Ok(Value::Object(out))
    }

    fn execute(&mut self, a: &ActionRow, plan_actions: &[ActionRow]) -> Result<Next> {
        let contract = capability::lookup(&a.capability).ok_or_else(|| {
            crate::util::Error::new(format!("unknown capability {}", a.capability))
        })?;
        let step: Step = serde_json::from_value(a.step_spec.clone())?;
        let args = match self.resolve_args(a, plan_actions) {
            Ok(v) => v,
            Err(e) => {
                self.store
                    .set_action_status(a.action_id, A_FAILED, None, Some(&e))?;
                return Ok(Next::Replan(e));
            }
        };
        let rev = self.store.state()?.goal_revision;
        // Fresh observation for precondition checks (unless this is a same-key retry).
        if !step.preconditions.is_empty() && a.status == A_PENDING {
            let obs = match self.observe() {
                Ok(o) => o,
                Err(e) => {
                    return Ok(Next::Blocked(format!(
                        "cannot observe before {}: {e}",
                        step.step_id
                    )));
                }
            };
            let classes = self.classes();
            match all_hold(&obs, &classes, &step.preconditions, rev) {
                Tri::Yes => {}
                Tri::Unknown(why) => {
                    return Ok(Next::Blocked(format!(
                        "cannot check preconditions of {}: {why}",
                        step.step_id
                    )));
                }
                Tri::No(why) => {
                    let mut posts = implied_postconditions(&a.capability, &args);
                    posts.extend(step.postconditions.iter().cloned());
                    if contract.effect_class == EffectClass::PhysicalUncertain
                        && matches!(all_hold(&obs, &classes, &posts, rev), Tri::Yes)
                    {
                        self.store.set_action_status(
                            a.action_id,
                            A_SATISFIED,
                            None,
                            Some(&format!("postconditions already hold ({why})")),
                        )?;
                        self.event(
                            "step_satisfied",
                            &format!("{} not dispatched: its effect already holds", step.step_id),
                            Some(a.action_id),
                            None,
                        )?;
                        return Ok(Next::Continue);
                    }
                    self.store.set_action_status(
                        a.action_id,
                        A_FAILED,
                        None,
                        Some(&format!("precondition failed before dispatch: {why}")),
                    )?;
                    self.event("precondition_failed", &format!("{}: precondition failed before dispatch ({why}); no effect; replanning", step.step_id), Some(a.action_id), None)?;
                    return Ok(Next::Replan(format!(
                        "precondition of {} failed: {why}",
                        step.step_id
                    )));
                }
            }
        }
        let descriptor = capability::logical_descriptor(&a.capability, &args);
        let op_key = match self.store.claim(
            a.action_id,
            &args,
            &descriptor,
            KeyScheme::LogicalEffect,
            false,
        )? {
            ClaimResult::Claimed { op_key, .. } => op_key,
            ClaimResult::StaleRevision { current } => {
                return Ok(Next::Replan(format!(
                    "goal revised to r{current} before dispatch"
                )));
            }
            ClaimResult::ControlHold(_) => return Ok(Next::Hold),
            ClaimResult::NotClaimable(s) => {
                return Ok(Next::Replan(format!(
                    "action {} not claimable ({s})",
                    a.action_id
                )));
            }
            ClaimResult::SameEffectUnsettled { action_id } => {
                return Ok(Next::Blocked(format!(
                    "action {action_id} with the same logical effect has an unestablished outcome"
                )));
            }
        };
        self.say(&format!(
            "dispatch {} {} [{}]",
            a.capability,
            crate::util::canonical_json(&args),
            op_key
        ));
        let outcome = self.dispatch(&op_key, &a.capability, &a.capability_version, &args);
        self.settle(a, &step, &contract, &args, outcome)
    }

    fn settle(
        &mut self,
        a: &ActionRow,
        step: &Step,
        contract: &capability::Contract,
        args: &Value,
        outcome: DispatchOutcome,
    ) -> Result<Next> {
        let rev = self.store.state()?.goal_revision;
        match outcome {
            DispatchOutcome::Denied(d) => {
                self.store
                    .set_action_status(a.action_id, A_DENIED, None, Some(&d.to_string()))?;
                Ok(Next::Replan(format!("policy denied {}: {d}", step.step_id)))
            }
            DispatchOutcome::Invalid(m) => {
                self.store
                    .set_action_status(a.action_id, A_FAILED, None, Some(&m))?;
                Ok(Next::Replan(format!("{} invalid: {m}", step.step_id)))
            }
            DispatchOutcome::Adapter(AdapterOutcome::Applied { receipt, replayed }) => {
                if contract.completion == Completion::AcknowledgedThenObserve {
                    self.store.set_action_status(
                        a.action_id,
                        A_ACKNOWLEDGED,
                        Some(&receipt),
                        None,
                    )?;
                    let mut posts = implied_postconditions(&a.capability, args);
                    posts.extend(step.postconditions.iter().cloned());
                    let obs = match self.observe() {
                        Ok(o) => o,
                        Err(e) => {
                            return Ok(Next::Blocked(format!(
                                "{} acknowledged but cannot be verified: {e}",
                                step.step_id
                            )));
                        }
                    };
                    match all_hold(&obs, &self.classes(), &posts, rev) {
                        Tri::Yes => {
                            self.store.set_action_status(
                                a.action_id,
                                A_SUCCEEDED,
                                Some(&receipt),
                                None,
                            )?;
                            self.store.event(
                                "action_succeeded",
                                &format!(
                                    "{} verified{}",
                                    step.step_id,
                                    if replayed {
                                        " (receipt replayed; no new effect)"
                                    } else {
                                        ""
                                    }
                                ),
                                Some(a.action_id),
                                None,
                            )?;
                            Ok(Next::Continue)
                        }
                        Tri::Unknown(why) => Ok(Next::Blocked(format!(
                            "{} acknowledged but not verifiable: {why}",
                            step.step_id
                        ))),
                        Tri::No(why) => {
                            self.store.set_action_status(
                                a.action_id,
                                A_OUTCOME_UNKNOWN,
                                Some(&receipt),
                                Some(&why),
                            )?;
                            Ok(Next::Blocked(format!(
                                "{} acknowledged by the adapter but observation disagrees: {why}",
                                step.step_id
                            )))
                        }
                    }
                } else {
                    self.store
                        .set_action_status(a.action_id, A_SUCCEEDED, Some(&receipt), None)?;
                    self.store.event(
                        "action_succeeded",
                        &format!("{} applied", step.step_id),
                        Some(a.action_id),
                        None,
                    )?;
                    Ok(Next::Continue)
                }
            }
            DispatchOutcome::Adapter(AdapterOutcome::Rejected { category, message }) => {
                self.store.set_action_status(
                    a.action_id,
                    A_FAILED,
                    None,
                    Some(&format!("{category}: {message}")),
                )?;
                self.event(
                    "action_rejected",
                    &format!(
                        "{} rejected by adapter ({category}): {message}; no effect",
                        step.step_id
                    ),
                    Some(a.action_id),
                    None,
                )?;
                if category == "key_conflict" {
                    return Ok(Next::Blocked(format!(
                        "operation key conflict on {}: {message}",
                        step.step_id
                    )));
                }
                Ok(Next::Replan(format!(
                    "{} rejected: {message}",
                    step.step_id
                )))
            }
            DispatchOutcome::Adapter(AdapterOutcome::Timeout) => {
                self.store.set_action_status(
                    a.action_id,
                    A_OUTCOME_UNKNOWN,
                    None,
                    Some("timeout: outcome unknown"),
                )?;
                self.event(
                    "outcome_unknown",
                    &format!(
                        "{} timed out; outcome unknown — reconciling before any retry",
                        step.step_id
                    ),
                    Some(a.action_id),
                    None,
                )?;
                let row = self.store.action(a.action_id)?;
                match self.reconcile_one(&row)? {
                    Recon::Settled => Ok(Next::Continue),
                    Recon::Unresolved(r) => Ok(Next::Blocked(r)),
                }
            }
            DispatchOutcome::Adapter(AdapterOutcome::Unavailable { message }) => {
                let row = self.store.action(a.action_id)?;
                let max_attempts = step.retry.max_attempts.clamp(1, 3) as i64;
                if contract.safe_to_redispatch_same_key() && row.attempts < max_attempts {
                    self.store.set_action_status(
                        a.action_id,
                        A_RETRY_PENDING,
                        None,
                        Some(&format!(
                            "adapter unavailable, request not accepted: {message}"
                        )),
                    )?;
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(Next::Continue)
                } else {
                    self.store.set_action_status(
                        a.action_id,
                        A_RETRY_PENDING,
                        None,
                        Some(&format!("adapter unavailable: {message}")),
                    )?;
                    Ok(Next::Blocked(format!(
                        "{}: adapter unavailable ({message}); resume when it is available",
                        step.step_id
                    )))
                }
            }
        }
    }

    // ------------------------------------------------------------------ completion

    fn build_report(&mut self, status: &str, reason: &str) -> Result<Option<Value>> {
        let obs = match self.observe() {
            Ok(o) => o,
            Err(_) => return Ok(None),
        };
        let (rev, goal) = self.store.current_goal()?;
        let classes = self.classes();
        let inv = inventory::view(&self.store)?;
        let consumed = inventory::consumed_evidence(&self.store)?;
        let assignments = self.summary()?.previous_kit_assignments;
        let rep = verifier::verify(&goal, rev, &obs, &classes, &inv, &consumed, &assignments);
        let mut v = serde_json::to_value(&rep)?;
        v["status"] = json!(status);
        v["reason"] = json!(reason);
        v["planner"] = json!(self.planner.label());
        v["arm"] = json!("revolve");
        v["adjustments"] = serde_json::to_value(
            inventory::ledger(&self.store)?
                .into_iter()
                .filter(|e| e.entry_type == "adjust")
                .collect::<Vec<_>>(),
        )?;
        v["questions"] = serde_json::to_value(self.store.questions()?)?;
        Ok(Some(v))
    }

    fn finalize(&mut self, forced: Option<&str>, reason: String) -> Result<String> {
        if let Some(e) = crate::bookkeeping::settle_inventory(
            &mut self.store,
            &mut self.world,
            &self.policy,
            false,
        )? {
            self.event("cleanup_failed", &e, None, None)?;
        }
        if let Some(r) = self.reconcile_all()? {
            self.store.set_status(R_BLOCKED, Some(&r))?;
            return Ok(R_BLOCKED.into());
        }
        let (_, goal) = self.store.current_goal()?;
        let Some(mut report) = self.build_report("pending", &reason)? else {
            self.store.set_status(
                R_BLOCKED,
                Some("cannot observe the workspace to verify the result"),
            )?;
            return Ok(R_BLOCKED.into());
        };
        let n = report["kits_verified_complete"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0) as u32;
        let status = match forced {
            Some(s) => s,
            None if n >= goal.kit_quantity => R_COMPLETED,
            None => R_PARTIAL,
        };
        report["status"] = json!(status);
        self.store.set_final_report(&report)?;
        self.store.set_status(status, Some(&reason))?;
        let missing = report["missing_to_finish"].clone();
        self.event(
            "run_finished",
            &format!("run {status}: {n} of {} kit(s) verified complete by observation; missing to finish: {}; {reason}", goal.kit_quantity, missing),
            None,
            Some(report),
        )?;
        Ok(status.into())
    }

    fn finish_cancel(&mut self) -> Result<String> {
        let n = self.store.invalidate_pending(A_CANCELLED)?;
        self.event("cancel_processing", &format!("cancel request: {n} undispatched action(s) cancelled; establishing in-flight outcomes"), None, None)?;
        if let Some(reason) = self.reconcile_all()? {
            self.store.set_status(
                R_BLOCKED,
                Some(&format!(
                    "cancel requested but an in-flight outcome is unknown: {reason}"
                )),
            )?;
            return Ok(R_BLOCKED.into());
        }
        if let Some(p) = self.active_plan()? {
            self.store.set_plan_status(p.plan_seq, "superseded")?;
            self.store.set_active_plan(None)?;
        }
        if let Some(e) = crate::bookkeeping::settle_inventory(
            &mut self.store,
            &mut self.world,
            &self.policy,
            true,
        )? {
            self.event("cleanup_failed", &e, None, None)?;
        }
        let report = self.build_report(R_STOPPED, "stopped by human cancel")?;
        if let Some(r) = &report {
            self.store.set_final_report(r)?;
        }
        self.store.set_status(
            R_STOPPED,
            Some("stopped by human cancel; effects that already happened remain"),
        )?;
        let kits = report
            .as_ref()
            .and_then(|r| r["kits_verified_complete"].as_array().map(|a| a.len()))
            .unwrap_or(0);
        self.event("cancel_acknowledged", &format!("cancel acknowledged: run stopped with {kits} kit(s) verified complete; completed effects remain in the workspace"), None, report)?;
        Ok(R_STOPPED.into())
    }
}
