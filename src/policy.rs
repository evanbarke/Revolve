//! Authorisation policy. Grants are held by the runtime (never read from a plan)
//! and every dispatch — from Revolve's executor or the direct-tool baseline — is
//! checked here. Compile-time checks use the same rules on literal arguments.

use crate::capability::counts_from_value;
use crate::model::is_known_kind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Capability id -> granted contract version.
    pub grants: BTreeMap<String, String>,
    /// Location id prefixes that pick may use as a source.
    pub pick_sources: Vec<String>,
    /// Location id prefixes that place may use as a destination.
    pub place_destinations: Vec<String>,
    pub max_reserve_per_kind: u32,
    pub max_reserve_total: u32,
    pub max_units_per_record: u32,
    pub max_adjust_count: u32,
}

impl Default for Policy {
    fn default() -> Self {
        let grants = [
            "workspace.observe",
            "workspace.pick",
            "workspace.place",
            "inventory.reserve",
            "inventory.consume",
            "inventory.release",
            "inventory.return",
            "inventory.adjust_count",
            "ops.status",
        ]
        .iter()
        .map(|c| (c.to_string(), "1".to_string()))
        .collect();
        Policy {
            grants,
            pick_sources: vec!["bin-".into(), "tray-".into()],
            place_destinations: vec!["bin-".into(), "tray-".into()],
            max_reserve_per_kind: 24,
            max_reserve_total: 64,
            max_units_per_record: 6,
            max_adjust_count: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "denial", rename_all = "snake_case")]
pub enum Denial {
    CapabilityNotGranted { capability: String },
    VersionNotGranted { capability: String, requested: String, granted: String },
    ArgumentOutOfScope { capability: String, argument: String, reason: String },
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denial::CapabilityNotGranted { capability } => write!(f, "capability `{capability}` is not granted to this run"),
            Denial::VersionNotGranted { capability, requested, granted } => {
                write!(f, "`{capability}` version {requested} requested but only version {granted} is granted")
            }
            Denial::ArgumentOutOfScope { capability, argument, reason } => {
                write!(f, "`{capability}` argument `{argument}` is outside the granted scope: {reason}")
            }
        }
    }
}

impl Policy {
    pub fn check_capability(&self, capability: &str, version: &str) -> Result<(), Denial> {
        match self.grants.get(capability) {
            None => Err(Denial::CapabilityNotGranted { capability: capability.into() }),
            Some(g) if g != version => Err(Denial::VersionNotGranted {
                capability: capability.into(),
                requested: version.into(),
                granted: g.clone(),
            }),
            Some(_) => Ok(()),
        }
    }

    /// Check argument scope. Arguments that are unresolved references are skipped
    /// here (they are type-checked by the compiler and re-checked at dispatch).
    pub fn check_arguments(&self, capability: &str, args: &serde_json::Map<String, Value>) -> Result<(), Denial> {
        let deny = |arg: &str, reason: String| Denial::ArgumentOutOfScope {
            capability: capability.into(),
            argument: arg.into(),
            reason,
        };
        let is_ref = |v: &Value| v.as_object().is_some_and(|o| o.contains_key("$ref"));
        let str_arg = |k: &str| args.get(k).filter(|v| !is_ref(v)).and_then(Value::as_str);
        match capability {
            "workspace.pick" => {
                if let Some(src) = str_arg("source_id") {
                    if !self.pick_sources.iter().any(|p| src.starts_with(p.as_str())) {
                        return Err(deny("source_id", format!("`{src}` is not a granted pick source ({})", self.pick_sources.join(", "))));
                    }
                }
            }
            "workspace.place" => {
                if let Some(dst) = str_arg("destination_id") {
                    if !self.place_destinations.iter().any(|p| dst.starts_with(p.as_str())) {
                        return Err(deny(
                            "destination_id",
                            format!("`{dst}` is not a granted destination ({})", self.place_destinations.join(", ")),
                        ));
                    }
                }
            }
            "inventory.reserve" => {
                if let Some(v) = args.get("counts").filter(|v| !is_ref(v)) {
                    let counts = counts_from_value(v);
                    let mut total = 0u32;
                    for (k, n) in &counts {
                        if !is_known_kind(k) {
                            return Err(deny("counts", format!("unknown part kind `{k}`")));
                        }
                        if *n > self.max_reserve_per_kind {
                            return Err(deny("counts", format!("{n} x {k} exceeds the per-kind limit {}", self.max_reserve_per_kind)));
                        }
                        total += n;
                    }
                    if total > self.max_reserve_total {
                        return Err(deny("counts", format!("total {total} exceeds the limit {}", self.max_reserve_total)));
                    }
                }
            }
            "inventory.consume" | "inventory.return" => {
                if let Some(q) = args.get("quantity").and_then(Value::as_u64) {
                    if q == 0 || q > self.max_units_per_record as u64 {
                        return Err(deny("quantity", format!("must be 1..={}", self.max_units_per_record)));
                    }
                }
            }
            "inventory.adjust_count" => {
                if let Some(q) = args.get("observed_count").and_then(Value::as_u64) {
                    if q > self.max_adjust_count as u64 {
                        return Err(deny("observed_count", format!("exceeds the limit {}", self.max_adjust_count)));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn authorize(&self, capability: &str, version: &str, args: &Value) -> Result<(), Denial> {
        self.check_capability(capability, version)?;
        let empty = serde_json::Map::new();
        self.check_arguments(capability, args.as_object().unwrap_or(&empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denies_ungranted_capability_and_version() {
        let p = Policy::default();
        assert!(matches!(p.authorize("system.shell", "1", &json!({})), Err(Denial::CapabilityNotGranted { .. })));
        assert!(matches!(p.authorize("workspace.pick", "2", &json!({})), Err(Denial::VersionNotGranted { .. })));
    }

    #[test]
    fn denies_out_of_scope_arguments() {
        let p = Policy::default();
        let r = p.authorize("workspace.place", "1", &json!({"object_id": "red-01", "destination_id": "outside"}));
        assert!(matches!(r, Err(Denial::ArgumentOutOfScope { .. })));
        let r = p.authorize("inventory.reserve", "1", &json!({"counts": {"washer": 500}}));
        assert!(matches!(r, Err(Denial::ArgumentOutOfScope { .. })));
        assert!(p.authorize("workspace.place", "1", &json!({"object_id": "red-01", "destination_id": "tray-1"})).is_ok());
    }
}
