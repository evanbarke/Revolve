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
            Condition::ObjectAt {
                object_id,
                location_id,
            } => format!("object_at({object_id}, {location_id})"),
            Condition::GripperEmpty => "gripper_empty".into(),
            Condition::GripperHolds { object_id } => format!("gripper_holds({object_id})"),
            Condition::LocationAvailable { location_id } => {
                format!("location_available({location_id})")
            }
            Condition::QuantityAtLeast {
                location_id,
                part_kind,
                quantity,
            } => {
                format!("quantity_at_least({location_id}, {part_kind}, {quantity})")
            }
            Condition::GoalRevisionEquals { revision } => {
                format!("goal_revision_equals({revision})")
            }
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
        if let Some(obj) = v.as_object()
            && let Some(r) = obj.get("$ref")
        {
            if obj.len() != 1 {
                return Err("an output reference object must contain only `$ref`".into());
            }
            let s = r
                .as_str()
                .ok_or("`$ref` must be a string of the form `step_id.output`")?;
            let (step, out) = s
                .rsplit_once('.')
                .ok_or("`$ref` must be of the form `step_id.output`")?;
            if step.is_empty() || out.is_empty() {
                return Err("`$ref` must be of the form `step_id.output`".into());
            }
            return Ok(ArgValue::Ref {
                step_id: step.into(),
                output: out.into(),
            });
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

/// The plan format handed to every planner with each request: rules plus a small,
/// valid-shaped example (values are illustrative; bind ids/revisions from the request).
pub fn plan_format() -> serde_json::Value {
    serde_json::json!({
        "notes": [
            "Return one JSON object with exactly the fields shown in `example` (kit_assignments and expected_outcome may be empty; step fields preconditions, postconditions, on_unknown_outcome, retry and note are optional).",
            "`capabilities` is an OBJECT mapping capability id to version, e.g. {\"workspace.pick\": \"1\"}; list every capability the steps use, and repeat the ids in `requested_permissions` (an array of strings).",
            "Copy goal_id, goal_revision and basis.world_revision from the request (goal_id, goal_revision, observation.world_revision).",
            "Each step: step_id (unique), capability, capability_version, depends_on (step ids), arguments (exactly the capability's argument names).",
            "Reference another step's output with {\"$ref\": \"<step_id>.<output>\"}; the referenced step must be a (transitive) dependency.",
            "Condition kinds: object_at{object_id,location_id}, gripper_empty, gripper_holds{object_id}, location_available{location_id}, quantity_at_least{location_id,part_kind,quantity}, goal_revision_equals{revision} (preconditions only).",
            "inventory.consume and inventory.return record exactly one unit (quantity 1) with the object as evidence."
        ],
        "example": {
            "schema_version": ACTION_IR_VERSION,
            "plan_id": "example-plan",
            "goal_id": "goal-example",
            "goal_revision": 1,
            "basis": {"world_revision": 0},
            "capabilities": {"inventory.reserve": "1", "workspace.pick": "1", "workspace.place": "1", "inventory.consume": "1", "inventory.release": "1"},
            "requested_permissions": ["inventory.reserve", "workspace.pick", "workspace.place", "inventory.consume", "inventory.release"],
            "budgets": {"max_actions": 120, "max_planner_rounds": 12, "max_elapsed_ms": 600000},
            "kit_assignments": [{"kit": 1, "tray_id": "tray-1"}],
            "explanation": "Reserve one red block, place it in tray-1, record its consumption, release the rest.",
            "expected_outcome": {"kits_complete": [], "kits_incomplete": [{"kit": 1, "tray_id": "tray-1", "missing": {"washer": 2}, "reason": "example only"}]},
            "steps": [
                {"step_id": "reserve", "capability": "inventory.reserve", "capability_version": "1", "depends_on": [],
                 "arguments": {"counts": {"red_block": 1}}},
                {"step_id": "pick-red-01", "capability": "workspace.pick", "capability_version": "1", "depends_on": ["reserve"],
                 "arguments": {"object_id": "red-01", "source_id": "bin-red"},
                 "preconditions": [{"kind": "gripper_empty"}, {"kind": "object_at", "object_id": "red-01", "location_id": "bin-red"}],
                 "postconditions": [{"kind": "gripper_holds", "object_id": "red-01"}]},
                {"step_id": "place-red-01", "capability": "workspace.place", "capability_version": "1", "depends_on": ["pick-red-01"],
                 "arguments": {"object_id": {"$ref": "pick-red-01.object_id"}, "destination_id": "tray-1"},
                 "postconditions": [{"kind": "object_at", "object_id": "red-01", "location_id": "tray-1"}]},
                {"step_id": "consume-red-01", "capability": "inventory.consume", "capability_version": "1", "depends_on": ["place-red-01"],
                 "arguments": {"reservation_id": {"$ref": "reserve.reservation_id"}, "part_kind": "red_block", "quantity": 1, "evidence_object_id": "red-01"}},
                {"step_id": "release", "capability": "inventory.release", "capability_version": "1", "depends_on": ["consume-red-01"],
                 "arguments": {"reservation_id": {"$ref": "reserve.reservation_id"}}}
            ]
        }
    })
}

/// The tool-call format handed to the direct-tool baseline with each request.
pub fn call_format() -> serde_json::Value {
    serde_json::json!({
        "notes": [
            "Choose one decision per turn. For a tool call, `capability` is a tool id and `arguments_json` is a JSON object string with exactly that tool's argument names.",
            "inventory.consume / inventory.return record exactly one unit (quantity 1) citing the object as evidence.",
            "Use `status` with an op_key from history to learn the outcome of a call that timed out or was interrupted; retry an earlier call under the same key with retry_of_action_id (otherwise -1)."
        ],
        "examples": [
            {"decision": "call", "capability": "workspace.pick", "arguments_json": "{\"object_id\":\"red-01\",\"source_id\":\"bin-red\"}", "retry_of_action_id": -1},
            {"decision": "call", "capability": "inventory.consume", "arguments_json": "{\"reservation_id\":\"res-abc\",\"part_kind\":\"red_block\",\"quantity\":1,\"evidence_object_id\":\"red-01\"}", "retry_of_action_id": -1}
        ]
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn plan_format_example_is_a_well_formed_proposal() {
        let ex = super::plan_format()["example"].clone();
        let p = crate::compiler::parse_proposal(&ex)
            .expect("the example handed to planners must parse");
        assert_eq!(p.steps.len(), 5);
    }
}
