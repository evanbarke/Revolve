//! Scenario definitions: initial world, inventory record, goal, event-triggered
//! faults and the scripted human. Faults and the human script are harness data:
//! they are stored in the adapter/harness store and never shown to planners.

use crate::model::{GoalSpec, KindCounts};
use crate::util::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Capability whose applied effects are counted (e.g. `workspace.place`).
    pub capability: String,
    /// Fires on the nth applied effect of that capability (1-based), counted by the
    /// simulator across the whole run — the same world event for both benchmark arms.
    pub nth: u32,
}

impl Trigger {
    pub fn describe(&self) -> String {
        format!("{} #{}", self.capability, self.nth)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultSpec {
    /// The effect commits, then the executor process terminates before it can record the result.
    CrashAfterEffect { on: Trigger },
    /// The executor process terminates just before the effect would have been applied.
    CrashBeforeEffect { on: Trigger },
    /// The effect commits but the caller sees a timeout (unknown outcome).
    TimeoutAfterEffect {
        on: Trigger,
        #[serde(default)]
        blackout: bool,
    },
    /// No effect happens and the caller sees a timeout (unknown outcome).
    TimeoutBeforeEffect {
        on: Trigger,
        #[serde(default)]
        blackout: bool,
    },
    /// A tray (or bin) becomes unavailable after the triggering effect.
    DestinationUnavailable { on: Trigger, location_id: String },
    /// An operator removes an object from the workspace after the triggering effect.
    RemoveObject { on: Trigger, object_id: String },
    /// The planner connection is lost after the triggering effect (or from the start).
    PlannerUnavailable {
        #[serde(default)]
        on: Option<Trigger>,
    },
    /// The next planner decision after the trigger (or the first) proposes an
    /// unauthorised action. Tests that policy holds whatever the planner says.
    InjectUnauthorizedAction {
        #[serde(default)]
        on: Option<Trigger>,
    },
}

impl FaultSpec {
    pub fn trigger(&self) -> Option<&Trigger> {
        match self {
            FaultSpec::CrashAfterEffect { on }
            | FaultSpec::CrashBeforeEffect { on }
            | FaultSpec::TimeoutAfterEffect { on, .. }
            | FaultSpec::TimeoutBeforeEffect { on, .. }
            | FaultSpec::DestinationUnavailable { on, .. }
            | FaultSpec::RemoveObject { on, .. } => Some(on),
            FaultSpec::PlannerUnavailable { on } | FaultSpec::InjectUnauthorizedAction { on } => {
                on.as_ref()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownItem {
    pub bin: String,
    /// Hidden truth: a known kind, or e.g. "scrap".
    pub true_kind: String,
}

fn default_trays() -> u32 {
    4
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldSetup {
    pub stock: KindCounts,
    /// Parts in a mixed pile on the table (playground setups).
    #[serde(default, skip_serializing_if = "KindCounts::is_empty")]
    pub table: KindCounts,
    #[serde(default)]
    pub unknown_items: Vec<UnknownItem>,
    #[serde(default = "default_trays")]
    pub trays: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    /// "truth" (answer with the hidden true kind) or an explicit kind / "not_usable".
    #[serde(default)]
    pub classification: Option<String>,
    /// "approve" | "decline"
    #[serde(default)]
    pub substitution: Option<String>,
    /// "approve" | "decline"
    #[serde(default)]
    pub rework: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanAction {
    Correct { on: Trigger, goal: GoalSpec },
    Cancel { on: Trigger },
}

impl HumanAction {
    pub fn trigger(&self) -> &Trigger {
        match self {
            HumanAction::Correct { on, .. } | HumanAction::Cancel { on } => on,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HumanScript {
    #[serde(default)]
    pub answers: Answers,
    #[serde(default)]
    pub actions: Vec<HumanAction>,
    /// When the run blocks on an unknown outcome, restore the withheld evidence
    /// (models a person inspecting the workspace / service recovering) and resume.
    #[serde(default)]
    pub restore_evidence_when_blocked: bool,
    /// When the run waits for an unavailable planner, restore the connection and resume.
    #[serde(default)]
    pub restore_planner_when_waiting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// completed | partial | blocked | stopped | awaiting_human | failed
    pub outcome: String,
    /// Number of kits that should truly be complete at the end.
    pub kits_complete: u32,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub family: String,
    pub description: String,
    #[serde(default)]
    pub seed: u64,
    pub world: WorldSetup,
    /// Inventory record at start; defaults to the true stock counts.
    #[serde(default)]
    pub inventory_record: Option<KindCounts>,
    pub goal: GoalSpec,
    #[serde(default)]
    pub faults: Vec<FaultSpec>,
    #[serde(default)]
    pub human: HumanScript,
    pub expect: Expectation,
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Scenario> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            crate::util::Error::new(format!("reading scenario {}: {e}", path.display()))
        })?;
        let s: Scenario = serde_json::from_str(&text).map_err(|e| {
            crate::util::Error::new(format!("parsing scenario {}: {e}", path.display()))
        })?;
        s.goal.validate()?;
        Ok(s)
    }

    pub fn inventory_record(&self) -> KindCounts {
        let mut rec = self
            .inventory_record
            .clone()
            .unwrap_or_else(|| self.world.stock.clone());
        for k in crate::model::KNOWN_KINDS {
            rec.entry(k.to_string()).or_insert(0);
        }
        rec
    }
}

/// Resolve a scenario name to a file: an explicit path, or `<scenarios_dir>/**/<name>.json`.
pub fn resolve(name_or_path: &str, scenarios_dir: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(name_or_path);
    if p.extension().is_some_and(|e| e == "json") && p.exists() {
        return Ok(p);
    }
    for sub in ["demo", "development", "held-out", "playground"] {
        let cand = scenarios_dir.join(sub).join(format!("{name_or_path}.json"));
        if cand.exists() {
            return Ok(cand);
        }
    }
    crate::bail!(
        "scenario `{name_or_path}` not found (looked in {}/{{demo,development,held-out,playground}}); run `revolve scenarios` to list them",
        scenarios_dir.display()
    )
}

pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
