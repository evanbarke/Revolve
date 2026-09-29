//! Fixed-plan adapter: returns a handwritten Action IR file once. It cannot
//! replan; later requests get a structured inability rather than a fake plan.

use super::{CallInfo, Planner, PlannerError, PlannerResponse, PlanningRequest};
use serde_json::Value;

pub struct FixedPlanner {
    plan: Value,
    used: bool,
}

impl FixedPlanner {
    pub fn new(plan: Value) -> FixedPlanner {
        FixedPlanner { plan, used: false }
    }
}

impl Planner for FixedPlanner {
    fn label(&self) -> String {
        "fixed-plan".into()
    }

    fn plan(&mut self, _req: &PlanningRequest) -> (Result<PlannerResponse, PlannerError>, CallInfo) {
        let info = CallInfo { latency_ms: 0, transport_attempts: 1, usage: None, detail: None };
        if self.used {
            return (
                Ok(PlannerResponse::Unable {
                    reason: "fixed-plan mode has no planner to revise the plan; start a run with --planner scripted or a configured live planner".into(),
                }),
                info,
            );
        }
        self.used = true;
        (Ok(PlannerResponse::Plan(self.plan.clone())), info)
    }
}
