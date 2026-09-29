//! The evaluator must catch false completion claims, whatever the system says.

mod common;

use common::*;
use revolve::baseline;
use revolve::harness::Arm;
use revolve::planner::{
    AgentDecision, AgentReport, AgentRequest, CallInfo, Planner, PlannerError, PlannerResponse,
    PlanningRequest,
};
use revolve::runtime::ExecOptions;

/// An agent that claims success without doing anything.
struct Braggart;

impl Planner for Braggart {
    fn label(&self) -> String {
        "test-braggart".into()
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
        (
            Ok(AgentDecision::Finish(AgentReport {
                status: "completed".into(),
                kits_complete: vec!["tray-1".into(), "tray-2".into(), "tray-3".into()],
                message: "all done".into(),
            })),
            CallInfo::default(),
        )
    }
}

#[test]
fn false_success_claims_are_detected() {
    let state = temp_state("braggart");
    let (_run, dir) = new_run(&state, &load("nominal"), Arm::Baseline, "interactive");
    let status = baseline::drive(
        &dir,
        Box::new(Braggart),
        None,
        ExecOptions {
            quiet: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(status, "completed", "the agent's own claim");
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert_eq!(v.kits_truly_complete.len(), 0);
    assert_eq!(
        v.false_success_claims, 4,
        "three false kit claims plus a false completed status"
    );
    assert!(!v.success);
}

#[test]
fn every_demo_scenario_meets_its_expectation_on_both_arms() {
    let state = temp_state("demo-suite");
    let dir = std::path::Path::new("scenarios/demo");
    for f in revolve::scenario::list_dir(dir) {
        let sc = revolve::scenario::Scenario::load(&f).unwrap();
        for arm in [Arm::Revolve, Arm::Baseline] {
            let (run, rdir) = new_run(&state, &sc, arm, "scripted");
            revolve::harness::supervise_with(&exe(), &state, &run, 0, true).unwrap();
            let v = revolve::eval::evaluate_run(&rdir).unwrap();
            assert!(
                v.success,
                "{} / {}: {}",
                sc.id,
                arm.as_str(),
                revolve::eval::summarize_verdict(&v)
            );
        }
    }
}
