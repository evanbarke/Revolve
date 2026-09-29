//! Local inventory bookkeeping executed by the runner itself (not by a planner):
//! evidence-bound record adjustments, consumption for parts observed in kit trays,
//! and reservation release. Every operation goes through the normal
//! claim -> gateway -> adapter path with a runtime-origin action and its own key.
//!
//! Revolve's executor uses these automatically (before planning, at completion,
//! on cancel). The baseline runner uses them only on cancel, where the agent is
//! no longer consulted; during normal operation the baseline agent keeps the
//! record itself through the same capabilities.

use crate::capability;
use crate::gateway::{DispatchOutcome, Gateway};
use crate::inventory;
use crate::model::{LocationType, Observation, effective_kind};
use crate::policy::Policy;
use crate::sim::{AdapterOutcome, SimWorld};
use crate::store::*;
use crate::util::Result;
use serde_json::{Value, json};

/// Whether a bookkeeping error means "something changed underneath" (a pause or
/// cancel is in force, or the goal was revised) rather than a real failure.
pub fn is_hold(err: &str) -> bool {
    err.contains("ControlHold") || err.contains("StaleRevision")
}

/// Execute one runtime-originated bookkeeping action. Returns Some(error) if it did not apply.
pub fn runtime_action(
    store: &mut RunStore,
    world: &mut SimWorld,
    policy: &Policy,
    capability: &str,
    args: Value,
    cleanup: bool,
) -> Result<Option<String>> {
    let rev = store.state()?.goal_revision;
    let step_id = format!("rt-{}", capability.replace('.', "-"));
    let spec = json!({"step_id": step_id, "capability": capability, "capability_version": "1", "arguments": args});
    let id = store.materialize(
        None,
        "runtime",
        rev,
        &[(step_id, capability.into(), "1".into(), args.clone(), spec)],
    )?[0];
    let descriptor = capability::logical_descriptor(capability, &args);
    let op_key = match store.claim(id, &args, &descriptor, KeyScheme::LogicalEffect, cleanup)? {
        ClaimResult::Claimed { op_key, .. } => op_key,
        other => {
            store.set_action_status(id, A_SUPERSEDED, None, Some(&format!("{other:?}")))?;
            return Ok(Some(format!("{other:?}")));
        }
    };
    let outcome = {
        let mut gw = Gateway {
            world,
            policy,
            hook: None,
        };
        gw.dispatch(store, &op_key, capability, "1", &args)
    };
    let (status, result, err): (&str, Option<Value>, Option<String>) = match outcome {
        DispatchOutcome::Adapter(AdapterOutcome::Applied { receipt, .. }) => {
            (A_SUCCEEDED, Some(receipt), None)
        }
        DispatchOutcome::Adapter(AdapterOutcome::Rejected { category, message }) => {
            (A_FAILED, None, Some(format!("{category}: {message}")))
        }
        DispatchOutcome::Adapter(AdapterOutcome::Timeout) => {
            (A_OUTCOME_UNKNOWN, None, Some("timeout".into()))
        }
        DispatchOutcome::Adapter(AdapterOutcome::Unavailable { message }) => {
            (A_FAILED, None, Some(format!("unavailable: {message}")))
        }
        DispatchOutcome::Denied(d) => (A_DENIED, None, Some(d.to_string())),
        DispatchOutcome::Invalid(m) => (A_FAILED, None, Some(m)),
    };
    store.set_action_status(id, status, result.as_ref(), err.as_deref())?;
    Ok(err)
}

fn observe(
    store: &RunStore,
    world: &mut SimWorld,
    policy: &Policy,
) -> std::result::Result<Observation, String> {
    let mut gw = Gateway {
        world,
        policy,
        hook: None,
    };
    gw.observe(store)
}

/// Bring the inventory record in line with a complete observation via evidence-bound adjustments.
pub fn sync_inventory(
    store: &mut RunStore,
    world: &mut SimWorld,
    policy: &Policy,
    obs: &Observation,
    cleanup: bool,
) -> Result<Option<String>> {
    if !obs.withheld.is_empty() {
        return Ok(Some(format!(
            "observation withheld ({}); inventory cannot be verified",
            obs.withheld.join(", ")
        )));
    }
    let consumed = inventory::consumed_evidence(store)?;
    let observed = unconsumed_counts(obs, &store.classifications()?, &consumed);
    let view = inventory::view(store)?;
    for (k, n) in &observed {
        let rec = view.on_hand.get(k).copied().unwrap_or(0);
        if rec == *n {
            continue;
        }
        if view.reserved.get(k).copied().unwrap_or(0) > *n
            && let Some(e) = release_open_reservations(store, world, policy, cleanup)?
        {
            return Ok(Some(e));
        }
        store.event(
            "inventory_discrepancy",
            &format!("inventory record says {rec} {k}, observation (world rev {}) shows {n} unconsumed; recording an evidence-bound adjustment", obs.world_revision),
            None,
            Some(json!({"kind": k, "record": rec, "observed": n, "world_revision": obs.world_revision})),
        )?;
        if let Some(err) = runtime_action(
            store,
            world,
            policy,
            "inventory.adjust_count",
            json!({"part_kind": k, "observed_count": n, "observation_revision": obs.world_revision}),
            cleanup,
        )? {
            return Ok(Some(format!("inventory adjustment for {k} failed: {err}")));
        }
    }
    Ok(None)
}

/// Release every open reservation. Returns Some(error) if a pause/cancel/correction
/// prevented it (the caller should re-evaluate); other failures are logged.
pub fn release_open_reservations(
    store: &mut RunStore,
    world: &mut SimWorld,
    policy: &Policy,
    cleanup: bool,
) -> Result<Option<String>> {
    for r in inventory::view(store)?
        .open_reservations()
        .cloned()
        .collect::<Vec<_>>()
    {
        if let Some(e) = runtime_action(
            store,
            world,
            policy,
            "inventory.release",
            json!({"reservation_id": r.reservation_id}),
            cleanup,
        )? {
            if is_hold(&e) {
                return Ok(Some(e));
            }
            store.event(
                "cleanup_failed",
                &format!("could not release {}: {e}", r.reservation_id),
                None,
                None,
            )?;
        }
    }
    Ok(None)
}

/// Record returns for consumed parts observed back in their stock bin, record
/// consumption for parts observed in kit trays that are not yet recorded (e.g. a
/// cancel landed between a place and its consume), then release reservations. Residual differences are NOT adjusted here: they are reported,
/// so an accounting bug stays visible.
pub fn settle_inventory(
    store: &mut RunStore,
    world: &mut SimWorld,
    policy: &Policy,
    cleanup: bool,
) -> Result<Option<String>> {
    let obs = match observe(store, world, policy) {
        Ok(o) => o,
        Err(e) => return Ok(Some(format!("cannot observe to settle inventory: {e}"))),
    };
    if !obs.withheld.is_empty() {
        return Ok(Some(format!(
            "observation withheld ({}); inventory not settled",
            obs.withheld.join(", ")
        )));
    }
    let classes = store.classifications()?;
    let consumed = inventory::consumed_evidence(store)?;
    // A consumed part seen back in its own stock bin (e.g. a rework placement whose
    // return record was interrupted) is recorded as returned.
    for o in &obs.objects {
        let kind = effective_kind(o, &classes);
        if consumed.contains(&o.object_id) && o.location_id == crate::model::bin_for_kind(&kind) {
            let args = json!({"part_kind": kind, "quantity": 1, "evidence_object_id": o.object_id});
            if let Some(e) =
                runtime_action(store, world, policy, "inventory.return", args, cleanup)?
            {
                if is_hold(&e) {
                    return Ok(Some(e));
                }
                store.event(
                    "cleanup_failed",
                    &format!("could not record return of {}: {e}", o.object_id),
                    None,
                    None,
                )?;
            }
        }
    }
    let consumed = inventory::consumed_evidence(store)?;
    for o in &obs.objects {
        let in_tray = obs
            .location(&o.location_id)
            .is_some_and(|l| l.loc_type == LocationType::Tray);
        if !in_tray || consumed.contains(&o.object_id) {
            continue;
        }
        let kind = effective_kind(o, &classes);
        let view = inventory::view(store)?;
        let Some(res) = view
            .open_reservations()
            .find(|r| r.remaining.get(&kind).copied().unwrap_or(0) > 0)
        else {
            continue;
        };
        let args = json!({"reservation_id": res.reservation_id, "part_kind": kind, "quantity": 1, "evidence_object_id": o.object_id});
        let r = runtime_action(store, world, policy, "inventory.consume", args, cleanup)?;
        if let Some(e) = &r
            && is_hold(e)
        {
            return Ok(Some(e.clone()));
        }
        if r.is_none() {
            store.event(
                "inventory_settled",
                &format!(
                    "recorded consumption of {} observed in {}",
                    o.object_id, o.location_id
                ),
                None,
                None,
            )?;
        }
    }
    release_open_reservations(store, world, policy, cleanup)
}
