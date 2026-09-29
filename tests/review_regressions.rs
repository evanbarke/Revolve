//! Regression tests for findings from the independent code review.

mod common;

use common::*;
use revolve::baseline;
use revolve::compiler::{self, CompileContext, DiagCategory, Limits};
use revolve::harness::Arm;
use revolve::inventory::{self, InventoryView};
use revolve::model::GoalSpec;
use revolve::planner::{
    AgentDecision, AgentReport, AgentRequest, CallInfo, Planner, PlannerError, PlannerResponse,
    PlanningRequest,
};
use revolve::policy::Policy;
use revolve::runtime::ExecOptions;
use revolve::scenario::{HumanAction, Trigger};
use revolve::sim::{AdapterOutcome, SimWorld};
use revolve::store::*;
use revolve::util::SIMULATED_CRASH_EXIT_CODE;
use serde_json::{Value, json};
use std::process::Command;

fn two_kits() -> GoalSpec {
    serde_json::from_value(json!({"kit_quantity": 2, "kit_components": {"red_block": 1, "blue_block": 1, "washer": 2}})).unwrap()
}

#[test]
fn goal_revision_is_not_accepted_as_an_effect() {
    let state = temp_state("postcond");
    let sc = load("nominal");
    let mut w = SimWorld::create(&state.join("world.db"), &sc).unwrap();
    let obs = w.observe().unwrap();
    let mut raw: Value =
        serde_json::from_str(&std::fs::read_to_string("examples/plans/kit-assembly.json").unwrap())
            .unwrap();
    raw["steps"][1]["postconditions"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind": "goal_revision_equals", "revision": 1}));
    let inv = InventoryView {
        on_hand: sc.inventory_record(),
        ..Default::default()
    };
    let policy = Policy::default();
    let p = compiler::parse_proposal(&raw).unwrap();
    let d = compiler::compile(
        &p,
        &CompileContext {
            goal_id: "goal-nominal",
            goal_revision: 1,
            goal: &sc.goal,
            observation: &obs,
            classifications: &[],
            inventory: &inv,
            consumed_evidence: &[],
            policy: &policy,
            limits: Limits::default(),
        },
    )
    .unwrap_err();
    assert_eq!(d[0].category, DiagCategory::Schema);
    assert!(d[0].path.contains("postconditions"));
}

#[test]
fn inventory_evidence_is_bound_to_one_object_and_its_status() {
    let state = temp_state("evidence");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut store = RunStore::open(&dir).unwrap();
    let AdapterOutcome::Applied { receipt, .. } = inventory::apply(
        &mut store,
        "op-r",
        "inventory.reserve",
        &json!({"counts": {"washer": 4}}),
    ) else {
        panic!()
    };
    let rid = receipt["reservation_id"].as_str().unwrap().to_string();
    let consume = |q: u64, ev: &str| json!({"reservation_id": rid, "part_kind": "washer", "quantity": q, "evidence_object_id": ev});
    // One object cannot justify three units.
    let r = inventory::apply(
        &mut store,
        "op-c0",
        "inventory.consume",
        &consume(3, "washer-01"),
    );
    assert!(
        matches!(r, AdapterOutcome::Rejected { ref category, .. } if category == "evidence_mismatch"),
        "{r:?}"
    );
    // One object is consumed at most once, even under a different key.
    assert!(matches!(
        inventory::apply(
            &mut store,
            "op-c1",
            "inventory.consume",
            &consume(1, "washer-01")
        ),
        AdapterOutcome::Applied { .. }
    ));
    let r = inventory::apply(
        &mut store,
        "op-c2",
        "inventory.consume",
        &consume(1, "washer-01"),
    );
    assert!(
        matches!(r, AdapterOutcome::Rejected { ref category, .. } if category == "evidence_mismatch"),
        "{r:?}"
    );
    // Only a consumed object can be returned.
    let r = inventory::apply(
        &mut store,
        "op-t1",
        "inventory.return",
        &json!({"part_kind": "washer", "quantity": 1, "evidence_object_id": "washer-02"}),
    );
    assert!(
        matches!(r, AdapterOutcome::Rejected { ref category, .. } if category == "evidence_mismatch"),
        "{r:?}"
    );
    assert_eq!(
        inventory::view(&store).unwrap().on_hand["washer"],
        5,
        "only one unit left stock"
    );
    // Policy (and so the compiler and gateway) allow one unit per record.
    assert!(
        Policy::default()
            .authorize("inventory.consume", "1", &consume(2, "washer-03"))
            .is_err()
    );
}

#[test]
fn control_and_questions_are_race_free() {
    let state = temp_state("control");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut store = RunStore::open(&dir).unwrap();
    // A cancel cannot be withdrawn, and resuming clears only a pause.
    store.request_control("cancel").unwrap();
    assert!(store.request_control("run").is_err());
    assert!(!store.resume_from_pause().unwrap());
    assert_eq!(store.state().unwrap().control, "cancel");

    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Revolve, "interactive");
    let mut store = RunStore::open(&dir).unwrap();
    // A question for an obsolete revision is not opened.
    store
        .submit_goal_revision(&two_kits(), "test", "correction")
        .unwrap();
    let opts = vec![
        QuestionOption {
            id: "approve".into(),
            label: "yes".into(),
        },
        QuestionOption {
            id: "decline".into(),
            label: "no".into(),
        },
    ];
    assert_eq!(
        store
            .ask(1, "rework", "stale?", &opts, &json!({"x": 1}))
            .unwrap(),
        None
    );
    // An answer builds on the goal as it is when the answer commits.
    let q = store
        .ask(2, "rework", "rework?", &opts, &json!({"x": 2}))
        .unwrap()
        .unwrap();
    store.answer(q, "approve", "test").unwrap();
    let (rev, goal) = store.current_goal().unwrap();
    assert_eq!(rev, 3);
    assert_eq!(
        goal.kit_quantity, 2,
        "the correction was not overwritten by the answer"
    );
    assert_eq!(goal.rework_permitted, Some(true));
    assert!(
        store.answer(q, "decline", "test").is_err(),
        "cannot answer twice"
    );

    // The terminal transition refuses if the revision moved on.
    assert!(
        !store
            .finish_run(2, &["run"], R_COMPLETED, "test", None)
            .unwrap()
    );
    assert!(
        store
            .finish_run(3, &["run"], R_PARTIAL, "test", None)
            .unwrap()
    );
    assert_eq!(store.state().unwrap().status, R_PARTIAL);
}

#[test]
fn human_action_due_on_a_crashed_effect_is_redelivered() {
    let state = temp_state("redeliver");
    let mut sc = load("crash-after-effect");
    let on = match &sc.faults[0] {
        revolve::scenario::FaultSpec::CrashAfterEffect { on } => on.clone(),
        other => panic!("{other:?}"),
    };
    sc.human.actions.push(HumanAction::Correct {
        on: Trigger {
            capability: on.capability.clone(),
            nth: on.nth,
        },
        goal: two_kits(),
    });
    let (run, dir) = new_run(&state, &sc, Arm::Revolve, "scripted");
    let resume = || {
        Command::new(exe())
            .args(["resume", "--run", &run, "--quiet", "--state-dir"])
            .arg(&state)
            .status()
            .unwrap()
    };
    assert_eq!(resume().code(), Some(SIMULATED_CRASH_EXIT_CODE));
    assert_eq!(
        RunStore::open(&dir).unwrap().state().unwrap().goal_revision,
        1,
        "the crash happened before delivery"
    );
    assert!(resume().success());
    let store = RunStore::open(&dir).unwrap();
    assert!(
        store
            .events(0, 100_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "human_actions_redelivered")
    );
    assert_eq!(store.state().unwrap().goal_revision, 2);
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(v.kits_truly_complete.len(), 2);
    assert!(
        v.invariant_violations.is_empty(),
        "{:?}",
        v.invariant_violations
    );
}

/// An agent that moves parts correctly but never records consumption, then claims success.
struct Forgetful {
    queue: Vec<(String, Value)>,
}

impl Planner for Forgetful {
    fn label(&self) -> String {
        "test-forgetful".into()
    }
    fn plan(
        &mut self,
        _req: &PlanningRequest,
    ) -> (Result<PlannerResponse, PlannerError>, CallInfo) {
        unreachable!()
    }
    fn next_action(
        &mut self,
        _req: &AgentRequest,
    ) -> (Result<AgentDecision, PlannerError>, CallInfo) {
        let d = if self.queue.is_empty() {
            AgentDecision::Finish(AgentReport {
                status: "completed".into(),
                kits_complete: vec!["tray-1".into()],
                message: "done".into(),
            })
        } else {
            let (cap, args) = self.queue.remove(0);
            AgentDecision::Call {
                capability: cap,
                version: "1".into(),
                arguments: args,
                retry_of: None,
            }
        };
        (Ok(d), CallInfo::default())
    }
}

#[test]
fn evaluator_catches_unrecorded_consumption_and_counts_refused_repeats() {
    let state = temp_state("forgetful");
    let mut sc = load("nominal");
    sc.goal.kit_quantity = 1;
    let (_run, dir) = new_run(&state, &sc, Arm::Baseline, "interactive");
    let mut q = vec![];
    for (o, bin) in [
        ("red-01", "bin-red"),
        ("blue-01", "bin-blue"),
        ("washer-01", "bin-washer"),
        ("washer-02", "bin-washer"),
    ] {
        q.push((
            "workspace.pick".to_string(),
            json!({"object_id": o, "source_id": bin}),
        ));
        q.push((
            "workspace.place".to_string(),
            json!({"object_id": o, "destination_id": "tray-1"}),
        ));
    }
    // A careless retry of an already-applied move under a fresh key: refused by the adapter.
    q.push((
        "workspace.place".to_string(),
        json!({"object_id": "washer-02", "destination_id": "tray-1"}),
    ));
    let status = baseline::drive(
        &dir,
        Box::new(Forgetful { queue: q }),
        None,
        ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, R_COMPLETED);
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(
        v.kits_truly_complete,
        vec!["tray-1"],
        "the kit itself is physically right"
    );
    assert!(
        !v.inventory_correct,
        "record never decremented: {:?}",
        v.inventory_mismatches
    );
    assert!(
        v.invariant_violations
            .iter()
            .any(|x| x.contains("consumption is not recorded")),
        "{:?}",
        v.invariant_violations
    );
    assert_eq!(v.duplicate_attempts_refused, 1);
    assert!(!v.success);
}

#[test]
fn preferred_trays_are_honoured_and_validated() {
    let state = temp_state("prefer");
    let mut sc = load("nominal");
    sc.goal = serde_json::from_value(json!({
        "kit_quantity": 1, "kit_components": {"red_block": 1, "washer": 1}, "preferred_trays": ["tray-4"]
    }))
    .unwrap();
    let (_run, dir) = new_run(&state, &sc, Arm::Revolve, "interactive");
    let status = revolve::harness::drive_in_process(
        &dir,
        ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, R_COMPLETED);
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(v.kits_truly_complete, vec!["tray-4"]);
    let bad: GoalSpec = serde_json::from_value(
        json!({"kit_quantity": 1, "kit_components": {"washer": 1}, "preferred_trays": ["bin-red"]}),
    )
    .unwrap();
    assert!(bad.validate().is_err());
}
