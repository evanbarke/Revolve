//! Capability registry: versioned contracts that the compiler, runtime, policy,
//! planner prompts and documentation all read from. Retry/reconciliation
//! decisions are taken from these declarations, never from operation names.

use crate::model::{KindCounts, is_known_kind};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    ObjectId,
    LocationId,
    PartKind,
    Quantity,
    KindCounts,
    ReservationId,
    WorldRevision,
    OpKey,
}

impl ArgType {
    /// Structural type check of a literal (scope/authorisation is policy's job).
    pub fn check(&self, v: &Value) -> Result<(), String> {
        let id_like = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        };
        match self {
            ArgType::ObjectId | ArgType::LocationId | ArgType::ReservationId | ArgType::OpKey => {
                match v.as_str() {
                    Some(s) if id_like(s) => Ok(()),
                    Some(s) => Err(format!(
                        "`{s}` is not a valid identifier (lowercase ascii, digits, '-', '_'; max 64)"
                    )),
                    None => Err(format!("expected a string identifier, got {v}")),
                }
            }
            ArgType::PartKind => match v.as_str() {
                Some(s) if is_known_kind(s) => Ok(()),
                Some(s) => Err(format!("unknown part kind `{s}`")),
                None => Err(format!("expected a part kind string, got {v}")),
            },
            ArgType::Quantity | ArgType::WorldRevision => match v.as_u64() {
                Some(_) => Ok(()),
                None => Err(format!("expected a non-negative integer, got {v}")),
            },
            ArgType::KindCounts => {
                let obj = v
                    .as_object()
                    .ok_or_else(|| format!("expected an object of part_kind -> count, got {v}"))?;
                if obj.is_empty() {
                    return Err("counts must not be empty".into());
                }
                for (k, n) in obj {
                    if !is_known_kind(k) {
                        return Err(format!("unknown part kind `{k}`"));
                    }
                    if n.as_u64().is_none() {
                        return Err(format!("count for `{k}` must be a non-negative integer"));
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArgSpec {
    pub name: &'static str,
    pub ty: ArgType,
    pub description: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputSpec {
    pub name: &'static str,
    pub ty: ArgType,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// No effects; safe to repeat.
    ReadOnly,
    /// Effects on project-owned local state in a single SQLite transaction; the
    /// outcome can always be established by status lookup against the same store.
    LocalTransactional,
    /// Effects on the (simulated) physical world. A timeout or crash can leave the
    /// outcome unknown until established by status lookup or observation.
    PhysicalUncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Immediate,
    /// An acknowledgement is not proof: the effect must be confirmed by observation.
    AcknowledgedThenObserve,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contract {
    pub id: &'static str,
    pub version: &'static str,
    pub summary: &'static str,
    pub args: Vec<ArgSpec>,
    pub outputs: Vec<OutputSpec>,
    pub resources: &'static str,
    pub preconditions: &'static str,
    pub effects: &'static str,
    pub permission: &'static str,
    pub effect_class: EffectClass,
    pub idempotency_key: bool,
    pub status_lookup: bool,
    pub cancellation: bool,
    pub reconciliation: &'static str,
    pub timeout_ms: u64,
    pub error_categories: &'static [&'static str],
    pub completion: Completion,
    /// Whether a plan may contain this capability as a step.
    pub plan_step_allowed: bool,
}

impl Contract {
    pub fn arg(&self, name: &str) -> Option<&ArgSpec> {
        self.args.iter().find(|a| a.name == name)
    }
    pub fn output(&self, name: &str) -> Option<&OutputSpec> {
        self.outputs.iter().find(|o| o.name == name)
    }
    pub fn is_effectful(&self) -> bool {
        self.effect_class != EffectClass::ReadOnly
    }
    /// Safe to re-dispatch after an unknown outcome *without* evidence, because
    /// the adapter guarantees at most one effect per operation key.
    pub fn safe_to_redispatch_same_key(&self) -> bool {
        self.idempotency_key
    }
}

const PHYS_ERRORS: &[&str] = &[
    "precondition_failed",
    "key_conflict",
    "unavailable",
    "timeout",
    "malformed",
];
const INV_ERRORS: &[&str] = &[
    "insufficient_stock",
    "unknown_reservation",
    "key_conflict",
    "evidence_mismatch",
    "malformed",
];

pub fn registry() -> Vec<Contract> {
    vec![
        Contract {
            id: "workspace.observe",
            version: "1",
            summary: "Observe the workspace: locations, availability, visible objects and their observed classification.",
            args: vec![],
            outputs: vec![OutputSpec {
                name: "world_revision",
                ty: ArgType::WorldRevision,
                description: "revision of the snapshot",
            }],
            resources: "none",
            preconditions: "none",
            effects: "none (read-only). Some locations may be reported as withheld when evidence is unavailable.",
            permission: "workspace.observe",
            effect_class: EffectClass::ReadOnly,
            idempotency_key: false,
            status_lookup: false,
            cancellation: false,
            reconciliation: "not applicable",
            timeout_ms: 2_000,
            error_categories: &["unavailable"],
            completion: Completion::Immediate,
            plan_step_allowed: false,
        },
        Contract {
            id: "workspace.pick",
            version: "1",
            summary: "Move one object from a bin or tray into the (single-object) gripper.",
            args: vec![
                ArgSpec {
                    name: "object_id",
                    ty: ArgType::ObjectId,
                    description: "object to pick",
                },
                ArgSpec {
                    name: "source_id",
                    ty: ArgType::LocationId,
                    description: "bin or tray currently holding it",
                },
            ],
            outputs: vec![OutputSpec {
                name: "object_id",
                ty: ArgType::ObjectId,
                description: "object now held",
            }],
            resources: "gripper (exclusive), the object (exclusive), source location",
            preconditions: "gripper_empty; object_at(object, source); location_available(source)",
            effects: "object_at(object, gripper)",
            permission: "workspace.pick (source must be a granted bin/tray)",
            effect_class: EffectClass::PhysicalUncertain,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key; otherwise observe gripper and source",
            timeout_ms: 5_000,
            error_categories: PHYS_ERRORS,
            completion: Completion::AcknowledgedThenObserve,
            plan_step_allowed: true,
        },
        Contract {
            id: "workspace.place",
            version: "1",
            summary: "Place the held object into a tray, or return it to the bin that accepts its kind.",
            args: vec![
                ArgSpec {
                    name: "object_id",
                    ty: ArgType::ObjectId,
                    description: "object currently held",
                },
                ArgSpec {
                    name: "destination_id",
                    ty: ArgType::LocationId,
                    description: "tray, or the bin for the object's kind",
                },
            ],
            outputs: vec![OutputSpec {
                name: "object_id",
                ty: ArgType::ObjectId,
                description: "object placed",
            }],
            resources: "gripper (exclusive), the object (exclusive), destination location",
            preconditions: "gripper_holds(object); location_available(destination); bin destinations accept only their kind",
            effects: "object_at(object, destination); gripper_empty",
            permission: "workspace.place (destination must be a granted bin/tray)",
            effect_class: EffectClass::PhysicalUncertain,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key; otherwise observe gripper and destination",
            timeout_ms: 5_000,
            error_categories: PHYS_ERRORS,
            completion: Completion::AcknowledgedThenObserve,
            plan_step_allowed: true,
        },
        Contract {
            id: "inventory.reserve",
            version: "1",
            summary: "Reserve stock counts in the inventory record for this run.",
            args: vec![ArgSpec {
                name: "counts",
                ty: ArgType::KindCounts,
                description: "part_kind -> quantity to reserve",
            }],
            outputs: vec![OutputSpec {
                name: "reservation_id",
                ty: ArgType::ReservationId,
                description: "id of the new reservation",
            }],
            resources: "inventory record",
            preconditions: "on_hand - reserved >= counts for every kind",
            effects: "reserved += counts; a reservation is opened",
            permission: "inventory.reserve (per-kind and total quantity bounds)",
            effect_class: EffectClass::LocalTransactional,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key (same local store)",
            timeout_ms: 1_000,
            error_categories: INV_ERRORS,
            completion: Completion::Immediate,
            plan_step_allowed: true,
        },
        Contract {
            id: "inventory.consume",
            version: "1",
            summary: "Record verified consumption of reserved stock, citing the object observed in a kit tray.",
            args: vec![
                ArgSpec {
                    name: "reservation_id",
                    ty: ArgType::ReservationId,
                    description: "reservation to draw from",
                },
                ArgSpec {
                    name: "part_kind",
                    ty: ArgType::PartKind,
                    description: "kind consumed",
                },
                ArgSpec {
                    name: "quantity",
                    ty: ArgType::Quantity,
                    description: "units consumed",
                },
                ArgSpec {
                    name: "evidence_object_id",
                    ty: ArgType::ObjectId,
                    description: "object observed in a kit tray",
                },
            ],
            outputs: vec![],
            resources: "inventory record, the reservation (exclusive)",
            preconditions: "reservation open with remaining >= quantity; evidence object observed in a tray",
            effects: "on_hand -= quantity; reserved -= quantity; reservation.remaining -= quantity",
            permission: "inventory.consume",
            effect_class: EffectClass::LocalTransactional,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key (same local store)",
            timeout_ms: 1_000,
            error_categories: INV_ERRORS,
            completion: Completion::Immediate,
            plan_step_allowed: true,
        },
        Contract {
            id: "inventory.release",
            version: "1",
            summary: "Release whatever remains of a reservation.",
            args: vec![ArgSpec {
                name: "reservation_id",
                ty: ArgType::ReservationId,
                description: "reservation to close",
            }],
            outputs: vec![OutputSpec {
                name: "released",
                ty: ArgType::KindCounts,
                description: "counts released",
            }],
            resources: "inventory record, the reservation (exclusive)",
            preconditions: "reservation exists",
            effects: "reserved -= remaining; reservation closed",
            permission: "inventory.release",
            effect_class: EffectClass::LocalTransactional,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key (same local store)",
            timeout_ms: 1_000,
            error_categories: INV_ERRORS,
            completion: Completion::Immediate,
            plan_step_allowed: true,
        },
        Contract {
            id: "inventory.return",
            version: "1",
            summary: "Record that a previously consumed part was returned to its stock bin (rework).",
            args: vec![
                ArgSpec {
                    name: "part_kind",
                    ty: ArgType::PartKind,
                    description: "kind returned",
                },
                ArgSpec {
                    name: "quantity",
                    ty: ArgType::Quantity,
                    description: "units returned",
                },
                ArgSpec {
                    name: "evidence_object_id",
                    ty: ArgType::ObjectId,
                    description: "object observed back in its bin",
                },
            ],
            outputs: vec![],
            resources: "inventory record",
            preconditions: "evidence object was previously consumed and is observed in its stock bin",
            effects: "on_hand += quantity",
            permission: "inventory.return",
            effect_class: EffectClass::LocalTransactional,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key (same local store)",
            timeout_ms: 1_000,
            error_categories: INV_ERRORS,
            completion: Completion::Immediate,
            plan_step_allowed: true,
        },
        Contract {
            id: "inventory.adjust_count",
            version: "1",
            summary: "Correct the inventory record to an observed stock count, citing the observation.",
            args: vec![
                ArgSpec {
                    name: "part_kind",
                    ty: ArgType::PartKind,
                    description: "kind to correct",
                },
                ArgSpec {
                    name: "observed_count",
                    ty: ArgType::Quantity,
                    description: "count seen in the stock bin",
                },
                ArgSpec {
                    name: "observation_revision",
                    ty: ArgType::WorldRevision,
                    description: "world revision of the cited observation",
                },
            ],
            outputs: vec![],
            resources: "inventory record",
            preconditions: "the cited observation was recorded in this run and shows observed_count; observed_count >= reserved",
            effects: "on_hand := observed_count (ledger entry with provenance)",
            permission: "inventory.adjust_count (evidence-bound)",
            effect_class: EffectClass::LocalTransactional,
            idempotency_key: true,
            status_lookup: true,
            cancellation: false,
            reconciliation: "status lookup by operation key (same local store)",
            timeout_ms: 1_000,
            error_categories: INV_ERRORS,
            completion: Completion::Immediate,
            plan_step_allowed: false,
        },
        Contract {
            id: "ops.status",
            version: "1",
            summary: "Look up the recorded outcome of an operation key (applied, rejected, not found, or unavailable).",
            args: vec![ArgSpec {
                name: "op_key",
                ty: ArgType::OpKey,
                description: "operation key to look up",
            }],
            outputs: vec![],
            resources: "none",
            preconditions: "none",
            effects: "none (read-only)",
            permission: "ops.status",
            effect_class: EffectClass::ReadOnly,
            idempotency_key: false,
            status_lookup: false,
            cancellation: false,
            reconciliation: "not applicable",
            timeout_ms: 2_000,
            error_categories: &["unavailable"],
            completion: Completion::Immediate,
            plan_step_allowed: false,
        },
    ]
}

pub fn lookup(id: &str) -> Option<Contract> {
    registry().into_iter().find(|c| c.id == id)
}

/// Exclusive resources a step with these (resolved or literal) arguments holds.
/// References are rendered as `ref:<step>.<output>` so two steps using the same
/// referenced reservation still conflict.
pub fn exclusive_resources(capability: &str, args: &serde_json::Map<String, Value>) -> Vec<String> {
    let s = |k: &str| -> String {
        match args.get(k) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Object(o)) => o
                .get("$ref")
                .and_then(Value::as_str)
                .map(|r| format!("ref:{r}"))
                .unwrap_or_default(),
            _ => String::new(),
        }
    };
    match capability {
        "workspace.pick" | "workspace.place" => {
            vec!["gripper".into(), format!("object:{}", s("object_id"))]
        }
        "inventory.consume" | "inventory.release" => {
            vec![format!("reservation:{}", s("reservation_id"))]
        }
        _ => vec![],
    }
}

/// Canonical text of a logical effect (capability + resolved arguments). Two
/// actions with the same descriptor would produce the same world change.
pub fn logical_descriptor(capability: &str, resolved_args: &Value) -> String {
    format!("{capability}{}", crate::util::canonical_json(resolved_args))
}

pub fn counts_from_value(v: &Value) -> KindCounts {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, n)| n.as_u64().map(|n| (k.clone(), n as u32)))
                .collect()
        })
        .unwrap_or_default()
}
