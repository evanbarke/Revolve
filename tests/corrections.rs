//! Goal corrections vs dispatch claims; stale planner replies; permission boundaries.

mod common;

use common::*;
use revolve::harness::Arm;
use revolve::model::GoalSpec;
use revolve::planner::scripted::ScriptedPlanner;
use revolve::planner::{CallInfo, Planner, PlannerError, PlannerResponse, PlanningRequest};
use revolve::runtime::{self, ExecOptions};
use revolve::store::*;
use serde_json::json;
use std::path::PathBuf;

fn materialize(store: &mut RunStore, n: usize, rev: u32) -> Vec<i64> {
    let steps: Vec<_> = (0..n)
        .map(|i| {
            (
                format!("s{i}"),
                "workspace.pick".to_string(),
                "1".to_string(),
                json!({"i": i}),
                json!({}),
            )
        })
        .collect();
    store.materialize(None, "plan", rev, &steps).unwrap()
}

fn two_kits() -> GoalSpec {
    serde_json::from_value(json!({"kit_quantity": 2, "kit_components": {"red_block": 1, "blue_block": 1, "washer": 2}})).unwrap()
}

#[test]
fn claim_ordering_is_defined_by_the_committed_revision() {
    let state = temp_state("claim-order");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut store = RunStore::open(&dir).unwrap();
    let ids = materialize(&mut store, 2, 1);

    // Claim first: the action is in flight and stays so across the correction.
    let c = store
        .claim(
            ids[0],
            &json!({"i": 0}),
            "d0",
            KeyScheme::LogicalEffect,
            false,
        )
        .unwrap();
    assert!(matches!(c, ClaimResult::Claimed { .. }));
    store
        .submit_goal_revision(&two_kits(), "test", "correction")
        .unwrap();
    assert_eq!(
        store.action(ids[0]).unwrap().status,
        A_DISPATCH_RECORDED,
        "claimed before the correction: in flight until reconciled"
    );

    // Correction first: the next claim from the old revision is refused and superseded.
    let c = store
        .claim(
            ids[1],
            &json!({"i": 1}),
            "d1",
            KeyScheme::LogicalEffect,
            false,
        )
        .unwrap();
    assert_eq!(c, ClaimResult::StaleRevision { current: 2 });
    assert_eq!(store.action(ids[1]).unwrap().status, A_SUPERSEDED);
    assert!(
        store.action(ids[1]).unwrap().op_key.is_none(),
        "no operation identity was issued"
    );
}

#[test]
fn concurrent_correction_cannot_slip_between_check_and_claim() {
    for round in 0..5 {
        let state = temp_state("claim-race");
        let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
        let mut store = RunStore::open(&dir).unwrap();
        let ids = materialize(&mut store, 300, 1);
        let dir2: PathBuf = dir.clone();
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = progress.clone();
        let corrector = std::thread::spawn(move || {
            // Commit the correction from a separate connection while claims are streaming.
            let mut s = RunStore::open(&dir2).unwrap();
            while seen.load(std::sync::atomic::Ordering::SeqCst) < 40 + (round as usize) * 20 {
                std::thread::yield_now();
            }
            s.submit_goal_revision(&two_kits(), "ui", "concurrent correction")
                .unwrap();
        });
        for (i, id) in ids.iter().enumerate() {
            let _ = store
                .claim(
                    *id,
                    &json!({"i": i}),
                    &format!("d{i}"),
                    KeyScheme::LogicalEffect,
                    false,
                )
                .unwrap();
            progress.store(i + 1, std::sync::atomic::Ordering::SeqCst);
            // Stand-in for the adapter call a real executor makes between claims. Without
            // it this loop starves other SQLite writers (their busy handler sleeps).
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        corrector.join().unwrap();
        let events = store.events(0, 100_000).unwrap();
        let corr = events
            .iter()
            .find(|e| e.kind == "goal_revised")
            .unwrap()
            .event_id;
        for e in events.iter().filter(|e| e.kind == "dispatch_recorded") {
            assert!(
                e.event_id < corr,
                "an old-revision action was claimed after the correction committed"
            );
        }
        for a in store.actions().unwrap() {
            assert!(
                a.status == A_DISPATCH_RECORDED || a.status == A_SUPERSEDED,
                "unexpected {}",
                a.status
            );
        }
        // With enough actions, both outcomes occur in each round.
        assert!(
            store
                .actions()
                .unwrap()
                .iter()
                .any(|a| a.status == A_SUPERSEDED)
        );
    }
}

/// A planner whose reply arrives after the human changed the goal.
struct SlowPlanner {
    dir: PathBuf,
    inner: ScriptedPlanner,
    corrected: bool,
}

impl Planner for SlowPlanner {
    fn label(&self) -> String {
        "test-slow".into()
    }
    fn plan(&mut self, req: &PlanningRequest) -> (Result<PlannerResponse, PlannerError>, CallInfo) {
        let r = self.inner.plan(req);
        if !self.corrected {
            self.corrected = true;
            let mut s = RunStore::open(&self.dir).unwrap();
            s.submit_goal_revision(&two_kits(), "ui", "changed while the planner was thinking")
                .unwrap();
        }
        r
    }
}

#[test]
fn late_planner_reply_for_an_obsolete_revision_is_rejected() {
    let state = temp_state("stale");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let planner = SlowPlanner {
        dir: dir.clone(),
        inner: ScriptedPlanner,
        corrected: false,
    };
    let status = runtime::drive(
        &dir,
        Box::new(planner),
        None,
        ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, R_COMPLETED);
    let store = RunStore::open(&dir).unwrap();
    assert!(
        store
            .events(0, 10_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "stale_plan_rejected")
    );
    let plans = store.plans().unwrap();
    assert_eq!(plans[0].status, "stale");
    assert!(
        store.plan_actions(plans[0].plan_seq).unwrap().is_empty(),
        "stale plan produced no actions"
    );
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(v.kits_truly_complete.len(), 2, "built against revision 2");
    assert_eq!(v.inventory_mismatches, Vec::<String>::new());
}

#[test]
fn goal_corrections_cannot_carry_permissions() {
    let r: Result<GoalSpec, _> = serde_json::from_value(json!({
        "kit_quantity": 1, "kit_components": {"red_block": 1}, "grants": {"system.shell": "1"}
    }));
    assert!(
        r.is_err(),
        "unknown fields (including permission-like ones) are rejected in a goal"
    );
    let r: Result<GoalSpec, _> = serde_json::from_value(
        json!({"kit_quantity": 1, "kit_components": {"red_block": 1}, "ask_before_substitution": false}),
    );
    assert!(r.unwrap().validate().is_err());
}

#[test]
fn cancel_is_acknowledged_and_keeps_completed_effects() {
    let state = temp_state("cancel");
    let (_run, dir) = new_run(&state, &load("cancel"), Arm::Revolve, "scripted");
    let status = revolve::harness::drive_in_process(
        &dir,
        ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, R_STOPPED);
    let store = RunStore::open(&dir).unwrap();
    assert!(
        store
            .events(0, 10_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "cancel_acknowledged")
    );
    assert!(
        store
            .actions()
            .unwrap()
            .iter()
            .any(|a| a.status == A_CANCELLED)
    );
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert!(v.success, "{}", revolve::eval::summarize_verdict(&v));
}
