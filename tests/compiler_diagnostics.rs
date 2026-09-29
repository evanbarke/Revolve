//! The example plans in examples/plans each exercise one diagnostic category;
//! rejected plans must have no effects.

mod common;

use common::*;
use revolve::compiler::{self, CompileContext, DiagCategory, Limits};
use revolve::harness::{Arm, PlannerChoice};
use revolve::inventory::InventoryView;
use revolve::policy::Policy;
use revolve::sim::SimWorld;
use revolve::store::*;
use serde_json::Value;

fn compile_file(name: &str) -> Result<compiler::CompiledPlan, Vec<compiler::Diagnostic>> {
    let state = temp_state("compile");
    let sc = load("nominal");
    let mut w = SimWorld::create(&state.join("world.db"), &sc).unwrap();
    let obs = w.observe().unwrap();
    let text = std::fs::read_to_string(format!("examples/plans/{name}.json")).unwrap();
    let raw: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return Err(vec![compiler::Diagnostic {
                category: DiagCategory::Syntax,
                step_id: None,
                path: "$".into(),
                message: e.to_string(),
            }]);
        }
    };
    let inv = InventoryView {
        on_hand: sc.inventory_record(),
        ..Default::default()
    };
    let policy = Policy::default();
    let p = compiler::parse_proposal(&raw)?;
    compiler::compile(
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
}

fn first_category(name: &str) -> DiagCategory {
    let d = compile_file(name).expect_err(name);
    assert!(!d.is_empty());
    d[0].category
}

#[test]
fn valid_plan_compiles_in_dependency_order() {
    let cp = compile_file("kit-assembly").unwrap();
    assert_eq!(cp.steps.len(), 38);
    assert_eq!(cp.steps[0].step_id, "reserve-stock");
    assert_eq!(
        cp.predicted_complete_trays,
        vec!["tray-1", "tray-2", "tray-3"]
    );
}

#[test]
fn each_invalid_plan_reports_its_category() {
    assert_eq!(first_category("invalid-syntax"), DiagCategory::Syntax);
    assert_eq!(
        first_category("invalid-unknown-capability"),
        DiagCategory::Schema
    );
    assert_eq!(first_category("invalid-cycle"), DiagCategory::Structure);
    assert_eq!(
        first_category("invalid-missing-reference"),
        DiagCategory::Structure
    );
    assert_eq!(
        first_category("invalid-resource-conflict"),
        DiagCategory::Structure
    );
    assert_eq!(
        first_category("invalid-stale-revision"),
        DiagCategory::Stale
    );
    assert_eq!(
        first_category("invalid-unauthorized-destination"),
        DiagCategory::Authorization
    );
    assert_eq!(
        first_category("invalid-ungranted-permission"),
        DiagCategory::Authorization
    );
    assert_eq!(
        first_category("invalid-runtime-owned-field"),
        DiagCategory::Authorization
    );
    assert_eq!(first_category("invalid-over-budget"), DiagCategory::Budget);
    assert_eq!(
        first_category("invalid-infeasible-object"),
        DiagCategory::Feasibility
    );
    assert_eq!(
        first_category("invalid-false-claim"),
        DiagCategory::Feasibility
    );
}

#[test]
fn diagnostics_point_at_the_offending_step_and_field() {
    let d = compile_file("invalid-unauthorized-destination").unwrap_err();
    assert_eq!(d[0].step_id.as_deref(), Some("place-red-01-in-tray-1"));
    assert!(d[0].path.contains("steps[2]"));
    let d = compile_file("invalid-missing-reference").unwrap_err();
    assert!(
        d[0].path.ends_with("arguments.reservation_id"),
        "{}",
        d[0].path
    );
}

#[test]
fn rejected_plans_have_no_effects() {
    let state = temp_state("reject");
    let sc = load("nominal");
    let (_run, dir) = revolve::harness::create_run(
        &state,
        &sc,
        Arm::Revolve,
        &PlannerChoice::Fixed("examples/plans/invalid-unauthorized-destination.json".into()),
        "interactive",
    )
    .unwrap();
    let status = revolve::harness::drive_in_process(
        &dir,
        revolve::runtime::ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, R_FAILED);
    let store = RunStore::open(&dir).unwrap();
    assert!(
        store
            .actions()
            .unwrap()
            .iter()
            .all(|a| a.op_key.is_none() || a.origin == "runtime"),
        "no plan action dispatched"
    );
    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    assert!(truth.receipts.is_empty(), "no world effects");
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(v.unauthorized_attempted, 1);
    assert_eq!(v.unauthorized_executed, 0);
}
