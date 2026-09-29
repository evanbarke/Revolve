//! Planner boundary. A planner receives only the context below and returns a
//! plan proposal (untrusted JSON), a clarification request, or a structured
//! inability. The same boundary serves the direct-tool baseline (`next_action`).
//!
//! Planners never see fault schedules, simulator truth, verifier state or grants
//! beyond the capability descriptions they may use.

pub mod faulty;
pub mod fixed;
pub mod live;
pub mod scripted;

use crate::compiler::{Diagnostic, Limits};
use crate::inventory::InventoryView;
use crate::ir::KitAssignment;
use crate::model::{Classification, GoalSpec, Observation};
use crate::store::QuestionOption;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityDescription {
    pub id: String,
    pub version: String,
    pub summary: String,
    pub arguments: Value,
    pub outputs: Value,
    pub preconditions: String,
    pub effects: String,
    pub plan_step_allowed: bool,
}

pub fn capability_descriptions(include_tools_only: bool) -> Vec<CapabilityDescription> {
    crate::capability::registry()
        .into_iter()
        .filter(|c| include_tools_only || c.plan_step_allowed)
        .map(|c| CapabilityDescription {
            id: c.id.into(),
            version: c.version.into(),
            summary: c.summary.into(),
            arguments: serde_json::to_value(&c.args).unwrap_or(Value::Null),
            outputs: serde_json::to_value(&c.outputs).unwrap_or(Value::Null),
            preconditions: c.preconditions.into(),
            effects: c.effects.into(),
            plan_step_allowed: c.plan_step_allowed,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnsweredQuestion {
    pub kind: String,
    pub context: Value,
    pub answer: String,
}

/// Execution summary: what has already happened, from persisted runtime state.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ExecutionSummary {
    /// Objects whose consumption is recorded in the inventory ledger.
    pub consumed_evidence: Vec<String>,
    /// Kit -> tray assignments of the most recent accepted plan.
    pub previous_kit_assignments: Vec<KitAssignment>,
    pub answered_questions: Vec<AnsweredQuestion>,
    pub actions_succeeded: u32,
    pub actions_failed: u32,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanningRequest {
    pub run_id: String,
    pub goal_id: String,
    pub goal_revision: u32,
    pub goal: GoalSpec,
    pub capabilities: Vec<CapabilityDescription>,
    pub observation: Observation,
    pub classifications: Vec<Classification>,
    pub inventory: InventoryView,
    pub summary: ExecutionSummary,
    /// Diagnostics from this planner's previous rejected proposal in this episode.
    pub feedback: Vec<Diagnostic>,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Clarification {
    /// classification | substitution | rework | other
    pub kind: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub context: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlannerResponse {
    /// Raw, untrusted Action IR JSON.
    Plan(Value),
    Clarify(Clarification),
    Unable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum PlannerError {
    Unavailable { message: String },
    Timeout,
    Malformed { message: String },
    NotConfigured { message: String },
    BudgetExhausted { message: String },
}

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlannerError::Unavailable { message } => write!(f, "planner unavailable: {message}"),
            PlannerError::Timeout => write!(f, "planner timed out"),
            PlannerError::Malformed { message } => {
                write!(f, "malformed planner response: {message}")
            }
            PlannerError::NotConfigured { message } => {
                write!(f, "planner not configured: {message}")
            }
            PlannerError::BudgetExhausted { message } => {
                write!(f, "planner budget exhausted: {message}")
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallInfo {
    pub latency_ms: i64,
    pub transport_attempts: u32,
    /// (input_tokens, output_tokens) when the provider reports usage.
    pub usage: Option<(u64, u64)>,
    pub detail: Option<Value>,
}

// ---------------------------------------------------------------- baseline agent

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRecord {
    pub action_id: i64,
    pub capability: String,
    pub arguments: Value,
    pub op_key: Option<String>,
    /// applied | rejected | denied | invalid | timeout | unavailable | unknown_after_restart | status:<...>
    pub outcome: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRequest {
    pub run_id: String,
    pub goal_revision: u32,
    pub goal: GoalSpec,
    pub tools: Vec<CapabilityDescription>,
    /// The latest observation (the baseline receives one after every tool result).
    pub observation: Option<Observation>,
    pub classifications: Vec<Classification>,
    pub inventory: InventoryView,
    pub history: Vec<ToolRecord>,
    pub answered_questions: Vec<AnsweredQuestion>,
    pub consumed_evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentReport {
    /// completed | partial | blocked | failed
    pub status: String,
    pub kits_complete: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentDecision {
    Call {
        capability: String,
        version: String,
        arguments: Value,
        retry_of: Option<i64>,
    },
    Status {
        op_key: String,
    },
    Ask(Clarification),
    Finish(AgentReport),
}

pub trait Planner {
    /// Label recorded in logs, UI and exports: `scripted`, `fixed-plan`, `live:<provider>/<model>`.
    fn label(&self) -> String;
    fn is_live(&self) -> bool {
        false
    }
    fn plan(&mut self, req: &PlanningRequest) -> (Result<PlannerResponse, PlannerError>, CallInfo);
    fn next_action(
        &mut self,
        _req: &AgentRequest,
    ) -> (Result<AgentDecision, PlannerError>, CallInfo) {
        (
            Err(PlannerError::NotConfigured {
                message: format!("{} does not support direct tool calling", self.label()),
            }),
            CallInfo::default(),
        )
    }
}
