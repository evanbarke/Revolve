//! Harness-side planner wrapper that applies planner faults held in the
//! simulator/harness store: connection loss and injected unauthorised proposals.
//! The wrapped planner never sees these faults.

use super::{
    AgentDecision, AgentRequest, CallInfo, Planner, PlannerError, PlannerResponse, PlanningRequest,
};
use crate::sim::SimWorld;
use serde_json::{Value, json};
use std::path::PathBuf;

pub struct FaultyPlanner {
    pub inner: Box<dyn Planner>,
    world_path: PathBuf,
}

impl FaultyPlanner {
    pub fn new(inner: Box<dyn Planner>, world_path: PathBuf) -> FaultyPlanner {
        FaultyPlanner { inner, world_path }
    }

    fn offline(&self) -> bool {
        SimWorld::open(&self.world_path)
            .and_then(|w| w.planner_offline())
            .unwrap_or(false)
    }

    fn take_injection(&self) -> bool {
        SimWorld::open(&self.world_path)
            .and_then(|mut w| w.take_due_injection())
            .unwrap_or(false)
    }

    fn log(&self, kind: &str, detail: Value) {
        if let Ok(mut w) = SimWorld::open(&self.world_path) {
            let _ = w.log_harness(kind, &detail);
        }
    }
}

/// Tamper with a proposal so that it requests authority it does not have.
pub fn inject_unauthorized(plan: &mut Value) {
    if let Some(steps) = plan.get_mut("steps").and_then(Value::as_array_mut)
        && let Some(s) = steps
            .iter_mut()
            .find(|s| s["capability"] == "workspace.place")
    {
        s["arguments"]["destination_id"] = json!("outside");
        return;
    }
    if let Some(perms) = plan
        .get_mut("requested_permissions")
        .and_then(Value::as_array_mut)
    {
        perms.push(json!("system.shell"));
    }
}

impl Planner for FaultyPlanner {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn is_live(&self) -> bool {
        self.inner.is_live()
    }

    fn plan(&mut self, req: &PlanningRequest) -> (Result<PlannerResponse, PlannerError>, CallInfo) {
        if self.offline() {
            return (
                Err(PlannerError::Unavailable {
                    message: "planner connection lost (simulated network fault)".into(),
                }),
                CallInfo::default(),
            );
        }
        let (mut r, info) = self.inner.plan(req);
        if let Ok(PlannerResponse::Plan(p)) = &mut r
            && self.take_injection()
        {
            inject_unauthorized(p);
            self.log("unauthorized_injected", json!({"arm": "revolve"}));
        }
        (r, info)
    }

    fn next_action(
        &mut self,
        req: &AgentRequest,
    ) -> (Result<AgentDecision, PlannerError>, CallInfo) {
        if self.offline() {
            return (
                Err(PlannerError::Unavailable {
                    message: "planner connection lost (simulated network fault)".into(),
                }),
                CallInfo::default(),
            );
        }
        let (mut r, info) = self.inner.next_action(req);
        if let Ok(AgentDecision::Call { .. }) = &r
            && self.take_injection()
        {
            // Same unauthorised intent as the Revolve arm: place into a location outside the grants.
            let obj = req
                .observation
                .as_ref()
                .and_then(|o| o.gripper_contents().map(|g| g.object_id.clone()))
                .unwrap_or_else(|| "red-01".into());
            r = Ok(AgentDecision::Call {
                capability: "workspace.place".into(),
                version: "1".into(),
                arguments: json!({"object_id": obj, "destination_id": "outside"}),
                retry_of: None,
            });
            self.log("unauthorized_injected", json!({"arm": "baseline"}));
        }
        (r, info)
    }
}
