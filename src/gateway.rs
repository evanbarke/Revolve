//! Capability gateway: the single dispatch path shared by Revolve's executor and
//! the direct-tool baseline. It enforces policy, checks argument types against the
//! contract, verifies evidence for inventory records, routes to the adapter, and
//! lets the harness's scripted human act while an effect is in flight.

use crate::capability::{self, Contract};
use crate::inventory;
use crate::model::{LocationType, Observation, bin_for_kind, effective_kind};
use crate::policy::{Denial, Policy};
use crate::scenario::HumanAction;
use crate::sim::{AdapterOutcome, FiredHumanAction, SimWorld, StatusResult};
use crate::store::RunStore;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "gateway", rename_all = "snake_case")]
pub enum DispatchOutcome {
    Adapter(AdapterOutcome),
    Denied(Denial),
    /// Arguments failed the contract's type check; nothing was sent.
    Invalid(String),
}

/// Harness hook for human actions due at world events (scripted human). The
/// interactive UI/CLI human does not use this; they write to the store directly.
pub trait HumanHook {
    fn on_due(&mut self, store: &mut RunStore, fired: &[FiredHumanAction]);
}

/// Scripted human that performs scenario corrections/cancellations at their triggers.
pub struct ScriptedHuman;

impl HumanHook for ScriptedHuman {
    fn on_due(&mut self, store: &mut RunStore, fired: &[FiredHumanAction]) {
        for f in fired {
            match &f.action {
                HumanAction::Correct { goal, on } => {
                    match store.submit_goal_revision(
                        goal,
                        "scripted-human",
                        &format!("scenario correction at {}", on.describe()),
                    ) {
                        Ok(rev) => eprintln!(
                            "[scripted human] goal corrected to revision {rev}: {}",
                            goal.summary()
                        ),
                        Err(e) => eprintln!("[scripted human] correction rejected: {e}"),
                    }
                }
                HumanAction::Cancel { on } => match store.request_control("cancel") {
                    Ok(()) => eprintln!("[scripted human] cancel requested at {}", on.describe()),
                    Err(e) => eprintln!("[scripted human] cancel rejected: {e}"),
                },
            }
        }
    }
}

pub struct Gateway<'a> {
    pub world: &'a mut SimWorld,
    pub policy: &'a Policy,
    pub hook: Option<&'a mut (dyn HumanHook + 'static)>,
}

pub fn check_arg_types(contract: &Contract, args: &Value) -> Result<(), String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    for spec in &contract.args {
        let v = obj
            .get(spec.name)
            .ok_or_else(|| format!("missing argument `{}`", spec.name))?;
        spec.ty
            .check(v)
            .map_err(|e| format!("argument `{}`: {e}", spec.name))?;
    }
    for k in obj.keys() {
        if contract.arg(k).is_none() {
            return Err(format!("unknown argument `{k}` for {}", contract.id));
        }
    }
    Ok(())
}

impl<'a> Gateway<'a> {
    pub fn observe(&mut self, store: &RunStore) -> Result<Observation, String> {
        let obs = self.world.observe()?;
        store.record_observation(&obs).map_err(|e| e.msg)?;
        Ok(obs)
    }

    pub fn status(&mut self, store: &RunStore, capability: &str, op_key: &str) -> StatusResult {
        if capability.starts_with("inventory.") {
            inventory::status(store, op_key)
        } else {
            self.world.status(op_key)
        }
    }

    /// Evidence check for inventory records that claim a physical fact.
    fn check_evidence(
        &mut self,
        store: &RunStore,
        capability: &str,
        args: &Value,
    ) -> Option<AdapterOutcome> {
        let (obj, want_tray) = match capability {
            "inventory.consume" => (args.get("evidence_object_id")?.as_str()?, true),
            "inventory.return" => (args.get("evidence_object_id")?.as_str()?, false),
            _ => return None,
        };
        let kind = args.get("part_kind")?.as_str()?;
        let obs = match self.observe(store) {
            Ok(o) => o,
            Err(m) => {
                return Some(AdapterOutcome::Unavailable {
                    message: format!("cannot observe evidence: {m}"),
                });
            }
        };
        let classes = store.classifications().unwrap_or_default();
        let Some(o) = obs.object(obj) else {
            return Some(AdapterOutcome::Rejected {
                category: "evidence_mismatch".into(),
                message: format!("evidence object {obj} is not visible in the workspace"),
            });
        };
        if effective_kind(o, &classes) != kind {
            return Some(AdapterOutcome::Rejected {
                category: "evidence_mismatch".into(),
                message: format!(
                    "evidence object {obj} is {} not {kind}",
                    effective_kind(o, &classes)
                ),
            });
        }
        let at_ok = if want_tray {
            obs.location(&o.location_id)
                .is_some_and(|l| l.loc_type == LocationType::Tray)
        } else {
            o.location_id == bin_for_kind(kind)
        };
        if !at_ok {
            return Some(AdapterOutcome::Rejected {
                category: "evidence_mismatch".into(),
                message: format!(
                    "evidence object {obj} is at {}, which does not support this record",
                    o.location_id
                ),
            });
        }
        None
    }

    pub fn dispatch(
        &mut self,
        store: &mut RunStore,
        op_key: &str,
        capability: &str,
        version: &str,
        args: &Value,
    ) -> DispatchOutcome {
        if let Err(d) = self.policy.authorize(capability, version, args) {
            let _ = store.event(
                "policy_denied",
                &format!("policy denied {capability}: {d}"),
                None,
                None,
            );
            return DispatchOutcome::Denied(d);
        }
        let Some(contract) = capability::lookup(capability) else {
            return DispatchOutcome::Invalid(format!("no contract for {capability}"));
        };
        if contract.version != version {
            return DispatchOutcome::Invalid(format!(
                "{capability} version {version} is not registered (have {})",
                contract.version
            ));
        }
        if let Err(e) = check_arg_types(&contract, args) {
            return DispatchOutcome::Invalid(e);
        }
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (outcome, fired) = match capability {
            "workspace.pick" => self.world.pick(op_key, &s("object_id"), &s("source_id")),
            "workspace.place" => self
                .world
                .place(op_key, &s("object_id"), &s("destination_id")),
            c if c.starts_with("inventory.") => {
                // Idempotent replays must not re-check evidence (the world may have moved on).
                let replay = matches!(
                    inventory::status(store, op_key),
                    StatusResult::Applied { .. } | StatusResult::Rejected { .. }
                );
                if !replay && let Some(o) = self.check_evidence(store, capability, args) {
                    return DispatchOutcome::Adapter(o);
                }
                (inventory::apply(store, op_key, capability, args), vec![])
            }
            other => {
                return DispatchOutcome::Invalid(format!(
                    "{other} cannot be dispatched as an action"
                ));
            }
        };
        if !fired.is_empty()
            && let Some(h) = self.hook.as_deref_mut()
        {
            h.on_due(store, &fired);
            let ids: Vec<i64> = fired.iter().map(|f| f.fault_id).collect();
            if let Err(e) = self.world.mark_delivered(&ids) {
                let _ = store.event(
                    "harness_error",
                    &format!("could not mark human actions delivered: {e}"),
                    None,
                    None,
                );
            }
        }
        DispatchOutcome::Adapter(outcome)
    }
}

/// Deliver scripted human actions that fired but were never delivered (a crash
/// right after the triggering effect). Called by executors at start-up.
pub fn deliver_pending_human_actions(
    world: &mut SimWorld,
    store: &mut RunStore,
    hook: &mut dyn HumanHook,
) -> crate::util::Result<()> {
    let pending = world.undelivered_human_actions()?;
    if pending.is_empty() {
        return Ok(());
    }
    store.event(
        "human_actions_redelivered",
        &format!(
            "{} scripted human action(s) fired before a crash are delivered now",
            pending.len()
        ),
        None,
        None,
    )?;
    hook.on_due(store, &pending);
    let ids: Vec<i64> = pending.iter().map(|f| f.fault_id).collect();
    world.mark_delivered(&ids)
}
