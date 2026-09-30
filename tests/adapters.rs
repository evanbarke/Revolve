//! Adapter-level guarantees: idempotency keys, argument fingerprints, receipts.

mod common;

use common::*;
use revolve::harness::Arm;
use revolve::inventory;
use revolve::sim::{AdapterOutcome, SimWorld, StatusResult};
use revolve::store::RunStore;
use serde_json::json;

#[test]
fn workspace_key_replay_and_conflict() {
    let state = temp_state("adapter-ws");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut w = SimWorld::open(&dir.join("world.db")).unwrap();

    let (o, _) = w.pick("op-k1", "red-01", "bin-red");
    assert!(matches!(
        o,
        AdapterOutcome::Applied {
            replayed: false,
            ..
        }
    ));
    let rev = w.truth().unwrap().world_revision;

    // Same key, same arguments: the recorded receipt is returned; no new effect.
    let (o, _) = w.pick("op-k1", "red-01", "bin-red");
    assert!(matches!(o, AdapterOutcome::Applied { replayed: true, .. }));
    assert_eq!(w.truth().unwrap().world_revision, rev);

    // Same key, different arguments: rejected without any effect.
    let (o, _) = w.pick("op-k1", "red-02", "bin-red");
    assert!(
        matches!(o, AdapterOutcome::Rejected { ref category, .. } if category == "key_conflict"),
        "{o:?}"
    );
    let t = w.truth().unwrap();
    assert_eq!(t.world_revision, rev);
    assert_eq!(
        t.objects
            .iter()
            .find(|x| x.object_id == "red-02")
            .unwrap()
            .location_id,
        "bin-red"
    );

    // Status lookup distinguishes applied / not found.
    assert!(matches!(w.status("op-k1"), StatusResult::Applied { .. }));
    assert!(matches!(w.status("op-never"), StatusResult::NotFound));

    // Adapter enforces preconditions itself: gripper is full.
    let (o, _) = w.pick("op-k2", "blue-01", "bin-blue");
    assert!(
        matches!(o, AdapterOutcome::Rejected { ref category, .. } if category == "precondition_failed")
    );
    // ...and records that rejection under the key.
    assert!(matches!(w.status("op-k2"), StatusResult::Rejected { .. }));
}

#[test]
fn objects_are_conserved_and_single_located() {
    let state = temp_state("adapter-inv");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut w = SimWorld::open(&dir.join("world.db")).unwrap();
    let before = w.truth().unwrap();
    for (i, (obj, bin)) in [("red-01", "bin-red"), ("blue-01", "bin-blue")]
        .iter()
        .enumerate()
    {
        let (o, _) = w.pick(&format!("op-p{i}"), obj, bin);
        assert!(matches!(o, AdapterOutcome::Applied { .. }), "{o:?}");
        let (o, _) = w.place(&format!("op-q{i}"), obj, "tray-1");
        assert!(matches!(o, AdapterOutcome::Applied { .. }), "{o:?}");
    }
    // Bin destinations only accept their kind.
    let (o, _) = w.pick("op-p9", "washer-01", "bin-washer");
    assert!(matches!(o, AdapterOutcome::Applied { .. }), "{o:?}");
    let (o, _) = w.place("op-q9", "washer-01", "bin-red");
    assert!(matches!(o, AdapterOutcome::Rejected { .. }));
    let after = w.truth().unwrap();
    assert_eq!(before.objects.len(), after.objects.len());
    assert!(
        after
            .objects
            .iter()
            .filter(|o| o.location_id == "gripper")
            .count()
            <= 1
    );
}

#[test]
fn inventory_key_replay_conflict_and_bounds() {
    let state = temp_state("adapter-ledger");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut store = RunStore::open(&dir).unwrap();

    let args = json!({"counts": {"red_block": 2}});
    let r1 = inventory::apply(&mut store, "op-r1", "inventory.reserve", &args);
    let AdapterOutcome::Applied { receipt, .. } = r1 else {
        panic!("{r1:?}")
    };
    let rid = receipt["reservation_id"].as_str().unwrap().to_string();
    assert!(matches!(
        inventory::apply(&mut store, "op-r1", "inventory.reserve", &args),
        AdapterOutcome::Applied { replayed: true, .. }
    ));
    assert_eq!(
        inventory::view(&store).unwrap().reserved["red_block"],
        2,
        "replay must not reserve twice"
    );
    let conflict = inventory::apply(
        &mut store,
        "op-r1",
        "inventory.reserve",
        &json!({"counts": {"red_block": 1}}),
    );
    assert!(
        matches!(conflict, AdapterOutcome::Rejected { ref category, .. } if category == "key_conflict")
    );

    // Reservations never exceed reservable stock (3 red on hand, 2 reserved).
    let over = inventory::apply(
        &mut store,
        "op-r2",
        "inventory.reserve",
        &json!({"counts": {"red_block": 2}}),
    );
    assert!(
        matches!(over, AdapterOutcome::Rejected { ref category, .. } if category == "insufficient_stock")
    );

    // Evidence-bound adjustment must cite a recorded observation with that count.
    let bad = inventory::apply(
        &mut store,
        "op-a1",
        "inventory.adjust_count",
        &json!({"part_kind": "red_block", "observed_count": 1, "observation_revision": 999}),
    );
    assert!(
        matches!(bad, AdapterOutcome::Rejected { ref category, .. } if category == "evidence_mismatch")
    );

    let rel = inventory::apply(
        &mut store,
        "op-x1",
        "inventory.release",
        &json!({"reservation_id": rid}),
    );
    assert!(matches!(rel, AdapterOutcome::Applied { .. }));
    assert_eq!(inventory::view(&store).unwrap().reserved["red_block"], 0);
}
