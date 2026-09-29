//! Runtime verifier: decides what was actually achieved from a fresh observation
//! and the persisted ledger — never from planner or agent claims. Its report is
//! the only source of "kit complete" statements shown to the user.
//!
//! (The benchmark evaluator in `eval.rs` is separate and uses simulator truth.)

use crate::inventory::InventoryView;
use crate::ir::KitAssignment;
use crate::model::{effective_kind, evaluate_kit, Classification, GoalSpec, KindCounts, Observation, GRIPPER};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TrayReport {
    pub tray_id: String,
    pub kit: Option<u32>,
    pub available: bool,
    pub objects: Vec<String>,
    pub contents: KindCounts,
    pub matches_composition: bool,
    pub missing: KindCounts,
    pub excess: KindCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub goal_revision: u32,
    pub goal: String,
    pub kits_requested: u32,
    /// Trays whose observed contents exactly match the current composition (capped at the request).
    pub kits_verified_complete: Vec<String>,
    pub trays: Vec<TrayReport>,
    /// Parts that would still be needed (beyond observed stock) to finish the remaining kits.
    pub missing_to_finish: KindCounts,
    pub stock_observed: KindCounts,
    pub inventory_record: KindCounts,
    pub inventory_matches_observation: bool,
    pub inventory_discrepancies: Vec<String>,
    pub gripper: Option<String>,
    pub notes: Vec<String>,
    pub verified_world_revision: u64,
    pub evidence: String,
}

pub fn verify(
    goal: &GoalSpec,
    goal_revision: u32,
    obs: &Observation,
    classes: &[Classification],
    inventory: &InventoryView,
    consumed: &[String],
    assignments: &[KitAssignment],
) -> Report {
    let mut trays = vec![];
    let mut complete = vec![];
    let mut notes = vec![];
    let mut tray_views = obs.trays();
    tray_views.sort_by_key(|t| t.location_id.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(u32::MAX));
    for t in tray_views {
        let objects: Vec<String> = obs.objects_at(&t.location_id).map(|o| o.object_id.clone()).collect();
        let mut contents = KindCounts::new();
        for o in obs.objects_at(&t.location_id) {
            *contents.entry(effective_kind(o, classes)).or_default() += 1;
        }
        let (ok, missing, excess) = evaluate_kit(&contents, goal);
        let kit = assignments.iter().find(|a| a.tray_id == t.location_id).map(|a| a.kit);
        if ok && !objects.is_empty() {
            complete.push(t.location_id.clone());
        }
        if !t.available && !objects.is_empty() {
            notes.push(format!("{} is unavailable and holds {}", t.location_id, objects.join(", ")));
        }
        trays.push(TrayReport {
            tray_id: t.location_id.clone(),
            kit,
            available: t.available,
            objects,
            contents,
            matches_composition: ok,
            missing,
            excess,
        });
    }
    let q = goal.kit_quantity as usize;
    if complete.len() > q {
        notes.push(format!(
            "{} trays match the composition but only {} kit(s) were requested; extra: {}",
            complete.len(),
            q,
            complete[q..].join(", ")
        ));
        complete.truncate(q);
    }
    // Parts available for the remaining kits (stock bins + an unconsumed part in transit).
    let stock = crate::store::available_stock(obs, classes, consumed);
    // What the record should say: every known part not yet recorded as consumed.
    let unconsumed = crate::store::unconsumed_counts(obs, classes, consumed);
    // What is still missing to finish the remaining kits.
    let remaining = q.saturating_sub(complete.len());
    let mut needed = KindCounts::new();
    let mut partials: Vec<&TrayReport> = trays.iter().filter(|t| !t.matches_composition && t.available && t.excess.is_empty() && !t.objects.is_empty()).collect();
    partials.truncate(remaining);
    for t in &partials {
        for (k, n) in &t.missing {
            *needed.entry(k.clone()).or_default() += n;
        }
    }
    for _ in partials.len()..remaining {
        for (k, n) in &goal.kit_components {
            *needed.entry(k.clone()).or_default() += n;
        }
    }
    let mut missing_to_finish = KindCounts::new();
    for (k, n) in &needed {
        let mut have = stock.get(k).copied().unwrap_or(0);
        for s in goal.substitute_for(k) {
            let spare = stock.get(&s).copied().unwrap_or(0).saturating_sub(needed.get(&s).copied().unwrap_or(0));
            have += spare;
        }
        if *n > have {
            missing_to_finish.insert(k.clone(), n - have);
        }
    }
    let mut discrepancies = vec![];
    if obs.withheld.is_empty() {
        for (k, n) in &unconsumed {
            let rec = inventory.on_hand.get(k).copied().unwrap_or(0);
            if rec != *n {
                discrepancies.push(format!("{k}: record {rec}, observed unconsumed {n}"));
            }
        }
    } else {
        notes.push(format!("observation withheld for {}; inventory not verified", obs.withheld.join(", ")));
    }
    let gripper = obs.objects_at(GRIPPER).next().map(|o| o.object_id.clone());
    if let Some(g) = &gripper {
        notes.push(format!("gripper still holds {g}"));
    }
    Report {
        goal_revision,
        goal: goal.summary(),
        kits_requested: goal.kit_quantity,
        kits_verified_complete: complete,
        trays,
        missing_to_finish,
        stock_observed: stock,
        inventory_record: inventory.on_hand.clone(),
        inventory_matches_observation: discrepancies.is_empty() && obs.withheld.is_empty(),
        inventory_discrepancies: discrepancies,
        gripper,
        notes,
        verified_world_revision: obs.world_revision,
        evidence: obs.source.clone(),
    }
}
