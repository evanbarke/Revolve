//! Action IR compiler/validator.
//!
//! Responsibilities (and only these):
//! 1. Parse an untrusted proposal with diagnostics that point at the step/field.
//! 2. Schema: IR version, known capabilities, contract versions, argument types.
//! 3. Structure: unique ids, dependencies, cycles, typed output references,
//!    explicit ordering of steps that share an exclusive resource.
//! 4. Binding: the plan targets the current goal revision and planning snapshot.
//! 5. Authorisation: requested permissions vs runtime-held grants; argument scope.
//! 6. Budgets: step count and requested budgets vs runtime limits.
//! 7. Feasibility: abstract execution of every step's contract semantics against
//!    the planner's observed snapshot, checking declared pre/postconditions,
//!    stock/reservation arithmetic, and that claimed kit outcomes follow.
//!
//! The output is a deterministic dispatch order. A compiled plan is still only
//! a belief-level check: the runtime re-checks preconditions against fresh
//! observations and the verifier checks reality afterwards.

use crate::capability::{self, ArgType, Contract, counts_from_value};
use crate::inventory::InventoryView;
use crate::ir::{
    ACTION_IR_VERSION, ArgValue, Condition, PlanHeader, PlanProposal, RUNTIME_OWNED_FIELDS, Step,
};
use crate::model::{
    Classification, GRIPPER, GoalSpec, KindCounts, LocationType, Observation, bin_for_kind,
    effective_kind, evaluate_against,
};
use crate::policy::Policy;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiagCategory {
    /// Not valid JSON / not the right shape.
    Syntax,
    /// Wrong IR version, unknown capability or version, bad argument type.
    Schema,
    /// Ids, dependencies, cycles, references, resource ordering.
    Structure,
    /// Targets an obsolete goal revision or planning snapshot.
    Stale,
    /// Requests a capability, version or argument outside the grants.
    Authorization,
    /// Exceeds runtime limits.
    Budget,
    /// Not executable against the observed world (preconditions, stock, claims).
    Feasibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub category: DiagCategory,
    pub step_id: Option<String>,
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}: {}", self.category, self.path, self.message)
    }
}

fn diag(
    category: DiagCategory,
    step: Option<&str>,
    path: impl Into<String>,
    message: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        category,
        step_id: step.map(String::from),
        path: path.into(),
        message: message.into(),
    }
}

/// A structural diagnostic about the plan as a whole (e.g. from code generation).
pub fn structure_diagnostic(message: String) -> Diagnostic {
    diag(DiagCategory::Structure, None, "steps", message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub max_actions: u32,
    pub max_planner_decisions: u32,
    pub max_elapsed_ms: u64,
    pub max_repairs_per_episode: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_actions: 120,
            max_planner_decisions: 12,
            max_elapsed_ms: 10 * 60 * 1000,
            max_repairs_per_episode: 3,
        }
    }
}

pub struct CompileContext<'a> {
    pub goal_id: &'a str,
    pub goal_revision: u32,
    pub goal: &'a GoalSpec,
    pub observation: &'a Observation,
    pub classifications: &'a [Classification],
    pub inventory: &'a InventoryView,
    pub consumed_evidence: &'a [String],
    pub policy: &'a Policy,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompiledPlan {
    pub header: PlanHeader,
    /// Steps in dispatch order.
    pub steps: Vec<Step>,
    pub predicted_complete_trays: Vec<String>,
}

fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse untrusted JSON into a proposal. Steps are parsed one by one so errors
/// name the offending step; runtime-owned fields get a specific diagnostic.
pub fn parse_proposal(v: &Value) -> Result<PlanProposal, Vec<Diagnostic>> {
    let mut diags = vec![];
    let Some(obj) = v.as_object() else {
        return Err(vec![diag(
            DiagCategory::Syntax,
            None,
            "$",
            "plan must be a JSON object",
        )]);
    };
    for k in obj.keys() {
        if RUNTIME_OWNED_FIELDS.contains(&k.as_str()) {
            diags.push(diag(
                DiagCategory::Authorization,
                None,
                format!("$.{k}"),
                format!("`{k}` is runtime-owned and cannot be supplied by a planner"),
            ));
        }
    }
    let mut header_obj = obj.clone();
    let steps_v = header_obj.remove("steps");
    let header: Option<PlanHeader> = match serde_json::from_value(Value::Object(
        header_obj
            .into_iter()
            .filter(|(k, _)| !RUNTIME_OWNED_FIELDS.contains(&k.as_str()))
            .collect(),
    )) {
        Ok(h) => Some(h),
        Err(e) => {
            let fields = header_field_diagnostics(obj);
            if fields.is_empty() {
                diags.push(diag(
                    DiagCategory::Syntax,
                    None,
                    "$",
                    format!("plan header: {e}"),
                ));
            }
            diags.extend(fields);
            None
        }
    };
    let mut steps = vec![];
    match steps_v {
        Some(Value::Array(arr)) => {
            for (i, s) in arr.iter().enumerate() {
                let sid = s.get("step_id").and_then(Value::as_str).map(String::from);
                let path = format!("$.steps[{i}]");
                if let Some(o) = s.as_object() {
                    for k in o.keys() {
                        if RUNTIME_OWNED_FIELDS.contains(&k.as_str()) {
                            diags.push(diag(
                                DiagCategory::Authorization,
                                sid.as_deref(),
                                format!("{path}.{k}"),
                                format!(
                                    "`{k}` is runtime-owned and cannot be supplied by a planner"
                                ),
                            ));
                        }
                    }
                }
                let cleaned = match s.as_object() {
                    Some(o) => Value::Object(
                        o.iter()
                            .filter(|(k, _)| !RUNTIME_OWNED_FIELDS.contains(&k.as_str()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    None => s.clone(),
                };
                match serde_json::from_value::<Step>(cleaned) {
                    Ok(st) => steps.push(st),
                    Err(e) => diags.push(diag(
                        DiagCategory::Syntax,
                        sid.as_deref(),
                        path,
                        format!("{e}"),
                    )),
                }
            }
        }
        Some(_) => diags.push(diag(
            DiagCategory::Syntax,
            None,
            "$.steps",
            "`steps` must be an array",
        )),
        None => diags.push(diag(
            DiagCategory::Syntax,
            None,
            "$.steps",
            "missing `steps`",
        )),
    }
    match (header, diags.is_empty()) {
        (Some(h), true) => Ok(PlanProposal { header: h, steps }),
        _ => Err(diags),
    }
}

type FieldCheck = fn(&Value) -> Result<(), String>;

fn check_as<T: serde::de::DeserializeOwned>(v: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(v.clone())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// One diagnostic per bad, missing or unknown header field, each with its path and
/// the expected shape, so a planner can repair its proposal.
fn header_field_diagnostics(obj: &serde_json::Map<String, Value>) -> Vec<Diagnostic> {
    use crate::ir::{Budgets, ExpectedOutcome, KitAssignment, PlanBasis};
    let fields: &[(&str, bool, FieldCheck, &str)] = &[
        (
            "schema_version",
            true,
            check_as::<String>,
            "a string: \"revolve.action-ir/v1\"",
        ),
        ("plan_id", true, check_as::<String>, "a string label"),
        (
            "goal_id",
            true,
            check_as::<String>,
            "a string (copy goal_id from the request)",
        ),
        (
            "goal_revision",
            true,
            check_as::<u32>,
            "an integer (copy goal_revision from the request)",
        ),
        (
            "basis",
            true,
            check_as::<PlanBasis>,
            "an object {\"world_revision\": <observation.world_revision>}",
        ),
        (
            "capabilities",
            true,
            check_as::<BTreeMap<String, String>>,
            "an object mapping capability id to version, e.g. {\"workspace.pick\": \"1\"}",
        ),
        (
            "requested_permissions",
            true,
            check_as::<Vec<String>>,
            "an array of capability ids",
        ),
        (
            "budgets",
            true,
            check_as::<Budgets>,
            "an object {\"max_actions\", \"max_planner_rounds\", \"max_elapsed_ms\"}",
        ),
        (
            "kit_assignments",
            false,
            check_as::<Vec<KitAssignment>>,
            "an array of {\"kit\", \"tray_id\"}",
        ),
        ("explanation", true, check_as::<String>, "a short string"),
        (
            "expected_outcome",
            false,
            check_as::<ExpectedOutcome>,
            "an object {\"kits_complete\": [tray ids], \"kits_incomplete\": [...]}",
        ),
    ];
    let mut d = vec![];
    for (name, required, check, shape) in fields {
        match obj.get(*name) {
            None if *required => d.push(diag(
                DiagCategory::Syntax,
                None,
                format!("$.{name}"),
                format!("missing required field; expected {shape}"),
            )),
            None => {}
            Some(v) => {
                if let Err(e) = check(v) {
                    d.push(diag(
                        DiagCategory::Syntax,
                        None,
                        format!("$.{name}"),
                        format!("expected {shape} ({e})"),
                    ));
                }
            }
        }
    }
    for k in obj.keys() {
        if k != "steps"
            && !RUNTIME_OWNED_FIELDS.contains(&k.as_str())
            && !fields.iter().any(|(n, ..)| n == k)
        {
            d.push(diag(
                DiagCategory::Syntax,
                None,
                format!("$.{k}"),
                "unknown field",
            ));
        }
    }
    d
}

pub fn compile(p: &PlanProposal, ctx: &CompileContext) -> Result<CompiledPlan, Vec<Diagnostic>> {
    let mut d = vec![];
    let h = &p.header;

    // ---- 2. schema
    if h.schema_version != ACTION_IR_VERSION {
        d.push(diag(
            DiagCategory::Schema,
            None,
            "$.schema_version",
            format!(
                "unsupported schema `{}` (expected `{ACTION_IR_VERSION}`)",
                h.schema_version
            ),
        ));
        return Err(d);
    }
    let mut contracts: HashMap<String, Contract> = HashMap::new();
    for (i, s) in p.steps.iter().enumerate() {
        let path = format!("$.steps[{i}]");
        let sid = Some(s.step_id.as_str());
        let Some(c) = capability::lookup(&s.capability) else {
            d.push(diag(
                DiagCategory::Schema,
                sid,
                format!("{path}.capability"),
                format!("unknown capability `{}`", s.capability),
            ));
            continue;
        };
        if !c.plan_step_allowed {
            d.push(diag(
                DiagCategory::Schema,
                sid,
                format!("{path}.capability"),
                format!("`{}` cannot be used as a plan step", s.capability),
            ));
        }
        for (ci, pc) in s.postconditions.iter().enumerate() {
            if matches!(pc, Condition::GoalRevisionEquals { .. }) {
                d.push(diag(
                    DiagCategory::Schema,
                    sid,
                    format!("{path}.postconditions[{ci}]"),
                    "goal_revision_equals is not an observable effect; use it as a precondition",
                ));
            }
        }
        if s.capability_version != c.version {
            d.push(diag(
                DiagCategory::Schema,
                sid,
                format!("{path}.capability_version"),
                format!(
                    "`{}` version `{}` is not registered (registered: `{}`)",
                    s.capability, s.capability_version, c.version
                ),
            ));
        }
        match h.capabilities.get(&s.capability) {
            None => d.push(diag(
                DiagCategory::Schema,
                sid,
                "$.capabilities",
                format!(
                    "capability `{}` used by step but not listed in `capabilities`",
                    s.capability
                ),
            )),
            Some(v) if *v != s.capability_version => d.push(diag(
                DiagCategory::Schema,
                sid,
                "$.capabilities",
                format!(
                    "`capabilities` lists `{}` version `{v}` but the step uses `{}`",
                    s.capability, s.capability_version
                ),
            )),
            _ => {}
        }
        for spec in &c.args {
            match s.arguments.get(spec.name) {
                None => d.push(diag(
                    DiagCategory::Schema,
                    sid,
                    format!("{path}.arguments"),
                    format!("missing argument `{}` ({:?})", spec.name, spec.ty),
                )),
                Some(v) => match ArgValue::parse(v) {
                    Err(e) => d.push(diag(
                        DiagCategory::Schema,
                        sid,
                        format!("{path}.arguments.{}", spec.name),
                        e,
                    )),
                    Ok(ArgValue::Literal(lit)) => {
                        if let Err(e) = spec.ty.check(&lit) {
                            d.push(diag(
                                DiagCategory::Schema,
                                sid,
                                format!("{path}.arguments.{}", spec.name),
                                e,
                            ));
                        }
                    }
                    Ok(ArgValue::Ref { .. }) => {}
                },
            }
        }
        for k in s.arguments.keys() {
            if c.arg(k).is_none() {
                d.push(diag(
                    DiagCategory::Schema,
                    sid,
                    format!("{path}.arguments.{k}"),
                    format!("unknown argument `{k}` for `{}`", s.capability),
                ));
            }
        }
        contracts.insert(s.step_id.clone(), c);
    }
    if !d.is_empty() {
        return Err(d);
    }

    // ---- 3. structure
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, s) in p.steps.iter().enumerate() {
        if !is_id(&s.step_id) {
            d.push(diag(
                DiagCategory::Structure,
                Some(&s.step_id),
                format!("$.steps[{i}].step_id"),
                "step_id must be 1-80 chars of [A-Za-z0-9_-]",
            ));
        }
        if index.insert(s.step_id.as_str(), i).is_some() {
            d.push(diag(
                DiagCategory::Structure,
                Some(&s.step_id),
                format!("$.steps[{i}].step_id"),
                format!("duplicate step_id `{}`", s.step_id),
            ));
        }
    }
    for (i, s) in p.steps.iter().enumerate() {
        for dep in &s.depends_on {
            if dep == &s.step_id {
                d.push(diag(
                    DiagCategory::Structure,
                    Some(&s.step_id),
                    format!("$.steps[{i}].depends_on"),
                    "a step cannot depend on itself",
                ));
            } else if !index.contains_key(dep.as_str()) {
                d.push(diag(
                    DiagCategory::Structure,
                    Some(&s.step_id),
                    format!("$.steps[{i}].depends_on"),
                    format!("unknown dependency `{dep}`"),
                ));
            }
        }
    }
    if !d.is_empty() {
        return Err(d);
    }
    // Kahn's algorithm; ties broken by list order for a deterministic dispatch order.
    let n = p.steps.len();
    let mut indeg = vec![0usize; n];
    let mut children: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, s) in p.steps.iter().enumerate() {
        for dep in &s.depends_on {
            let j = index[dep.as_str()];
            children[j].push(i);
            indeg[i] += 1;
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|i| indeg[*i] == 0).collect();
    let mut order = vec![];
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        order.push(i);
        for &c in &children[i] {
            indeg[c] -= 1;
            if indeg[c] == 0 {
                ready.insert(c);
            }
        }
    }
    if order.len() != n {
        // Report one concrete cycle (not every step downstream of it).
        let stuck: BTreeSet<usize> = (0..n).filter(|i| !order.contains(i)).collect();
        let mut path = vec![];
        let mut cur = *stuck.iter().next().unwrap_or(&0);
        let mut seen: HashMap<usize, usize> = HashMap::new();
        while !seen.contains_key(&cur) {
            seen.insert(cur, path.len());
            path.push(cur);
            cur = p.steps[cur]
                .depends_on
                .iter()
                .map(|d| index[d.as_str()])
                .find(|j| stuck.contains(j))
                .unwrap_or(cur);
        }
        let cycle: Vec<&str> = path[seen[&cur]..]
            .iter()
            .rev()
            .map(|&i| p.steps[i].step_id.as_str())
            .collect();
        d.push(diag(
            DiagCategory::Structure,
            Some(cycle[0]),
            "$.steps",
            format!(
                "dependency cycle: {} -> {} ({} step(s) cannot be scheduled)",
                cycle.join(" -> "),
                cycle[0],
                stuck.len()
            ),
        ));
        return Err(d);
    }
    // Transitive ancestors.
    let mut ancestors: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for &i in &order {
        let mut set = BTreeSet::new();
        for dep in &p.steps[i].depends_on {
            let j = index[dep.as_str()];
            set.insert(j);
            set.extend(ancestors[j].iter().copied());
        }
        ancestors[i] = set;
    }
    // Typed output references.
    for (i, s) in p.steps.iter().enumerate() {
        let c = &contracts[&s.step_id];
        for (name, v) in &s.arguments {
            if let Ok(ArgValue::Ref { step_id, output }) = ArgValue::parse(v) {
                let path = format!("$.steps[{i}].arguments.{name}");
                let Some(&j) = index.get(step_id.as_str()) else {
                    d.push(diag(
                        DiagCategory::Structure,
                        Some(&s.step_id),
                        path,
                        format!("reference to unknown step `{step_id}`"),
                    ));
                    continue;
                };
                if !ancestors[i].contains(&j) {
                    d.push(diag(DiagCategory::Structure, Some(&s.step_id), path.clone(), format!("reference to `{step_id}` requires `{step_id}` to be a (transitive) dependency")));
                }
                let src = &contracts[&p.steps[j].step_id];
                match src.output(&output) {
                    None => d.push(diag(
                        DiagCategory::Structure,
                        Some(&s.step_id),
                        path,
                        format!("`{}` has no output `{output}`", src.id),
                    )),
                    Some(o) => {
                        let want = c.arg(name).map(|a| a.ty);
                        if want != Some(o.ty) {
                            d.push(diag(DiagCategory::Structure, Some(&s.step_id), path, format!("type mismatch: `{step_id}.{output}` is {:?} but `{name}` expects {:?}", o.ty, want.unwrap_or(ArgType::OpKey))));
                        }
                    }
                }
            }
        }
    }
    // Exclusive resources must be ordered by an explicit dependency path.
    let resources: Vec<Vec<String>> = p
        .steps
        .iter()
        .map(|s| {
            let m: serde_json::Map<String, Value> = s
                .arguments
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            capability::exclusive_resources(&s.capability, &m)
        })
        .collect();
    'outer: for a in 0..n {
        for b in (a + 1)..n {
            if let Some(r) = resources[a].iter().find(|r| resources[b].contains(r))
                && !ancestors[a].contains(&b)
                && !ancestors[b].contains(&a)
            {
                d.push(diag(
                        DiagCategory::Structure,
                        Some(&p.steps[b].step_id),
                        format!("$.steps[{b}].depends_on"),
                        format!("steps `{}` and `{}` both use exclusive resource `{r}` but neither depends on the other", p.steps[a].step_id, p.steps[b].step_id),
                    ));
                if d.len() > 20 {
                    break 'outer;
                }
            }
        }
    }
    if !d.is_empty() {
        return Err(d);
    }

    // ---- 4. binding / staleness
    if h.goal_id != ctx.goal_id {
        d.push(diag(
            DiagCategory::Stale,
            None,
            "$.goal_id",
            format!(
                "plan is for goal `{}` but this run's goal is `{}`",
                h.goal_id, ctx.goal_id
            ),
        ));
    }
    if h.goal_revision != ctx.goal_revision {
        d.push(diag(
            DiagCategory::Stale,
            None,
            "$.goal_revision",
            format!(
                "plan targets goal revision {} but the current revision is {}",
                h.goal_revision, ctx.goal_revision
            ),
        ));
    }
    if h.basis.world_revision != ctx.observation.world_revision {
        d.push(diag(
            DiagCategory::Stale,
            None,
            "$.basis.world_revision",
            format!(
                "plan is based on world revision {} but the planning snapshot is revision {}",
                h.basis.world_revision, ctx.observation.world_revision
            ),
        ));
    }
    if !d.is_empty() {
        return Err(d);
    }

    // ---- 5. authorisation
    let requested: BTreeSet<&str> = h.requested_permissions.iter().map(String::as_str).collect();
    for perm in &h.requested_permissions {
        let ver = h.capabilities.get(perm).map(String::as_str).unwrap_or("1");
        if let Err(den) = ctx.policy.check_capability(perm, ver) {
            d.push(diag(
                DiagCategory::Authorization,
                None,
                "$.requested_permissions",
                den.to_string(),
            ));
        }
    }
    for (i, s) in p.steps.iter().enumerate() {
        if !requested.contains(s.capability.as_str()) {
            d.push(diag(
                DiagCategory::Authorization,
                Some(&s.step_id),
                format!("$.steps[{i}].capability"),
                format!(
                    "step uses `{}` but the plan does not request that permission",
                    s.capability
                ),
            ));
        }
        if let Err(den) = ctx
            .policy
            .check_capability(&s.capability, &s.capability_version)
        {
            d.push(diag(
                DiagCategory::Authorization,
                Some(&s.step_id),
                format!("$.steps[{i}].capability"),
                den.to_string(),
            ));
        }
        let m: serde_json::Map<String, Value> = s
            .arguments
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Err(den) = ctx.policy.check_arguments(&s.capability, &m) {
            d.push(diag(
                DiagCategory::Authorization,
                Some(&s.step_id),
                format!("$.steps[{i}].arguments"),
                den.to_string(),
            ));
        }
    }
    if !d.is_empty() {
        return Err(d);
    }

    // ---- 6. budgets
    let max_actions = h.budgets.max_actions.min(ctx.limits.max_actions);
    if n as u32 > max_actions {
        d.push(diag(DiagCategory::Budget, None, "$.steps", format!("{n} steps exceed the action budget {max_actions} (plan requested {}, runtime limit {})", h.budgets.max_actions, ctx.limits.max_actions)));
    }
    if h.budgets.max_actions == 0
        || h.budgets.max_elapsed_ms == 0
        || h.budgets.max_planner_rounds == 0
    {
        d.push(diag(
            DiagCategory::Budget,
            None,
            "$.budgets",
            "budgets must be positive",
        ));
    }
    if !d.is_empty() {
        return Err(d);
    }

    // ---- 7. feasibility: abstract execution
    let mut belief = Belief::from(ctx);
    let ordered: Vec<&Step> = order.iter().map(|&i| &p.steps[i]).collect();
    for s in &ordered {
        let i = index[s.step_id.as_str()];
        let path = format!("$.steps[{i}]");
        for (ci, c) in s.preconditions.iter().enumerate() {
            if let Err(why) = belief.holds(c, ctx.goal_revision) {
                d.push(diag(
                    DiagCategory::Feasibility,
                    Some(&s.step_id),
                    format!("{path}.preconditions[{ci}]"),
                    format!(
                        "{} does not hold at this point in the plan: {why}",
                        c.describe()
                    ),
                ));
            }
        }
        if let Err(why) = belief.apply(s) {
            d.push(diag(
                DiagCategory::Feasibility,
                Some(&s.step_id),
                path.clone(),
                why,
            ));
            // Stop simulating: later steps would produce cascading noise.
            break;
        }
        for (ci, c) in s.postconditions.iter().enumerate() {
            if let Err(why) = belief.holds(c, ctx.goal_revision) {
                d.push(diag(
                    DiagCategory::Feasibility,
                    Some(&s.step_id),
                    format!("{path}.postconditions[{ci}]"),
                    format!(
                        "declared postcondition {} does not follow from the contract: {why}",
                        c.describe()
                    ),
                ));
            }
        }
    }
    if !d.is_empty() {
        return Err(d);
    }
    if let Some(held) = belief.gripper() {
        d.push(diag(
            DiagCategory::Feasibility,
            None,
            "$.steps",
            format!("plan ends with {held} still in the gripper"),
        ));
    }
    for (rid, rem) in &belief.new_reservations {
        if rem.1 {
            d.push(diag(
                DiagCategory::Feasibility,
                Some(&rem.0),
                "$.steps",
                format!("reservation opened by `{rid}` is never released"),
            ));
        }
    }
    // Kit assignments and claimed outcome.
    let mut trays_seen = BTreeSet::new();
    for (ki, ka) in h.kit_assignments.iter().enumerate() {
        let path = format!("$.kit_assignments[{ki}]");
        if ka.kit == 0 || ka.kit > ctx.goal.kit_quantity {
            d.push(diag(
                DiagCategory::Feasibility,
                None,
                path.clone(),
                format!("kit {} is outside 1..={}", ka.kit, ctx.goal.kit_quantity),
            ));
        }
        if !trays_seen.insert(ka.tray_id.clone()) {
            d.push(diag(
                DiagCategory::Feasibility,
                None,
                path.clone(),
                format!("tray {} assigned to more than one kit", ka.tray_id),
            ));
        }
        if ctx.observation.location(&ka.tray_id).map(|l| l.loc_type) != Some(LocationType::Tray) {
            d.push(diag(
                DiagCategory::Feasibility,
                None,
                path,
                format!("{} is not a tray", ka.tray_id),
            ));
        }
    }
    // Each assigned tray is judged against its own kit's parts.
    let predicted: BTreeSet<String> = h
        .kit_assignments
        .iter()
        .filter(|ka| belief.tray_complete(&ka.tray_id, &ctx.goal.composition(ka.kit), ctx.goal))
        .map(|ka| ka.tray_id.clone())
        .collect();
    let claimed: BTreeSet<String> = h.expected_outcome.kits_complete.iter().cloned().collect();
    for t in claimed.difference(&predicted) {
        d.push(diag(
            DiagCategory::Feasibility,
            None,
            "$.expected_outcome.kits_complete",
            format!("claims {t} will be a complete kit, but the plan does not complete it"),
        ));
    }
    for t in predicted.difference(&claimed) {
        d.push(diag(
            DiagCategory::Feasibility,
            None,
            "$.expected_outcome.kits_complete",
            format!("plan completes {t} but does not claim it"),
        ));
    }
    if !d.is_empty() {
        return Err(d);
    }

    Ok(CompiledPlan {
        header: h.clone(),
        steps: ordered.into_iter().cloned().collect(),
        predicted_complete_trays: predicted.into_iter().collect(),
    })
}

/// Belief-level world + ledger model used for abstract execution.
struct Belief<'a> {
    loc: BTreeMap<String, String>,
    kind: BTreeMap<String, String>,
    visible_locs: BTreeMap<String, (LocationType, Option<String>, bool)>,
    withheld: Vec<String>,
    on_hand: KindCounts,
    reserved: KindCounts,
    /// Existing open reservations by id -> remaining.
    reservations: BTreeMap<String, KindCounts>,
    /// Reservations created by this plan: symbol -> (step_id, still_open).
    new_reservations: BTreeMap<String, (String, bool)>,
    consumed: BTreeSet<String>,
    /// Statically known step outputs (`step.output` -> value), e.g. pick/place echo `object_id`.
    outputs: BTreeMap<String, Value>,
    ctx: &'a CompileContext<'a>,
}

impl<'a> Belief<'a> {
    fn from(ctx: &'a CompileContext<'a>) -> Belief<'a> {
        let obs = ctx.observation;
        Belief {
            loc: obs
                .objects
                .iter()
                .map(|o| (o.object_id.clone(), o.location_id.clone()))
                .collect(),
            kind: obs
                .objects
                .iter()
                .map(|o| (o.object_id.clone(), effective_kind(o, ctx.classifications)))
                .collect(),
            visible_locs: obs
                .locations
                .iter()
                .map(|l| {
                    (
                        l.location_id.clone(),
                        (l.loc_type, l.accepts_kind.clone(), l.available),
                    )
                })
                .collect(),
            withheld: obs.withheld.clone(),
            on_hand: ctx.inventory.on_hand.clone(),
            reserved: ctx.inventory.reserved.clone(),
            reservations: ctx
                .inventory
                .open_reservations()
                .map(|r| (r.reservation_id.clone(), r.remaining.clone()))
                .collect(),
            new_reservations: BTreeMap::new(),
            consumed: ctx.consumed_evidence.iter().cloned().collect(),
            outputs: BTreeMap::new(),
            ctx,
        }
    }

    fn gripper(&self) -> Option<String> {
        self.loc
            .iter()
            .find(|(_, l)| *l == GRIPPER)
            .map(|(o, _)| o.clone())
    }

    fn tray_complete(&self, tray: &str, comp: &KindCounts, goal: &GoalSpec) -> bool {
        let mut c = KindCounts::new();
        for (o, l) in &self.loc {
            if l == tray {
                *c.entry(self.kind[o].clone()).or_default() += 1;
            }
        }
        !c.is_empty() && !comp.is_empty() && evaluate_against(&c, comp, goal).0
    }

    fn holds(&self, c: &Condition, goal_rev: u32) -> Result<(), String> {
        let seen = |l: &str| -> Result<(), String> {
            if self.withheld.iter().any(|w| w == l) {
                Err(format!("{l} was not observable in the planning snapshot"))
            } else {
                Ok(())
            }
        };
        match c {
            Condition::ObjectAt {
                object_id,
                location_id,
            } => {
                seen(location_id)?;
                match self.loc.get(object_id) {
                    Some(l) if l == location_id => Ok(()),
                    Some(l) => Err(format!("{object_id} would be at {l}")),
                    None => Err(format!("{object_id} is not in the observed workspace")),
                }
            }
            Condition::GripperEmpty => {
                seen(GRIPPER)?;
                match self.gripper() {
                    None => Ok(()),
                    Some(o) => Err(format!("gripper would hold {o}")),
                }
            }
            Condition::GripperHolds { object_id } => {
                seen(GRIPPER)?;
                match self.gripper() {
                    Some(o) if &o == object_id => Ok(()),
                    Some(o) => Err(format!("gripper would hold {o}")),
                    None => Err("gripper would be empty".into()),
                }
            }
            Condition::LocationAvailable { location_id } => {
                match self.visible_locs.get(location_id) {
                    Some((_, _, true)) => Ok(()),
                    Some((_, _, false)) => Err(format!("{location_id} is observed unavailable")),
                    None => Err(format!("{location_id} is not a known location")),
                }
            }
            Condition::QuantityAtLeast {
                location_id,
                part_kind,
                quantity,
            } => {
                seen(location_id)?;
                let n = self
                    .loc
                    .iter()
                    .filter(|(o, l)| *l == location_id && self.kind.get(*o) == Some(part_kind))
                    .count() as u32;
                if n >= *quantity {
                    Ok(())
                } else {
                    Err(format!("{location_id} would have {n} {part_kind}"))
                }
            }
            Condition::GoalRevisionEquals { revision } => {
                if *revision == goal_rev {
                    Ok(())
                } else {
                    Err(format!("current goal revision is {goal_rev}"))
                }
            }
        }
    }

    fn reservation_key(v: &Value) -> String {
        match ArgValue::parse(v) {
            Ok(ArgValue::Ref { step_id, output }) => format!("ref:{step_id}.{output}"),
            Ok(ArgValue::Literal(l)) => l.as_str().unwrap_or_default().to_string(),
            Err(_) => String::new(),
        }
    }

    fn apply(&mut self, s: &Step) -> Result<(), String> {
        // Resolve statically known references (e.g. `pick.object_id`) to literals.
        let a: BTreeMap<String, Value> = s
            .arguments
            .iter()
            .map(|(k, v)| match ArgValue::parse(v) {
                Ok(ArgValue::Ref { step_id, output }) => (
                    k.clone(),
                    self.outputs
                        .get(&format!("{step_id}.{output}"))
                        .cloned()
                        .unwrap_or_else(|| v.clone()),
                ),
                _ => (k.clone(), v.clone()),
            })
            .collect();
        let a = &a;
        let lit = |k: &str| {
            a.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let num = |k: &str| a.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
        match s.capability.as_str() {
            "workspace.pick" => {
                let (o, src) = (lit("object_id"), lit("source_id"));
                self.holds(&Condition::GripperEmpty, 0)
                    .map_err(|e| format!("pick {o}: {e}"))?;
                self.holds(
                    &Condition::ObjectAt {
                        object_id: o.clone(),
                        location_id: src.clone(),
                    },
                    0,
                )
                .map_err(|e| format!("pick {o}: {e}"))?;
                self.holds(
                    &Condition::LocationAvailable {
                        location_id: src.clone(),
                    },
                    0,
                )
                .map_err(|e| format!("pick {o}: {e}"))?;
                self.outputs
                    .insert(format!("{}.object_id", s.step_id), Value::String(o.clone()));
                self.loc.insert(o, GRIPPER.into());
            }
            "workspace.place" => {
                let (o, dst) = (lit("object_id"), lit("destination_id"));
                self.holds(
                    &Condition::GripperHolds {
                        object_id: o.clone(),
                    },
                    0,
                )
                .map_err(|e| format!("place {o}: {e}"))?;
                self.holds(
                    &Condition::LocationAvailable {
                        location_id: dst.clone(),
                    },
                    0,
                )
                .map_err(|e| format!("place {o}: {e}"))?;
                if let Some((LocationType::Bin, Some(acc), _)) = self.visible_locs.get(&dst) {
                    let k = self.kind.get(&o).cloned().unwrap_or_default();
                    if *acc != k {
                        return Err(format!(
                            "place {o}: bin {dst} accepts only {acc}, and {o} is {k}"
                        ));
                    }
                }
                if !matches!(
                    self.visible_locs.get(&dst),
                    Some((LocationType::Bin | LocationType::Tray, _, _))
                ) {
                    return Err(format!("place {o}: {dst} is not a bin or tray"));
                }
                self.outputs
                    .insert(format!("{}.object_id", s.step_id), Value::String(o.clone()));
                self.loc.insert(o, dst);
            }
            "inventory.reserve" => {
                let counts = counts_from_value(&a["counts"]);
                for (k, n) in &counts {
                    let free = self
                        .on_hand
                        .get(k)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(self.reserved.get(k).copied().unwrap_or(0));
                    if *n > free {
                        return Err(format!(
                            "reserve: {n} {k} requested but only {free} reservable in the inventory record"
                        ));
                    }
                    *self.reserved.entry(k.clone()).or_default() += n;
                }
                let sym = format!("ref:{}.reservation_id", s.step_id);
                self.reservations.insert(sym.clone(), counts);
                self.new_reservations.insert(sym, (s.step_id.clone(), true));
            }
            "inventory.consume" => {
                let rid = Self::reservation_key(&a["reservation_id"]);
                let (k, q, ev) = (lit("part_kind"), num("quantity"), lit("evidence_object_id"));
                let rem = self
                    .reservations
                    .get_mut(&rid)
                    .ok_or_else(|| format!("consume: reservation `{rid}` is not open"))?;
                let have = rem.get(&k).copied().unwrap_or(0);
                if have < q {
                    return Err(format!(
                        "consume: reservation `{rid}` has {have} {k} remaining, {q} needed"
                    ));
                }
                rem.insert(k.clone(), have - q);
                match (self.loc.get(&ev), self.kind.get(&ev)) {
                    (Some(l), Some(kk))
                        if kk == &k
                            && matches!(
                                self.visible_locs.get(l),
                                Some((LocationType::Tray, _, _))
                            ) => {}
                    _ => {
                        return Err(format!(
                            "consume: evidence {ev} would not be a {k} in a tray at this point"
                        ));
                    }
                }
                if !self.consumed.insert(ev.clone()) {
                    return Err(format!("consume: {ev} is already recorded as consumed"));
                }
                *self.on_hand.entry(k.clone()).or_default() -=
                    q.min(self.on_hand.get(&k).copied().unwrap_or(0));
                *self.reserved.entry(k).or_default() -= q;
            }
            "inventory.release" => {
                let rid = Self::reservation_key(&a["reservation_id"]);
                let rem = self
                    .reservations
                    .remove(&rid)
                    .ok_or_else(|| format!("release: reservation `{rid}` is not open"))?;
                for (k, n) in rem {
                    *self.reserved.entry(k).or_default() -= n;
                }
                if let Some(e) = self.new_reservations.get_mut(&rid) {
                    e.1 = false;
                }
            }
            "inventory.return" => {
                let (k, q, ev) = (lit("part_kind"), num("quantity"), lit("evidence_object_id"));
                if self.loc.get(&ev) != Some(&bin_for_kind(&k)) {
                    return Err(format!(
                        "return: {ev} would not be in {} at this point",
                        bin_for_kind(&k)
                    ));
                }
                if !self.consumed.remove(&ev) {
                    return Err(format!("return: {ev} was not recorded as consumed"));
                }
                *self.on_hand.entry(k).or_default() += q;
            }
            other => return Err(format!("no abstract semantics for `{other}`")),
        }
        let _ = self.ctx;
        Ok(())
    }
}
