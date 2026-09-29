//! Runtime verifier: decides what was actually achieved from a fresh observation
//! and the persisted ledger — never from planner or agent claims. Its report is
//! the only source of "kit complete" statements shown to the user.
//!
//! (The benchmark evaluator in `eval.rs` is separate and uses simulator truth.)

use crate::inventory::InventoryView;
use crate::ir::KitAssignment;
use crate::model::{
    Classification, GRIPPER, GoalSpec, KindCounts, Observation, effective_kind, evaluate_against,
    match_complete_kits,
};
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
    tray_views.sort_by_key(|t| {
        t.location_id
            .rsplit('-')
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    // Each tray is judged against the kit it belongs to (its planned assignment, or
    // the kit whose parts it matches exactly).
    let assigned: std::collections::BTreeMap<String, u32> = assignments
        .iter()
        .map(|a| (a.tray_id.clone(), a.kit))
        .collect();
    let tray_counts: Vec<(String, KindCounts)> = tray_views
        .iter()
        .map(|t| {
            let mut c = KindCounts::new();
            for o in obs.objects_at(&t.location_id) {
                *c.entry(effective_kind(o, classes)).or_default() += 1;
            }
            (t.location_id.clone(), c)
        })
        .collect();
    let matched = match_complete_kits(goal, &tray_counts, &assigned);
    for (t, (_, contents)) in tray_views.iter().zip(tray_counts.iter()) {
        let objects: Vec<String> = obs
            .objects_at(&t.location_id)
            .map(|o| o.object_id.clone())
            .collect();
        let kit = assigned
            .get(&t.location_id)
            .or_else(|| matched.get(&t.location_id))
            .copied();
        let (ok, missing, excess) = match kit {
            Some(k) => {
                let (_, m, e) = evaluate_against(contents, &goal.composition(k), goal);
                (matched.contains_key(&t.location_id), m, e)
            }
            // Parts in a tray that belongs to no kit are all extra.
            None => (false, KindCounts::new(), contents.clone()),
        };
        if ok {
            complete.push(t.location_id.clone());
        }
        if !t.available && !objects.is_empty() {
            notes.push(format!(
                "{} is unavailable and holds {}",
                t.location_id,
                objects.join(", ")
            ));
        }
        trays.push(TrayReport {
            tray_id: t.location_id.clone(),
            kit,
            available: t.available,
            objects,
            contents: contents.clone(),
            matches_composition: ok,
            missing,
            excess,
        });
    }
    let done_kits: std::collections::BTreeSet<u32> = matched.values().copied().collect();
    // Parts available for the remaining kits (stock bins + an unconsumed part in transit).
    let stock = crate::store::available_stock(obs, classes, consumed);
    // What the record should say: every known part not yet recorded as consumed.
    let unconsumed = crate::store::unconsumed_counts(obs, classes, consumed);
    // What is still missing to finish the remaining kits: each unfinished kit needs its
    // own parts, less what is already in its (available, conforming) tray.
    let mut needed = KindCounts::new();
    for k in 1..=goal.kit_quantity {
        if done_kits.contains(&k) {
            continue;
        }
        let comp = goal.composition(k);
        let partial = trays.iter().find(|t| {
            t.kit == Some(k) && t.available && t.excess.is_empty() && !t.objects.is_empty()
        });
        let still = match partial {
            Some(t) => evaluate_against(&t.contents, &comp, goal).1,
            None => comp,
        };
        for (kd, n) in still {
            *needed.entry(kd).or_default() += n;
        }
    }
    let mut missing_to_finish = KindCounts::new();
    for (k, n) in &needed {
        let mut have = stock.get(k).copied().unwrap_or(0);
        for s in goal.substitute_for(k) {
            let spare = stock
                .get(&s)
                .copied()
                .unwrap_or(0)
                .saturating_sub(needed.get(&s).copied().unwrap_or(0));
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
        notes.push(format!(
            "observation withheld for {}; inventory not verified",
            obs.withheld.join(", ")
        ));
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
