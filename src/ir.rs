//! Action IR v1: the planner-proposed plan representation.
//!
//! Semantics (see docs/action-ir.md):
//! - A plan is a DAG of steps; dispatch is sequential in a deterministic
//!   topological order (ties broken by list order).
//! - Steps that touch the same exclusive resource (gripper, an object, a
//!   reservation) must be ordered by an explicit dependency path.
//! - Argument values are literals or typed output references `{"$ref": "step.output"}`;
//!   a reference must point at a transitive dependency and match the argument type.
//! - Preconditions are evaluated against a fresh observation immediately before
//!   dispatch; postconditions are verified by observation after an acknowledged effect.
//! - Everything here is an untrusted *proposal*. Runtime-owned facts (grants,
//!   operation keys, receipts, timestamps, identity) live in `PlanRecord` and are
//!   rejected if a planner tries to supply them.

use crate::model::KindCounts;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ACTION_IR_VERSION: &str = "revolve.action-ir/v1";

/// Field names that are owned by the runtime. A proposal containing any of them
/// is rejected with a specific diagnostic rather than a generic "unknown field".
pub const RUNTIME_OWNED_FIELDS: &[&str] = &[
    "granted_permissions",
    "authorized_permissions",
    "grants",
    "op_key",
    "operation_key",
    "idempotency_key",
    "committed_effect_id",
    "effect_id",
    "receipt",
    "receipt_id",
    "authenticated_identity",
    "principal",
    "approved_by",
    "audit",
    "audit_timestamp",
    "status",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanBasis {
    /// World revision of the observation snapshot the planner was given.
    pub world_revision: u64,
    /// Maximum age of the observation used to check each step's preconditions.
    #[serde(default)]
    pub max_observation_age_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub max_actions: u32,
    pub max_planner_rounds: u32,
    pub max_elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitAssignment {
    pub kit: u32,
    pub tray_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompleteKit {
    pub kit: u32,
    #[serde(default)]
    pub tray_id: Option<String>,
    pub missing: KindCounts,
    pub reason: String,
}

/// The planner's claim about what the plan will achieve. The compiler checks it
/// against the plan's abstract execution; the verifier checks reality afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExpectedOutcome {
    #[serde(default)]
    pub kits_complete: Vec<String>,
    #[serde(default)]
    pub kits_incomplete: Vec<IncompleteKit>,
}

/// Plan-level fields (everything except `steps`, which are parsed one by one so
/// diagnostics can point at the offending step).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanHeader {
    pub schema_version: String,
    pub plan_id: String,
    pub goal_id: String,
    pub goal_revision: u32,
    pub basis: PlanBasis,
    /// Capability id -> requested contract version.
    pub capabilities: BTreeMap<String, String>,
    /// Capability ids the plan asks to use. Compared with externally held grants.
    pub requested_permissions: Vec<String>,
    pub budgets: Budgets,
    #[serde(default)]
    pub kit_assignments: Vec<KitAssignment>,
    pub explanation: String,
    #[serde(default)]
    pub expected_outcome: ExpectedOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnknownOutcomePolicy {
    /// Establish the outcome (status lookup, then observation) before any retry.
    #[default]
    ReconcileBeforeRetry,
    /// Never retry automatically; block for a human once the outcome is unknown.
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Total dispatch attempts under the same operation key (1 = no retry).
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_attempts: 2 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    ObjectAt {
        object_id: String,
        location_id: String,
    },
    GripperEmpty,
    GripperHolds {
        object_id: String,
    },
    #[serde(alias = "destination_available")]
    LocationAvailable {
        #[serde(alias = "destination_id")]
        location_id: String,
    },
    QuantityAtLeast {
        location_id: String,
        part_kind: String,
        quantity: u32,
    },
    GoalRevisionEquals {
        revision: u32,
    },
}

impl Condition {
    pub fn describe(&self) -> String {
        match self {
            Condition::ObjectAt { object_id, location_id } => format!("object_at({object_id}, {location_id})"),
            Condition::GripperEmpty => "gripper_empty".into(),
            Condition::GripperHolds { object_id } => format!("gripper_holds({object_id})"),
            Condition::LocationAvailable { location_id } => format!("location_available({location_id})"),
            Condition::QuantityAtLeast { location_id, part_kind, quantity } => {
                format!("quantity_at_least({location_id}, {part_kind}, {quantity})")
            }
            Condition::GoalRevisionEquals { revision } => format!("goal_revision_equals({revision})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub step_id: String,
    pub capability: String,
    pub capability_version: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Raw argument values; each is a literal or `{"$ref": "step_id.output"}`.
    #[serde(default)]
    pub arguments: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub preconditions: Vec<Condition>,
    #[serde(default)]
    pub postconditions: Vec<Condition>,
    #[serde(default)]
    pub on_unknown_outcome: UnknownOutcomePolicy,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default)]
    pub note: Option<String>,
}

/// A parsed argument value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    Literal(serde_json::Value),
    Ref { step_id: String, output: String },
}

impl ArgValue {
    pub fn parse(v: &serde_json::Value) -> Result<ArgValue, String> {
        if let Some(obj) = v.as_object() {
            if let Some(r) = obj.get("$ref") {
                if obj.len() != 1 {
                    return Err("an output reference object must contain only `$ref`".into());
                }
                let s = r.as_str().ok_or("`$ref` must be a string of the form `step_id.output`")?;
                let (step, out) = s
                    .rsplit_once('.')
                    .ok_or("`$ref` must be of the form `step_id.output`")?;
                if step.is_empty() || out.is_empty() {
                    return Err("`$ref` must be of the form `step_id.output`".into());
                }
                return Ok(ArgValue::Ref { step_id: step.into(), output: out.into() });
            }
        }
        Ok(ArgValue::Literal(v.clone()))
    }
}

/// A fully parsed proposal. Construct only through `compiler::parse_proposal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProposal {
    #[serde(flatten)]
    pub header: PlanHeader,
    pub steps: Vec<Step>,
}

pub fn make_ref(step_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!({ "$ref": format!("{step_id}.{output}") })
}
