//! Real subprocess crash at the effect/result gap, then recovery by a new process.

mod common;

use common::*;
use revolve::harness::Arm;
use revolve::sim::SimWorld;
use revolve::store::*;
use revolve::util::SIMULATED_CRASH_EXIT_CODE;
use std::process::Command;

fn resume(state: &std::path::Path, run: &str) -> std::process::ExitStatus {
    Command::new(exe())
        .args(["resume", "--run", run, "--quiet", "--state-dir"])
        .arg(state)
        .status()
        .expect("spawn revolve")
}

#[test]
fn crash_after_effect_is_reconciled_not_repeated() {
    let state = temp_state("crash-after");
    let sc = load("crash-after-effect");
    let (run, dir) = new_run(&state, &sc, Arm::Revolve, "interactive");

    // 1. The executor process dies right after the simulator commits an effect.
    let st = resume(&state, &run);
    assert_eq!(
        st.code(),
        Some(SIMULATED_CRASH_EXIT_CODE),
        "first executor should exit via the injected crash"
    );

    // 2. Across the gap: the effect and its receipt exist in the adapter store,
    //    while the executor store still shows the action only as dispatch-recorded.
    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    let fired = truth
        .faults
        .iter()
        .find(|f| f.state == "fired")
        .expect("crash fault fired");
    let key = fired.fired_op_key.clone().unwrap();
    let receipt = truth
        .receipts
        .iter()
        .find(|r| r.op_key == key)
        .expect("effect receipt survives the crash");
    assert_eq!(receipt.outcome, "applied");
    let store = RunStore::open(&dir).unwrap();
    let action = store
        .actions()
        .unwrap()
        .into_iter()
        .find(|a| a.op_key.as_deref() == Some(&key))
        .unwrap();
    assert_eq!(
        action.status, A_DISPATCH_RECORDED,
        "executor never recorded the result"
    );
    assert!(
        !OwnerLock::is_held(&dir),
        "OS released the dead executor's lock"
    );
    let applied_before = truth
        .receipts
        .iter()
        .filter(|r| r.outcome == "applied")
        .count();
    drop(store);

    // 3. A new executor reconciles by status lookup + observation and continues.
    let st = resume(&state, &run);
    assert!(st.success());
    let store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_COMPLETED);
    let action = store.action(action.action_id).unwrap();
    assert_eq!(action.status, A_SUCCEEDED);
    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    // The reconciled effect was not applied a second time under any key.
    let same: Vec<_> = truth
        .receipts
        .iter()
        .filter(|r| {
            r.outcome == "applied"
                && r.detail["object_id"] == receipt.detail["object_id"]
                && r.detail["to"] == receipt.detail["to"]
        })
        .collect();
    assert_eq!(same.len(), 1, "effect repeated: {same:?}");
    assert!(
        truth
            .receipts
            .iter()
            .filter(|r| r.outcome == "applied")
            .count()
            > applied_before
    );
    assert!(
        store
            .events(0, 10_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "reconciled" && e.action_id == Some(action.action_id))
    );

    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert!(v.success, "{}", revolve::eval::summarize_verdict(&v));
}

#[test]
fn crash_before_effect_retries_under_the_same_key_once() {
    let state = temp_state("crash-before");
    let sc = load("crash-before-effect");
    let (run, dir) = new_run(&state, &sc, Arm::Revolve, "interactive");
    assert_eq!(resume(&state, &run).code(), Some(SIMULATED_CRASH_EXIT_CODE));

    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    let key = truth
        .faults
        .iter()
        .find(|f| f.state == "fired")
        .unwrap()
        .fired_op_key
        .clone()
        .unwrap();
    assert!(
        truth.receipts.iter().all(|r| r.op_key != key),
        "no effect before the crash"
    );

    assert!(resume(&state, &run).success());
    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    let under_key: Vec<_> = truth.receipts.iter().filter(|r| r.op_key == key).collect();
    assert_eq!(
        under_key.len(),
        1,
        "exactly one effect under the original key"
    );
    assert_eq!(under_key[0].outcome, "applied");
    let store = RunStore::open(&dir).unwrap();
    let a = store
        .actions()
        .unwrap()
        .into_iter()
        .find(|a| a.op_key.as_deref() == Some(&key))
        .unwrap();
    assert_eq!(a.attempts, 2, "same action, same key, second attempt");
    assert_eq!(store.state().unwrap().status, R_COMPLETED);
}

#[test]
fn baseline_arm_recovers_from_the_same_crash() {
    let state = temp_state("crash-baseline");
    let sc = load("crash-after-effect");
    let (run, dir) = new_run(&state, &sc, Arm::Baseline, "interactive");
    assert_eq!(resume(&state, &run).code(), Some(SIMULATED_CRASH_EXIT_CODE));
    assert!(resume(&state, &run).success());
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert!(v.success, "{}", revolve::eval::summarize_verdict(&v));
}

#[test]
fn second_executor_cannot_take_an_owned_run() {
    let state = temp_state("lock");
    let sc = load("nominal");
    let (_run, dir) = new_run(&state, &sc, Arm::Revolve, "interactive");
    let _held = OwnerLock::acquire(&dir).unwrap();
    assert!(
        OwnerLock::acquire(&dir).is_err(),
        "a second owner must be refused"
    );
    let err = revolve::harness::drive_in_process(&dir, Default::default()).unwrap_err();
    assert!(err.msg.contains("owned by another executor"), "{err}");
}
