//! Static checks for Revolve Script, before anything is compiled or run.
//!
//! Guarantees by construction (the language has no way to break them):
//! - it always finishes: no while loops, no recursion (checked here), and `for`
//!   loops run over a copy of a finite list taken when the loop starts;
//! - the robot is reached only through built-in actions (pick, place, move),
//!   each checked again by policy and the adapter at run time.
//!
//! Checked here, with a line number for each problem:
//! - every name, function, argument and keyword is known;
//! - attributes of parts and places exist;
//! - literal part kinds and place ids exist in the observed workspace, and
//!   literal destinations are allowed by the policy;
//! - the program states how success is checked (`expect` statements that look
//!   at the table).

use super::syntax::*;
use super::{Diag, Facts};
use crate::model::{KNOWN_KINDS, LocationType};
use std::collections::{BTreeMap, BTreeSet};

/// What the checker knows about the world and the policy.
#[derive(Debug, Clone)]
pub struct CheckEnv {
    /// (place id, type, accepted kind for bins)
    pub places: Vec<(String, LocationType, Option<String>)>,
    pub pick_sources: Vec<String>,
    pub place_destinations: Vec<String>,
}

impl CheckEnv {
    pub fn from(obs: &crate::model::Observation, policy: &crate::policy::Policy) -> CheckEnv {
        CheckEnv {
            places: obs
                .locations
                .iter()
                .filter(|l| {
                    matches!(
                        l.loc_type,
                        LocationType::Bin | LocationType::Tray | LocationType::Table
                    )
                })
                .map(|l| (l.location_id.clone(), l.loc_type, l.accepts_kind.clone()))
                .collect(),
            pick_sources: policy.pick_sources.clone(),
            place_destinations: policy.place_destinations.clone(),
        }
    }
    fn place_ids(&self) -> Vec<&str> {
        self.places.iter().map(|p| p.0.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    Str,
    Bool,
    NoneT,
    Part,
    Place,
    List(Box<Ty>),
    Any,
}

impl Ty {
    fn name(&self) -> String {
        match self {
            Ty::Int => "a number".into(),
            Ty::Str => "text".into(),
            Ty::Bool => "True/False".into(),
            Ty::NoneT => "None".into(),
            Ty::Part => "a part".into(),
            Ty::Place => "a place".into(),
            Ty::List(_) => "a list".into(),
            Ty::Any => "a value".into(),
        }
    }
    fn join(a: &Ty, b: &Ty) -> Ty {
        if a == b { a.clone() } else { Ty::Any }
    }
}

pub const PART_FIELDS: [&str; 4] = ["id", "kind", "at", "location"];
pub const PLACE_FIELDS: [&str; 5] = ["id", "kind", "accepts", "available", "count"];
pub const PLACE_KINDS: [&str; 3] = ["bin", "tray", "table"];

/// A built-in function: (name, min positional, max positional, keywords).
pub struct Builtin {
    pub name: &'static str,
    pub min: usize,
    pub max: usize,
    pub kw: &'static [&'static str],
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "len",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "range",
        min: 1,
        max: 2,
        kw: &[],
    },
    Builtin {
        name: "str",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "int",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "min",
        min: 1,
        max: 8,
        kw: &[],
    },
    Builtin {
        name: "max",
        min: 1,
        max: 8,
        kw: &[],
    },
    Builtin {
        name: "sum",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "abs",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "sorted",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "parts",
        min: 0,
        max: 1,
        kw: &["kind", "at"],
    },
    Builtin {
        name: "places",
        min: 0,
        max: 1,
        kw: &["kind"],
    },
    Builtin {
        name: "count",
        min: 0,
        max: 2,
        kw: &["at", "kind"],
    },
    Builtin {
        name: "holding",
        min: 0,
        max: 0,
        kw: &[],
    },
    Builtin {
        name: "pick",
        min: 1,
        max: 1,
        kw: &[],
    },
    Builtin {
        name: "place",
        min: 1,
        max: 2,
        kw: &["to"],
    },
    Builtin {
        name: "move",
        min: 1,
        max: 2,
        kw: &["to"],
    },
    Builtin {
        name: "ask",
        min: 1,
        max: 2,
        kw: &["options"],
    },
    Builtin {
        name: "say",
        min: 1,
        max: 8,
        kw: &[],
    },
    Builtin {
        name: "print",
        min: 1,
        max: 8,
        kw: &[],
    },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

pub const WORLD_QUERIES: [&str; 4] = ["parts", "places", "count", "holding"];

fn d(line: u32, col: u32, msg: impl Into<String>) -> Diag {
    Diag {
        line,
        col,
        message: msg.into(),
    }
}

fn closest<'a>(word: &str, options: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    fn dist(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut cur = vec![i; b.len() + 1];
            for j in 1..=b.len() {
                cur[j] = (prev[j] + 1)
                    .min(cur[j - 1] + 1)
                    .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
            }
            prev = cur;
        }
        prev[b.len()]
    }
    let options: Vec<&'a str> = options.into_iter().collect();
    if word.len() >= 3
        && let Some(p) = options
            .iter()
            .find(|o| o.starts_with(word) || word.starts_with(**o))
    {
        return Some(p);
    }
    options
        .into_iter()
        .map(|o| (dist(word, o), o))
        .filter(|(n, o)| *n <= 2.max(o.len() / 3))
        .min()
        .map(|(_, o)| o)
}

struct Checker<'a> {
    env: &'a CheckEnv,
    funcs: BTreeMap<String, usize>, // name -> arity
    globals: BTreeMap<String, Ty>,
    diags: Vec<Diag>,
    facts: Facts,
    calls: BTreeMap<String, BTreeSet<String>>, // user function -> user functions it calls
    cur_func: Option<String>,
    loop_depth: u32,
}

struct Scope {
    vars: BTreeMap<String, Ty>,
}

impl<'a> Checker<'a> {
    fn kinds_msg() -> String {
        format!(
            "{} (or \"unknown\" when a part was not identified)",
            KNOWN_KINDS.join(", ")
        )
    }

    fn check_kind_literal(&mut self, e: &Expr) {
        if let ExprKind::Str(s) = &e.kind
            && !KNOWN_KINDS.contains(&s.as_str())
            && s != "unknown"
        {
            let hint = closest(s, KNOWN_KINDS.iter().copied().chain(["unknown"]))
                .map(|c| format!(" (did you mean \"{c}\"?)"))
                .unwrap_or_default();
            self.diags.push(d(
                e.line,
                e.col,
                format!(
                    "no part kind is called \"{s}\"{hint}; kinds are {}",
                    Self::kinds_msg()
                ),
            ));
        }
    }

    fn check_place_literal(&mut self, e: &Expr, destination: bool) {
        let ExprKind::Str(s) = &e.kind else {
            if destination {
                self.facts.dynamic_destinations = true;
            }
            return;
        };
        let ids = self.env.place_ids();
        if !ids.contains(&s.as_str()) {
            let hint = closest(s, ids.iter().copied())
                .map(|c| format!(" (did you mean \"{c}\"?)"))
                .unwrap_or_default();
            self.diags.push(d(
                e.line,
                e.col,
                format!(
                    "there is no place called \"{s}\"{hint}; places are: {}",
                    ids.join(", ")
                ),
            ));
            return;
        }
        if destination {
            if !self
                .env
                .place_destinations
                .iter()
                .any(|p| s.starts_with(p.as_str()))
            {
                self.diags.push(d(
                    e.line,
                    e.col,
                    format!("the robot is not allowed to put things in \"{s}\""),
                ));
                return;
            }
            self.facts.literal_destinations.insert(s.clone());
        }
    }

    fn check_place_kind_literal(&mut self, e: &Expr) {
        if let ExprKind::Str(s) = &e.kind
            && !PLACE_KINDS.contains(&s.as_str())
        {
            self.diags.push(d(
                e.line,
                e.col,
                format!("no place kind is called \"{s}\"; place kinds are bin, tray, table"),
            ));
        }
    }

    fn lookup(&self, scope: &Scope, name: &str) -> Option<Ty> {
        scope
            .vars
            .get(name)
            .or_else(|| self.globals.get(name))
            .cloned()
    }

    fn expr(&mut self, scope: &mut Scope, e: &Expr) -> Ty {
        match &e.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Str(s) => {
                if self.env.places.iter().any(|p| &p.0 == s) {
                    self.facts.places_named.insert(s.clone());
                }
                Ty::Str
            }
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::None => Ty::NoneT,
            ExprKind::Name(n) => {
                if let Some(t) = self.lookup(scope, n) {
                    return t;
                }
                if self.funcs.contains_key(n) || builtin(n).is_some() {
                    self.diags.push(d(
                        e.line,
                        e.col,
                        format!("`{n}` is a function; call it with {n}(...)"),
                    ));
                    return Ty::Any;
                }
                let known: Vec<String> = scope
                    .vars
                    .keys()
                    .chain(self.globals.keys())
                    .cloned()
                    .collect();
                let hint = closest(n, known.iter().map(|s| s.as_str()))
                    .map(|c| format!(" (did you mean `{c}`?)"))
                    .unwrap_or_default();
                self.diags
                    .push(d(e.line, e.col, format!("`{n}` is not defined{hint}")));
                Ty::Any
            }
            ExprKind::List(items) => {
                let mut t: Option<Ty> = None;
                for i in items {
                    let it = self.expr(scope, i);
                    t = Some(match t {
                        None => it,
                        Some(p) => Ty::join(&p, &it),
                    });
                }
                Ty::List(Box::new(t.unwrap_or(Ty::Any)))
            }
            ExprKind::ListComp {
                elt,
                var,
                iter,
                cond,
            } => {
                let it = self.expr(scope, iter);
                let elem = self.elem_type(&it, iter);
                let saved = scope.vars.insert(var.clone(), elem);
                if let Some(c) = cond {
                    self.expr(scope, c);
                }
                let et = self.expr(scope, elt);
                match saved {
                    Some(s) => {
                        scope.vars.insert(var.clone(), s);
                    }
                    None => {
                        scope.vars.remove(var);
                    }
                }
                Ty::List(Box::new(et))
            }
            ExprKind::Attr(obj, name) => {
                let t = self.expr(scope, obj);
                match t {
                    Ty::Part => {
                        if !PART_FIELDS.contains(&name.as_str()) {
                            self.diags.push(d(e.line, e.col, format!("a part has no `{name}`; parts have .id, .kind and .at (where it is)")));
                        }
                        Ty::Str
                    }
                    Ty::Place => match name.as_str() {
                        "id" | "kind" => Ty::Str,
                        "accepts" => Ty::Any,
                        "available" => Ty::Bool,
                        "count" => Ty::Int,
                        _ => {
                            self.diags.push(d(e.line, e.col, format!("a place has no `{name}`; places have .id, .kind, .accepts, .available and .count")));
                            Ty::Any
                        }
                    },
                    Ty::List(_) | Ty::Int | Ty::Bool | Ty::NoneT => {
                        self.diags
                            .push(d(e.line, e.col, format!("{} has no `{name}`", t.name())));
                        Ty::Any
                    }
                    _ => Ty::Any,
                }
            }
            ExprKind::Index(obj, idx) => {
                let t = self.expr(scope, obj);
                let it = self.expr(scope, idx);
                if !matches!(it, Ty::Int | Ty::Any) {
                    self.diags
                        .push(d(idx.line, idx.col, "list positions must be whole numbers"));
                }
                match t {
                    Ty::List(inner) => *inner,
                    Ty::Str => Ty::Str,
                    Ty::Any => Ty::Any,
                    other => {
                        self.diags.push(d(
                            e.line,
                            e.col,
                            format!("{} cannot be indexed with [...]", other.name()),
                        ));
                        Ty::Any
                    }
                }
            }
            ExprKind::Call { func, args, kwargs } => self.call(scope, e, func, args, kwargs),
            ExprKind::Bin(op, a, b) => {
                let (ta, tb) = (self.expr(scope, a), self.expr(scope, b));
                match (op, &ta, &tb) {
                    (BinOp::Add, Ty::Str, _) | (BinOp::Add, _, Ty::Str) => {
                        if !matches!((&ta, &tb), (Ty::Str | Ty::Any, Ty::Str | Ty::Any)) {
                            self.diags.push(d(
                                e.line,
                                e.col,
                                "text can only be joined with text; use str(value)",
                            ));
                        }
                        Ty::Str
                    }
                    (BinOp::Add, Ty::List(x), Ty::List(y)) => Ty::List(Box::new(Ty::join(x, y))),
                    (_, Ty::Int | Ty::Any, Ty::Int | Ty::Any) => Ty::Int,
                    _ => {
                        self.diags.push(d(
                            e.line,
                            e.col,
                            format!("cannot do arithmetic with {} and {}", ta.name(), tb.name()),
                        ));
                        Ty::Any
                    }
                }
            }
            ExprKind::Cmp(op, a, b) => {
                let (ta, tb) = (self.expr(scope, a), self.expr(scope, b));
                // `part.kind == "red"`: check the literal against the known kinds
                let kind_attr = |x: &Expr, t: &Ty| {
                    matches!(&x.kind, ExprKind::Attr(o, n) if n == "kind") && *t == Ty::Str
                };
                for (x, y) in [(a, b), (b, a)] {
                    if let ExprKind::Attr(o, n) = &x.kind
                        && n == "kind"
                    {
                        let ot = self.expr_quiet(scope, o);
                        if kind_attr(x, &Ty::Str) {
                            match (&ot, &y.kind, op) {
                                (Ty::Part, ExprKind::Str(_), CmpOp::Eq | CmpOp::Ne) => {
                                    self.check_kind_literal(y)
                                }
                                (Ty::Part, ExprKind::List(items), CmpOp::In | CmpOp::NotIn) => {
                                    for i in items.clone() {
                                        self.check_kind_literal(&i);
                                    }
                                }
                                (Ty::Place, ExprKind::Str(_), CmpOp::Eq | CmpOp::Ne) => {
                                    self.check_place_kind_literal(y)
                                }
                                _ => {}
                            }
                        }
                    }
                }
                let _ = (ta, tb);
                Ty::Bool
            }
            ExprKind::And(a, b) | ExprKind::Or(a, b) => {
                let (ta, tb) = (self.expr(scope, a), self.expr(scope, b));
                Ty::join(&ta, &tb)
            }
            ExprKind::Not(a) => {
                self.expr(scope, a);
                Ty::Bool
            }
            ExprKind::Neg(a) => {
                let t = self.expr(scope, a);
                if !matches!(t, Ty::Int | Ty::Any) {
                    self.diags
                        .push(d(e.line, e.col, format!("cannot negate {}", t.name())));
                }
                Ty::Int
            }
            ExprKind::IfExp(a, c, b) => {
                self.expr(scope, c);
                let (ta, tb) = (self.expr(scope, a), self.expr(scope, b));
                Ty::join(&ta, &tb)
            }
        }
    }

    /// Type of an expression without reporting anything (used for context).
    fn expr_quiet(&mut self, scope: &mut Scope, e: &Expr) -> Ty {
        let n = self.diags.len();
        let t = self.expr(scope, e);
        self.diags.truncate(n);
        t
    }

    fn elem_type(&mut self, t: &Ty, at: &Expr) -> Ty {
        match t {
            Ty::List(inner) => (**inner).clone(),
            Ty::Str => Ty::Str,
            Ty::Any => Ty::Any,
            other => {
                self.diags.push(d(
                    at.line,
                    at.col,
                    format!("cannot loop over {}; loop over a list", other.name()),
                ));
                Ty::Any
            }
        }
    }

    fn call(
        &mut self,
        scope: &mut Scope,
        e: &Expr,
        func: &Expr,
        args: &[Expr],
        kwargs: &[(String, Expr)],
    ) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.expr(scope, a)).collect();
        for (_, v) in kwargs {
            self.expr(scope, v);
        }
        match &func.kind {
            ExprKind::Attr(recv, method) => {
                let rt = self.expr(scope, recv);
                match (&rt, method.as_str()) {
                    (Ty::List(_) | Ty::Any, "append") if args.len() == 1 && kwargs.is_empty() => {
                        Ty::NoneT
                    }
                    (Ty::Str | Ty::Any, "startswith" | "endswith") if args.len() == 1 => Ty::Bool,
                    (Ty::Str | Ty::Any, "lower" | "upper" | "strip") if args.is_empty() => Ty::Str,
                    (Ty::Part | Ty::Place, _) => {
                        self.diags.push(d(e.line, e.col, format!("{} has no methods; use move(part, \"place\") and the fields .id, .kind, .at", rt.name())));
                        Ty::Any
                    }
                    (Ty::List(_), m) => {
                        self.diags.push(d(e.line, e.col, format!("lists support only .append(x) (not .{m}); use len(), sorted() and for loops")));
                        Ty::Any
                    }
                    (_, m) => {
                        self.diags.push(d(e.line, e.col, format!("unknown method .{m}(); available: list.append(x), text.startswith/endswith/lower/upper/strip")));
                        Ty::Any
                    }
                }
            }
            ExprKind::Name(name) => {
                if let Some(&arity) = self.funcs.get(name) {
                    if !kwargs.is_empty() {
                        self.diags.push(d(
                            e.line,
                            e.col,
                            format!("{name}() takes positional arguments only"),
                        ));
                    }
                    if args.len() != arity {
                        self.diags.push(d(
                            e.line,
                            e.col,
                            format!("{name}() takes {arity} argument(s), {} given", args.len()),
                        ));
                    }
                    if let Some(cur) = &self.cur_func {
                        self.calls
                            .entry(cur.clone())
                            .or_default()
                            .insert(name.clone());
                    }
                    return Ty::Any;
                }
                let Some(b) = builtin(name) else {
                    let names: Vec<&str> = BUILTINS
                        .iter()
                        .map(|b| b.name)
                        .chain(self.funcs.keys().map(|s| s.as_str()))
                        .collect();
                    let hint = closest(name, names)
                        .map(|c| format!(" (did you mean {c}()?)"))
                        .unwrap_or_default();
                    self.diags
                        .push(d(e.line, e.col, format!("unknown function {name}(){hint}")));
                    return Ty::Any;
                };
                if args.len() < b.min || args.len() > b.max {
                    self.diags.push(d(
                        e.line,
                        e.col,
                        format!(
                            "{name}() takes {} argument(s), {} given",
                            if b.min == b.max {
                                b.min.to_string()
                            } else {
                                format!("{} to {}", b.min, b.max)
                            },
                            args.len()
                        ),
                    ));
                }
                for (k, v) in kwargs {
                    if !b.kw.contains(&k.as_str()) {
                        self.diags.push(d(
                            v.line,
                            v.col,
                            format!(
                                "{name}() has no keyword `{k}`{}",
                                if b.kw.is_empty() {
                                    String::new()
                                } else {
                                    format!("; keywords: {}", b.kw.join(", "))
                                }
                            ),
                        ));
                    }
                }
                let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v);
                let pos_or_kw = |i: usize, k: &str| args.get(i).or_else(|| kw(k));
                match name.as_str() {
                    "parts" => {
                        self.facts.effects.insert("look".into());
                        if let Some(k) = pos_or_kw(0, "kind") {
                            self.check_kind_literal(k);
                        }
                        if let Some(a) = kw("at") {
                            self.check_place_literal(a, false);
                        }
                        Ty::List(Box::new(Ty::Part))
                    }
                    "places" => {
                        self.facts.effects.insert("look".into());
                        if let Some(k) = pos_or_kw(0, "kind") {
                            self.check_place_kind_literal(k);
                        }
                        Ty::List(Box::new(Ty::Place))
                    }
                    "count" => {
                        self.facts.effects.insert("look".into());
                        match pos_or_kw(0, "at") {
                            Some(a) => self.check_place_literal(a, false),
                            None => self.diags.push(d(e.line, e.col, "count() needs a place: count(\"tray-1\") or count(\"tray-1\", kind=\"washer\")")),
                        }
                        if let Some(k) = pos_or_kw(1, "kind") {
                            self.check_kind_literal(k);
                        }
                        Ty::Int
                    }
                    "holding" => {
                        self.facts.effects.insert("look".into());
                        Ty::Any
                    }
                    "pick" | "place" | "move" => {
                        self.facts.effects.insert(name.to_string());
                        if let Some(t) = arg_tys.first()
                            && !matches!(t, Ty::Part | Ty::Any)
                        {
                            self.diags.push(d(
                                e.line,
                                e.col,
                                format!(
                                    "{name}() needs a part (from parts() or holding()), not {}",
                                    t.name()
                                ),
                            ));
                        }
                        if name != "pick" {
                            match pos_or_kw(1, "to") {
                                Some(to) => self.check_place_literal(to, true),
                                None => self.diags.push(d(
                                    e.line,
                                    e.col,
                                    format!(
                                        "{name}() needs a destination: {name}(part, \"tray-1\")"
                                    ),
                                )),
                            }
                        }
                        Ty::Bool
                    }
                    "ask" => {
                        self.facts.effects.insert("ask".into());
                        if let Some(Expr {
                            kind: ExprKind::Str(q),
                            ..
                        }) = args.first()
                        {
                            self.facts.questions.push(q.clone());
                        }
                        match pos_or_kw(1, "options") {
                            Some(Expr {
                                kind: ExprKind::List(items),
                                ..
                            }) if items.len() >= 2 => {}
                            Some(Expr {
                                kind: ExprKind::List(_),
                                line,
                                col,
                            }) => {
                                self.diags
                                    .push(d(*line, *col, "ask() needs at least two options"))
                            }
                            Some(_) => {}
                            None => self.diags.push(d(
                                e.line,
                                e.col,
                                "ask() needs options: ask(\"question?\", [\"yes\", \"no\"])",
                            )),
                        }
                        Ty::Str
                    }
                    "say" | "print" => {
                        self.facts.effects.insert("say".into());
                        Ty::NoneT
                    }
                    "len" | "sum" | "abs" | "int" => Ty::Int,
                    "str" => Ty::Str,
                    "range" => {
                        for a in args {
                            if let ExprKind::Int(n) = a.kind
                                && n > super::MAX_RANGE
                            {
                                self.diags.push(d(
                                    a.line,
                                    a.col,
                                    format!("range() is limited to {} items", super::MAX_RANGE),
                                ));
                            }
                        }
                        Ty::List(Box::new(Ty::Int))
                    }
                    "sorted" => arg_tys.first().cloned().unwrap_or(Ty::Any),
                    "min" | "max" => match arg_tys.as_slice() {
                        [Ty::List(inner)] => (**inner).clone(),
                        [one] => one.clone(),
                        _ => Ty::Any,
                    },
                    _ => Ty::Any,
                }
            }
            _ => {
                self.diags
                    .push(d(e.line, e.col, "only functions can be called"));
                Ty::Any
            }
        }
    }

    fn assign(&mut self, scope: &mut Scope, name: &str, t: Ty, line: u32) {
        if builtin(name).is_some() || self.funcs.contains_key(name) {
            self.diags.push(d(
                line,
                1,
                format!("`{name}` is a function name; choose another variable name"),
            ));
        }
        let target = if self.cur_func.is_some() {
            &mut scope.vars
        } else {
            &mut self.globals
        };
        let joined = match target.get(name) {
            Some(prev) => Ty::join(prev, &t),
            None => t,
        };
        target.insert(name.to_string(), joined);
    }

    fn stmts(&mut self, scope: &mut Scope, body: &[Stmt]) {
        for s in body {
            self.stmt(scope, s);
        }
    }

    fn stmt(&mut self, scope: &mut Scope, s: &Stmt) {
        match &s.kind {
            StmtKind::Assign(n, e) => {
                let t = self.expr(scope, e);
                self.assign(scope, n, t, s.line);
            }
            StmtKind::AugAssign(n, _, e) => {
                if self.lookup(scope, n).is_none() {
                    self.diags.push(d(
                        s.line,
                        1,
                        format!("`{n}` must be given a value before it is updated"),
                    ));
                }
                let t = self.expr(scope, e);
                let prev = self.lookup(scope, n).unwrap_or(Ty::Any);
                self.assign(scope, n, Ty::join(&prev, &t), s.line);
            }
            StmtKind::If(branches, orelse) => {
                for (c, b) in branches {
                    self.expr(scope, c);
                    self.stmts(scope, b);
                }
                self.stmts(scope, orelse);
            }
            StmtKind::For(var, iter, body) => {
                self.facts.loops += 1;
                let it = self.expr(scope, iter);
                let elem = self.elem_type(&it, iter);
                self.assign(scope, var, elem, s.line);
                self.loop_depth += 1;
                self.stmts(scope, body);
                self.loop_depth -= 1;
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.loop_depth == 0 {
                    self.diags
                        .push(d(s.line, 1, "break/continue must be inside a for loop"));
                }
            }
            StmtKind::Pass => {}
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.expr(scope, e);
                }
            }
            StmtKind::Expect(c, desc) => {
                if !looks_at_world(c) {
                    self.diags.push(d(s.line, 1, "an expectation must look at the table when it is checked: call count(), parts(), places() or holding() inside it"));
                }
                self.expr(scope, c);
                self.facts.expectations.push(desc.clone());
            }
            StmtKind::Expr(e) => {
                if !matches!(e.kind, ExprKind::Call { .. }) {
                    self.diags.push(d(
                        s.line,
                        e.col,
                        "this line computes a value but does nothing with it",
                    ));
                }
                self.expr(scope, e);
            }
        }
    }
}

fn looks_at_world(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { func, args, kwargs } => {
            matches!(&func.kind, ExprKind::Name(n) if WORLD_QUERIES.contains(&n.as_str()))
                || looks_at_world(func)
                || args.iter().any(looks_at_world)
                || kwargs.iter().any(|(_, v)| looks_at_world(v))
        }
        ExprKind::List(items) => items.iter().any(looks_at_world),
        ExprKind::ListComp {
            elt, iter, cond, ..
        } => {
            looks_at_world(elt)
                || looks_at_world(iter)
                || cond.as_ref().is_some_and(|c| looks_at_world(c))
        }
        ExprKind::Attr(a, _) | ExprKind::Not(a) | ExprKind::Neg(a) => looks_at_world(a),
        ExprKind::Index(a, b)
        | ExprKind::Bin(_, a, b)
        | ExprKind::Cmp(_, a, b)
        | ExprKind::And(a, b)
        | ExprKind::Or(a, b) => looks_at_world(a) || looks_at_world(b),
        ExprKind::IfExp(a, c, b) => looks_at_world(a) || looks_at_world(c) || looks_at_world(b),
        _ => false,
    }
}

/// Check a parsed program. Returns the facts shown to the person, or every problem found.
pub fn check(p: &Program, env: &CheckEnv) -> Result<Facts, Vec<Diag>> {
    let mut c = Checker {
        env,
        funcs: BTreeMap::new(),
        globals: BTreeMap::new(),
        diags: vec![],
        facts: Facts::default(),
        calls: BTreeMap::new(),
        cur_func: None,
        loop_depth: 0,
    };
    for f in &p.funcs {
        if builtin(&f.name).is_some() {
            c.diags.push(d(
                f.line,
                1,
                format!("`{}` is a built-in function; choose another name", f.name),
            ));
        }
        if c.funcs.insert(f.name.clone(), f.params.len()).is_some() {
            c.diags.push(d(
                f.line,
                1,
                format!("function `{}` is defined twice", f.name),
            ));
        }
        let mut seen = BTreeSet::new();
        for prm in &f.params {
            if !seen.insert(prm) {
                c.diags
                    .push(d(f.line, 1, format!("parameter `{prm}` appears twice")));
            }
        }
    }
    c.facts.functions = p.funcs.iter().map(|f| f.name.clone()).collect();
    // Top-level code first (defines the globals functions may read).
    let mut top = Scope {
        vars: BTreeMap::new(),
    };
    c.stmts(&mut top, &p.body);
    for f in &p.funcs {
        c.cur_func = Some(f.name.clone());
        let mut scope = Scope {
            vars: f.params.iter().map(|p| (p.clone(), Ty::Any)).collect(),
        };
        // Pre-declare names assigned anywhere in the function (Python scoping).
        c.stmts(&mut scope, &f.body);
        c.cur_func = None;
    }
    // No recursion, direct or indirect.
    for f in &p.funcs {
        let mut stack = vec![f.name.clone()];
        let mut seen = BTreeSet::new();
        while let Some(g) = stack.pop() {
            for h in c.calls.get(&g).cloned().unwrap_or_default() {
                if h == f.name {
                    c.diags.push(d(f.line, 1, format!("function `{}` calls itself (directly or through other functions); recursion is not allowed so programs always finish", f.name)));
                    stack.clear();
                    break;
                }
                if seen.insert(h.clone()) {
                    stack.push(h);
                }
            }
        }
    }
    if c.facts.expectations.is_empty() {
        fn last_line(body: &[Stmt]) -> u32 {
            body.iter()
                .map(|s| match &s.kind {
                    StmtKind::For(_, _, b) => last_line(b).max(s.line),
                    StmtKind::If(br, orelse) => br
                        .iter()
                        .map(|(_, b)| last_line(b))
                        .chain([last_line(orelse), s.line])
                        .max()
                        .unwrap_or(s.line),
                    _ => s.line,
                })
                .max()
                .unwrap_or(0)
        }
        let last = last_line(&p.body).max(
            p.funcs
                .iter()
                .map(|f| last_line(&f.body))
                .max()
                .unwrap_or(0),
        ) + 1;
        c.diags.push(d(last, 1, "the program must say how success is checked: add at least one `expect <condition>, \"description\"` that looks at the table"));
    }
    if c.diags.is_empty() {
        Ok(c.facts)
    } else {
        c.diags.sort_by_key(|d| (d.line, d.col));
        c.diags.dedup();
        Err(c.diags)
    }
}
