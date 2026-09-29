//! Domain vocabulary shared by the planner, compiler, runtime and simulator:
//! part kinds, locations, observations and the human goal.

use crate::bail;
use crate::util::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Part kinds the v0 task family understands. `unknown` exists only as an
/// observed classification; it can never be required by a goal.
pub const KNOWN_KINDS: &[&str] = &["red_block", "blue_block", "green_block", "washer"];
pub const UNKNOWN_KIND: &str = "unknown";
pub const GRIPPER: &str = "gripper";

pub fn is_known_kind(k: &str) -> bool {
    KNOWN_KINDS.contains(&k)
}

/// Stock bin that holds (and accepts returns of) a given kind.
pub fn bin_for_kind(kind: &str) -> String {
    match kind {
        "red_block" => "bin-red".into(),
        "blue_block" => "bin-blue".into(),
        "green_block" => "bin-green".into(),
        "washer" => "bin-washer".into(),
        other => format!("bin-{other}"),
    }
}

pub type KindCounts = BTreeMap<String, u32>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationType {
    Bin,
    Tray,
    Gripper,
    /// Outside the workspace (e.g. removed by an operator). Not reachable by capabilities.
    Outside,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationView {
    pub location_id: String,
    pub loc_type: LocationType,
    /// For bins: the only kind a bin accepts. Trays accept any kind.
    pub accepts_kind: Option<String>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectView {
    pub object_id: String,
    /// Classification as observed (may be `unknown`). Never the hidden truth.
    pub observed_kind: String,
    pub location_id: String,
}

/// What an observer can see of the workspace at one world revision.
///
/// In v0 this is synthesised from simulator state; it is NOT perception.
/// `withheld` lists locations the observer could not see (evidence blackout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub world_revision: u64,
    pub observed_at_ms: i64,
    pub source: String,
    pub locations: Vec<LocationView>,
    pub objects: Vec<ObjectView>,
    pub withheld: Vec<String>,
}

impl Observation {
    pub fn location(&self, id: &str) -> Option<&LocationView> {
        self.locations.iter().find(|l| l.location_id == id)
    }
    pub fn is_withheld(&self, loc: &str) -> bool {
        self.withheld.iter().any(|w| w == loc)
    }
    pub fn object(&self, id: &str) -> Option<&ObjectView> {
        self.objects.iter().find(|o| o.object_id == id)
    }
    pub fn objects_at<'a>(&'a self, loc: &'a str) -> impl Iterator<Item = &'a ObjectView> + 'a {
        self.objects.iter().filter(move |o| o.location_id == loc)
    }
    pub fn gripper_contents(&self) -> Option<&ObjectView> {
        self.objects_at(GRIPPER).next()
    }
    pub fn trays(&self) -> Vec<&LocationView> {
        self.locations.iter().filter(|l| l.loc_type == LocationType::Tray).collect()
    }
    pub fn bins(&self) -> Vec<&LocationView> {
        self.locations.iter().filter(|l| l.loc_type == LocationType::Bin).collect()
    }
}

/// Human-supplied facts that refine observations (e.g. "item-01 is a washer").
/// Provenance is always recorded; the runtime never invents these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub object_id: String,
    /// A known kind, or "not_usable" if the human says it is none of the needed kinds.
    pub kind: String,
    pub provenance: String,
}

/// Effective kind of an observed object after applying human classifications.
pub fn effective_kind(obj: &ObjectView, classes: &[Classification]) -> String {
    if obj.observed_kind == UNKNOWN_KIND {
        if let Some(c) = classes.iter().rev().find(|c| c.object_id == obj.object_id) {
            return c.kind.clone();
        }
    }
    obj.observed_kind.clone()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Substitution {
    /// The required kind that may be replaced.
    pub replace: String,
    /// The kind that may stand in for it.
    pub with: String,
}

/// The human's goal for the kit-assembly task family. Deliberately contains no
/// permission fields: correcting a goal can never widen authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSpec {
    pub kit_quantity: u32,
    pub kit_components: KindCounts,
    /// Substitutions the human has explicitly approved.
    #[serde(default)]
    pub approved_substitutions: Vec<Substitution>,
    /// Substitutions the human has explicitly declined.
    #[serde(default)]
    pub declined_substitutions: Vec<Substitution>,
    /// Always true in v0: the runtime must ask before any unapproved substitution.
    #[serde(default = "default_true")]
    pub ask_before_substitution: bool,
    /// Whether parts may be moved back out of kit trays (rework). `None` = not yet decided.
    #[serde(default)]
    pub rework_permitted: Option<bool>,
    /// Original request text, kept for display and audit. Not interpreted by the runtime.
    #[serde(default)]
    pub source_text: Option<String>,
}

fn default_true() -> bool {
    true
}

pub const MAX_KITS: u32 = 8;
pub const MAX_PER_KIND_PER_KIT: u32 = 6;

impl GoalSpec {
    pub fn validate(&self) -> Result<()> {
        if self.kit_quantity == 0 || self.kit_quantity > MAX_KITS {
            bail!("kit_quantity must be between 1 and {MAX_KITS} (got {})", self.kit_quantity);
        }
        if self.kit_components.is_empty() {
            bail!("kit_components must list at least one part kind");
        }
        for (k, n) in &self.kit_components {
            if !is_known_kind(k) {
                bail!("kit_components: unknown part kind `{k}` (known: {})", KNOWN_KINDS.join(", "));
            }
            if *n == 0 || *n > MAX_PER_KIND_PER_KIT {
                bail!("kit_components.{k}: quantity must be 1..={MAX_PER_KIND_PER_KIT} (got {n})");
            }
        }
        for s in self.approved_substitutions.iter().chain(&self.declined_substitutions) {
            if !is_known_kind(&s.replace) || !is_known_kind(&s.with) || s.replace == s.with {
                bail!("invalid substitution {} -> {}", s.replace, s.with);
            }
        }
        if !self.ask_before_substitution {
            bail!("ask_before_substitution cannot be disabled in v0");
        }
        Ok(())
    }

    pub fn substitute_for(&self, kind: &str) -> Vec<String> {
        self.approved_substitutions
            .iter()
            .filter(|s| s.replace == kind)
            .map(|s| s.with.clone())
            .collect()
    }

    pub fn is_declined(&self, replace: &str, with: &str) -> bool {
        self.declined_substitutions.iter().any(|s| s.replace == replace && s.with == with)
    }

    pub fn is_approved(&self, replace: &str, with: &str) -> bool {
        self.approved_substitutions.iter().any(|s| s.replace == replace && s.with == with)
    }

    pub fn summary(&self) -> String {
        let parts: Vec<String> = self.kit_components.iter().map(|(k, n)| format!("{n}x {k}")).collect();
        format!("{} kit(s) of [{}]", self.kit_quantity, parts.join(", "))
    }
}

/// Domain knowledge: which kinds could plausibly stand in for which (only ever
/// used after asking the human).
pub fn substitution_candidates(kind: &str) -> &'static [&'static str] {
    match kind {
        "blue_block" | "red_block" => &["green_block"],
        _ => &[],
    }
}

/// Evaluate whether tray contents satisfy a composition exactly, allowing approved
/// substitutions. Returns (complete, missing, excess).
pub fn evaluate_kit(contents: &KindCounts, goal: &GoalSpec) -> (bool, KindCounts, KindCounts) {
    let mut have = contents.clone();
    let mut missing = KindCounts::new();
    // First satisfy requirements with the exact kind, then with approved substitutes.
    for (kind, need) in &goal.kit_components {
        let exact = have.get(kind).copied().unwrap_or(0);
        let used = exact.min(*need);
        *have.entry(kind.clone()).or_default() -= used;
        let mut short = need - used;
        for sub in goal.substitute_for(kind) {
            if short == 0 {
                break;
            }
            let avail = have.get(&sub).copied().unwrap_or(0);
            // Do not consume a substitute that is itself required by the composition.
            let reserved_for_self = goal.kit_components.get(&sub).copied().unwrap_or(0);
            let spare = avail.saturating_sub(reserved_for_self);
            let take = spare.min(short);
            *have.entry(sub.clone()).or_default() -= take;
            short -= take;
        }
        if short > 0 {
            missing.insert(kind.clone(), short);
        }
    }
    let excess: KindCounts = have.into_iter().filter(|(_, n)| *n > 0).collect();
    (missing.is_empty() && excess.is_empty(), missing, excess)
}

/// Restricted-grammar intent parser for the scripted mode. It accepts only the
/// task family's canonical phrasing, e.g.
/// "Prepare 3 kits. Each kit needs one red block, one blue block, and two washers."
/// Anything else returns an error instead of a guessed interpretation.
pub fn parse_restricted_request(text: &str) -> Result<GoalSpec> {
    let lower = text.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    let num = |w: &str| -> Option<u32> {
        match w {
            "a" | "an" | "one" => Some(1),
            "two" => Some(2),
            "three" => Some(3),
            "four" => Some(4),
            "five" => Some(5),
            "six" => Some(6),
            "seven" => Some(7),
            "eight" => Some(8),
            _ => w.parse().ok(),
        }
    };
    // "prepare|make|build N kit(s)"
    let mut qty = None;
    for i in 0..words.len().saturating_sub(2) {
        if matches!(words[i].as_str(), "prepare" | "make" | "build" | "assemble")
            && matches!(words[i + 2].as_str(), "kit" | "kits")
        {
            qty = num(&words[i + 1]);
        }
    }
    let Some(kit_quantity) = qty else {
        bail!("scripted parser: expected `Prepare <N> kits` (restricted grammar; free-text interpretation needs a live planner)");
    };
    let start = words
        .iter()
        .position(|w| w == "needs" || w == "need" || w == "contains")
        .ok_or_else(|| crate::util::Error::new("scripted parser: expected `Each kit needs ...`"))?;
    let mut comps = KindCounts::new();
    let mut i = start + 1;
    while i < words.len() {
        if let Some(n) = num(&words[i]) {
            let (kind, adv) = match (words.get(i + 1).map(String::as_str), words.get(i + 2).map(String::as_str)) {
                (Some("red"), Some("block" | "blocks")) => ("red_block", 3),
                (Some("blue"), Some("block" | "blocks")) => ("blue_block", 3),
                (Some("green"), Some("block" | "blocks")) => ("green_block", 3),
                (Some("washer" | "washers"), _) => ("washer", 2),
                (Some(other), _) => bail!("scripted parser: unsupported component `{other}`"),
                (None, _) => bail!("scripted parser: dangling quantity"),
            };
            *comps.entry(kind.to_string()).or_default() += n;
            i += adv;
        } else if matches!(words[i].as_str(), "and" | "each" | "kit" | "update" | "tell" | "ask") {
            // Clauses after the component list ("Update inventory...", "Tell me...") end the list.
            if !matches!(words[i].as_str(), "and") && !comps.is_empty() {
                break;
            }
            i += 1;
        } else {
            if comps.is_empty() {
                bail!("scripted parser: unexpected word `{}` in component list", words[i]);
            }
            break;
        }
    }
    let g = GoalSpec {
        kit_quantity,
        kit_components: comps,
        approved_substitutions: vec![],
        declined_substitutions: vec![],
        ask_before_substitution: true,
        rework_permitted: None,
        source_text: Some(text.to_string()),
    };
    g.validate()?;
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_request() {
        let g = parse_restricted_request(
            "Prepare three kits. Each kit needs one red block, one blue block, and two washers. \
             Update inventory as you use parts. Tell me what is missing and ask before making substitutions.",
        )
        .unwrap();
        assert_eq!(g.kit_quantity, 3);
        assert_eq!(g.kit_components.get("red_block"), Some(&1));
        assert_eq!(g.kit_components.get("blue_block"), Some(&1));
        assert_eq!(g.kit_components.get("washer"), Some(&2));
    }

    #[test]
    fn rejects_unsupported_text_instead_of_guessing() {
        assert!(parse_restricted_request("Could you tidy the workbench?").is_err());
        assert!(parse_restricted_request("Prepare 2 kits. Each kit needs one purple gear.").is_err());
    }

    #[test]
    fn kit_evaluation_exact_and_substitution() {
        let mut g = parse_restricted_request("Prepare 1 kit. Each kit needs one red block, one blue block and two washers").unwrap();
        let mut c = KindCounts::new();
        c.insert("red_block".into(), 1);
        c.insert("green_block".into(), 1);
        c.insert("washer".into(), 2);
        let (ok, missing, excess) = evaluate_kit(&c, &g);
        assert!(!ok);
        assert_eq!(missing.get("blue_block"), Some(&1));
        assert_eq!(excess.get("green_block"), Some(&1));
        g.approved_substitutions.push(Substitution { replace: "blue_block".into(), with: "green_block".into() });
        let (ok, _, _) = evaluate_kit(&c, &g);
        assert!(ok);
        c.insert("washer".into(), 3);
        let (ok, _, excess) = evaluate_kit(&c, &g);
        assert!(!ok, "excess parts mean the kit does not match the composition");
        assert_eq!(excess.get("washer"), Some(&1));
    }
}
