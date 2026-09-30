//! Runs a compiled Revolve Script program under the durable executor.
//!
//! Every call that reaches outside the program (looking at the table, pick,
//! place, asking the person, saying something, checking an expectation) is one
//! entry in the run's durable call log (the plan's action rows, in order). Each
//! run of the program starts at the top. Calls already in the log return their
//! recorded results, so the program retraces exactly what it did. At the first
//! call that is not in the log, the host records the request and stops the
//! program. The executor then carries out that one call through its normal path
//! and runs the program again.
//!
//! Determinism is checked: a replayed call must match the logged one
//! (capability and arguments). Because everything the program sees comes from
//! the log, a correct program always retraces the same path.

use super::codegen::{FALSE, NONE, PURE_BUILTINS, QUERIES, TRUE};
use crate::store::*;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};

const FUEL: u64 = 50_000_000;
const MAX_VALUES: usize = 200_000;
/// Outside-world calls one program may make (looks, moves, questions, checks).
pub const MAX_CALLS: usize = 600;

#[derive(Debug, Clone, PartialEq)]
pub enum ScriptStep {
    /// The program finished; every call it made is in the log.
    Done,
    /// A call that is not in the log yet: record it and carry it out.
    NewCall {
        index: usize,
        capability: String,
        args: Value,
        line: u32,
    },
    /// A logged call that has not been carried out yet (e.g. after a crash).
    Resume { action_id: i64 },
    /// The program failed at a line (a bad value, an index out of range, ...).
    Error { line: u32, message: String },
    /// The program tried something the policy forbids.
    Forbidden { line: u32, message: String },
    /// The program did not retrace its own log.
    Diverged(String),
    /// Stopped abnormally (fuel, memory).
    Trapped(String),
    /// Refused before running (imports).
    Refused(String),
}

pub struct State {
    log: Vec<ActionRow>,
    values: Vec<Value>,
    next_call: usize,
    outcome: Option<ScriptStep>,
}

type R<T> = wasmtime::Result<T>;

fn err(line: i32, msg: impl std::fmt::Display) -> wasmtime::Error {
    wasmtime::Error::msg(format!("line {line}: {msg}"))
}

fn is_part(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("part")
}
fn is_place(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("place")
}

/// How a value reads in messages and in str().
pub fn show(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(|x| match x {
                    Value::String(s) => format!("\"{s}\""),
                    other => show(other),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(o) => o
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "None",
        Value::Bool(_) => "True/False",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) if is_part(v) => "a part",
        Value::Object(_) if is_place(v) => "a place",
        Value::Object(_) => "a value",
    }
}

fn same(a: &Value, b: &Value) -> bool {
    // Parts and places are the same thing when their ids match.
    if (is_part(a) && is_part(b)) || (is_place(a) && is_place(b)) {
        return a.get("id") == b.get("id");
    }
    a == b
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64() != Some(0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64()
}

impl State {
    fn new(log: Vec<ActionRow>) -> State {
        State {
            log,
            values: vec![Value::Null, Value::Bool(true), Value::Bool(false)],
            next_call: 0,
            outcome: None,
        }
    }
    fn push(&mut self, v: Value) -> R<i32> {
        match v {
            Value::Null => return Ok(NONE),
            Value::Bool(true) => return Ok(TRUE),
            Value::Bool(false) => return Ok(FALSE),
            _ => {}
        }
        if self.values.len() >= MAX_VALUES {
            return Err(wasmtime::Error::msg("the program created too many values"));
        }
        self.values.push(v);
        Ok(self.values.len() as i32 - 1)
    }
    fn get(&self, h: i32) -> R<&Value> {
        usize::try_from(h)
            .ok()
            .and_then(|h| self.values.get(h))
            .ok_or_else(|| wasmtime::Error::msg(format!("bad value handle {h}")))
    }
    fn stop(&mut self, s: ScriptStep) -> i32 {
        if self.outcome.is_none() {
            self.outcome = Some(s);
        } else {
            self.outcome = Some(ScriptStep::Diverged(
                "the program continued after it was told to stop".into(),
            ));
        }
        -1
    }
    fn fail(&mut self, line: i32, message: impl Into<String>) -> wasmtime::Error {
        let message = message.into();
        if self.outcome.is_none() {
            self.outcome = Some(ScriptStep::Error {
                line: line as u32,
                message: message.clone(),
            });
        }
        err(line, message)
    }

    /// One outside-world call: replay it from the log, or stop and request it.
    fn ext(
        &mut self,
        capability: &str,
        args: Value,
        line: i32,
    ) -> Result<Result<&ActionRow, i32>, wasmtime::Error> {
        if self.outcome.is_some() {
            return Ok(Err(self.stop(ScriptStep::Done)));
        }
        let k = self.next_call;
        self.next_call += 1;
        if k >= MAX_CALLS {
            return Err(self.fail(
                line,
                format!("the program made more than {MAX_CALLS} calls to the outside world"),
            ));
        }
        let Some(row) = self.log.get(k) else {
            return Ok(Err(self.stop(ScriptStep::NewCall {
                index: k,
                capability: capability.into(),
                args,
                line: line as u32,
            })));
        };
        if row.capability != capability || row.arguments != args {
            let msg = format!(
                "call #{k} at line {line} is {capability} {args}, but the log recorded {} {}",
                row.capability, row.arguments
            );
            return Ok(Err(self.stop(ScriptStep::Diverged(msg))));
        }
        match row.status.as_str() {
            A_SUCCEEDED | A_FAILED => Ok(Ok(&self.log[k])),
            A_DENIED => {
                let m = row.error.clone().unwrap_or_default();
                Ok(Err(self.stop(ScriptStep::Forbidden {
                    line: line as u32,
                    message: m,
                })))
            }
            A_PENDING | A_RETRY_PENDING => {
                let id = row.action_id;
                Ok(Err(self.stop(ScriptStep::Resume { action_id: id })))
            }
            other => {
                let msg = format!(
                    "call #{k} ({capability}) has status {other}; its outcome must be settled first"
                );
                Ok(Err(self.stop(ScriptStep::Diverged(msg))))
            }
        }
    }

    fn observation(&mut self, query: &str, line: i32) -> R<Result<Value, i32>> {
        match self.ext("workspace.observe", json!({"query": query}), line)? {
            Err(stop) => Ok(Err(stop)),
            Ok(row) if row.status == A_SUCCEEDED => {
                Ok(Ok(row.result.clone().unwrap_or(Value::Null)))
            }
            Ok(row) => {
                let m = format!(
                    "could not look at the table: {}",
                    row.error.clone().unwrap_or_default()
                );
                Err(self.fail(line, m))
            }
        }
    }
}

fn read(caller: &mut Caller<'_, State>, ptr: i32, len: i32) -> R<String> {
    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
        return Err(wasmtime::Error::msg("program exports no memory"));
    };
    let (p, l) = (ptr as u32 as usize, len as u32 as usize);
    let bytes = mem
        .data(&caller)
        .get(p..p.saturating_add(l))
        .ok_or_else(|| wasmtime::Error::msg("text outside program memory"))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| wasmtime::Error::msg("text is not UTF-8"))
}

fn place_id(st: &mut State, v: &Value, what: &str, line: i32) -> R<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        v if is_place(v) => Ok(v["id"].as_str().unwrap_or_default().to_string()),
        other => Err(st.fail(
            line,
            format!(
                "{what} must be a place id like \"tray-1\" or a place, not {}",
                type_name(other)
            ),
        )),
    }
}

// ------------------------------------------------------------------ world views

fn part_value(o: &Value) -> Value {
    json!({"type": "part", "id": o["object_id"], "kind": o["observed_kind"], "at": o["location_id"]})
}

fn visible_places(obs: &Value) -> Vec<&Value> {
    obs["locations"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|l| matches!(l["loc_type"].as_str(), Some("bin" | "tray" | "table")))
                .collect()
        })
        .unwrap_or_default()
}

fn objects(obs: &Value) -> Vec<&Value> {
    obs["objects"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn query(
    st: &mut State,
    q: &str,
    obs: &Value,
    args: &[Value],
    kw: &Map<String, Value>,
    line: i32,
) -> R<Value> {
    let arg = |i: usize, k: &str| args.get(i).or_else(|| kw.get(k)).filter(|v| !v.is_null());
    match q {
        "parts" => {
            let kind = match arg(0, "kind") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => {
                    return Err(st.fail(
                        line,
                        format!(
                            "parts(kind=...) needs text like \"red_block\", not {}",
                            type_name(other)
                        ),
                    ));
                }
                None => None,
            };
            let at = match kw.get("at").filter(|v| !v.is_null()) {
                Some(v) => Some(place_id(st, &v.clone(), "parts(at=...)", line)?),
                None => None,
            };
            let list: Vec<Value> = objects(obs)
                .into_iter()
                .filter(|o| o["location_id"].as_str() != Some("outside"))
                .filter(|o| {
                    kind.as_deref()
                        .is_none_or(|k| o["observed_kind"].as_str() == Some(k))
                })
                .filter(|o| {
                    at.as_deref()
                        .is_none_or(|a| o["location_id"].as_str() == Some(a))
                })
                .map(part_value)
                .collect();
            Ok(Value::Array(list))
        }
        "places" => {
            let kind = match arg(0, "kind") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => {
                    return Err(st.fail(
                        line,
                        format!(
                            "places(kind=...) needs \"bin\", \"tray\" or \"table\", not {}",
                            type_name(other)
                        ),
                    ));
                }
                None => None,
            };
            let objs = objects(obs);
            let list: Vec<Value> = visible_places(obs)
                .into_iter()
                .filter(|l| kind.as_deref().is_none_or(|k| l["loc_type"].as_str() == Some(k)))
                .map(|l| {
                    let id = l["location_id"].as_str().unwrap_or_default();
                    let n = objs.iter().filter(|o| o["location_id"].as_str() == Some(id)).count();
                    json!({"type": "place", "id": id, "kind": l["loc_type"], "accepts": l["accepts_kind"], "available": l["available"], "count": n})
                })
                .collect();
            Ok(Value::Array(list))
        }
        "count" => {
            let Some(at) = arg(0, "at").cloned() else {
                return Err(st.fail(line, "count() needs a place"));
            };
            let at = place_id(st, &at, "count()", line)?;
            let kind = match arg(1, "kind") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => {
                    return Err(st.fail(
                        line,
                        format!("count(kind=...) needs text, not {}", type_name(other)),
                    ));
                }
                None => None,
            };
            let n = objects(obs)
                .into_iter()
                .filter(|o| o["location_id"].as_str() == Some(at.as_str()))
                .filter(|o| {
                    kind.as_deref()
                        .is_none_or(|k| o["observed_kind"].as_str() == Some(k))
                })
                .count();
            Ok(json!(n))
        }
        _ => {
            // holding()
            Ok(objects(obs)
                .into_iter()
                .find(|o| o["location_id"].as_str() == Some(crate::model::GRIPPER))
                .map(part_value)
                .unwrap_or(Value::Null))
        }
    }
}

// ------------------------------------------------------------------ pure operations

fn binop(st: &mut State, op: i32, a: &Value, b: &Value, line: i32) -> R<Value> {
    let ints = |a: &Value, b: &Value| int(a).zip(int(b));
    let checked = |r: Option<i64>, st: &mut State| {
        r.map(Value::from)
            .ok_or_else(|| st.fail(line, "number is too large"))
    };
    Ok(match op {
        0 => match (a, b) {
            (Value::String(x), Value::String(y)) => Value::String(format!("{x}{y}")),
            (Value::Array(x), Value::Array(y)) => {
                Value::Array(x.iter().chain(y.iter()).cloned().collect())
            }
            _ => match ints(a, b) {
                Some((x, y)) => checked(x.checked_add(y), st)?,
                None => {
                    return Err(st.fail(
                        line,
                        format!(
                            "cannot add {} and {} (to join text use str(value))",
                            type_name(a),
                            type_name(b)
                        ),
                    ));
                }
            },
        },
        1..=4 => {
            let Some((x, y)) = ints(a, b) else {
                return Err(st.fail(
                    line,
                    format!(
                        "arithmetic needs numbers, not {} and {}",
                        type_name(a),
                        type_name(b)
                    ),
                ));
            };
            if (op == 3 || op == 4) && y == 0 {
                return Err(st.fail(line, "division by zero"));
            }
            checked(
                match op {
                    1 => x.checked_sub(y),
                    2 => x.checked_mul(y),
                    3 => Some(x.div_euclid(y)),
                    _ => Some(x.rem_euclid(y)),
                },
                st,
            )?
        }
        5 => Value::Bool(same(a, b)),
        6 => Value::Bool(!same(a, b)),
        7..=10 => {
            let ord = match (a, b) {
                (Value::String(x), Value::String(y)) => x.cmp(y),
                _ => match ints(a, b) {
                    Some((x, y)) => x.cmp(&y),
                    None => {
                        return Err(st.fail(
                            line,
                            format!("cannot compare {} with {}", type_name(a), type_name(b)),
                        ));
                    }
                },
            };
            Value::Bool(match op {
                7 => ord.is_lt(),
                8 => ord.is_le(),
                9 => ord.is_gt(),
                _ => ord.is_ge(),
            })
        }
        _ => {
            let found = match b {
                Value::Array(items) => items.iter().any(|x| same(a, x)),
                Value::String(s) => match a {
                    Value::String(sub) => s.contains(sub.as_str()),
                    _ => return Err(st.fail(line, "`in` on text needs text on the left")),
                },
                other => {
                    return Err(st.fail(
                        line,
                        format!(
                            "`in` needs a list or text on the right, not {}",
                            type_name(other)
                        ),
                    ));
                }
            };
            Value::Bool(if op == 11 { found } else { !found })
        }
    })
}

fn builtin(st: &mut State, id: i32, args: &[Value], line: i32) -> R<Value> {
    let name = PURE_BUILTINS.get(id as usize).copied().unwrap_or("?");
    let items = |st: &mut State, args: &[Value]| -> R<Vec<Value>> {
        match args {
            [Value::Array(a)] => Ok(a.clone()),
            [one] if !matches!(one, Value::Array(_)) && (name == "min" || name == "max") => {
                Err(st.fail(line, format!("{name}() needs a list or several values")))
            }
            many => Ok(many.to_vec()),
        }
    };
    Ok(match name {
        "len" => match &args[0] {
            Value::Array(a) => json!(a.len()),
            Value::String(s) => json!(s.chars().count()),
            other => {
                return Err(st.fail(
                    line,
                    format!("len() needs a list or text, not {}", type_name(other)),
                ));
            }
        },
        "range" => {
            let (lo, hi) = match args {
                [n] => (0, int(n)),
                [a, b] => (int(a).unwrap_or(i64::MIN), int(b)),
                _ => (0, None),
            };
            let (Some(hi), true) = (hi, lo != i64::MIN) else {
                return Err(st.fail(line, "range() needs whole numbers"));
            };
            if hi - lo > super::MAX_RANGE {
                return Err(st.fail(
                    line,
                    format!("range() is limited to {} items", super::MAX_RANGE),
                ));
            }
            Value::Array((lo..hi.max(lo)).map(Value::from).collect())
        }
        "str" => Value::String(show(&args[0])),
        "int" => match &args[0] {
            Value::Number(_) => args[0].clone(),
            Value::Bool(b) => json!(i64::from(*b)),
            Value::String(s) => match s.trim().parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => {
                    return Err(
                        st.fail(line, format!("int() cannot read \"{s}\" as a whole number"))
                    );
                }
            },
            other => {
                return Err(st.fail(line, format!("int() cannot convert {}", type_name(other))));
            }
        },
        "sum" => {
            let mut total: i64 = 0;
            for v in items(st, args)? {
                let Some(n) = int(&v) else {
                    return Err(st.fail(
                        line,
                        format!("sum() needs numbers, found {}", type_name(&v)),
                    ));
                };
                total = total
                    .checked_add(n)
                    .ok_or_else(|| st.fail(line, "number is too large"))?;
            }
            json!(total)
        }
        "abs" => match int(&args[0]) {
            Some(n) => json!(n.abs()),
            None => return Err(st.fail(line, "abs() needs a number")),
        },
        "min" | "max" | "sorted" => {
            let mut v = items(st, args)?;
            if v.is_empty() && name != "sorted" {
                return Err(st.fail(line, format!("{name}() of an empty list")));
            }
            let key = |x: &Value| -> Option<(i64, String)> {
                match x {
                    Value::Number(n) => n.as_i64().map(|n| (n, String::new())),
                    Value::String(s) => Some((0, s.clone())),
                    o if is_part(o) || is_place(o) => {
                        Some((0, o["id"].as_str().unwrap_or_default().to_string()))
                    }
                    _ => None,
                }
            };
            if v.iter().any(|x| key(x).is_none()) {
                return Err(st.fail(
                    line,
                    format!("{name}() needs numbers, text, parts or places"),
                ));
            }
            v.sort_by_key(|a| key(a));
            match name {
                "min" => v.first().cloned().unwrap_or(Value::Null),
                "max" => v.last().cloned().unwrap_or(Value::Null),
                _ => Value::Array(v),
            }
        }
        _ => return Err(st.fail(line, format!("unknown builtin {id}"))),
    })
}

/// Loads, links and runs script programs for one executor.
pub struct ScriptHost {
    engine: Engine,
    linker: Linker<State>,
    allowed: Vec<(String, String)>,
    modules: HashMap<String, Module>,
}

impl ScriptHost {
    /// `grants`: capability ids the policy grants (observe/pick/place map onto them).
    pub fn new(grants: &BTreeMap<String, String>) -> Result<ScriptHost, String> {
        let engine = crate::program::engine()?;
        let mut l: Linker<State> = Linker::new(&engine);
        let e = |e: wasmtime::Error| format!("linking the script runtime: {e}");
        l.func_wrap("rt", "int", |mut c: Caller<'_, State>, n: i32| {
            c.data_mut().push(json!(n))
        })
        .map_err(e)?;
        l.func_wrap("rt", "str", |mut c: Caller<'_, State>, p: i32, n: i32| {
            let s = read(&mut c, p, n)?;
            c.data_mut().push(Value::String(s))
        })
        .map_err(e)?;
        l.func_wrap("rt", "list_new", |mut c: Caller<'_, State>| {
            c.data_mut().push(json!([]))
        })
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "list_push",
            |mut c: Caller<'_, State>, list: i32, v: i32| {
                let st = c.data_mut();
                let v = st.get(v)?.clone();
                let Some(Value::Array(a)) = usize::try_from(list)
                    .ok()
                    .and_then(|h| st.values.get_mut(h))
                else {
                    return Err(wasmtime::Error::msg("list_push on a non-list"));
                };
                a.push(v);
                Ok(())
            },
        )
        .map_err(e)?;
        l.func_wrap("rt", "kw_new", |mut c: Caller<'_, State>| {
            c.data_mut().push(json!({}))
        })
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "kw_set",
            |mut c: Caller<'_, State>, o: i32, p: i32, n: i32, v: i32| {
                let k = read(&mut c, p, n)?;
                let st = c.data_mut();
                let v = st.get(v)?.clone();
                let Some(Value::Object(m)) =
                    usize::try_from(o).ok().and_then(|h| st.values.get_mut(h))
                else {
                    return Err(wasmtime::Error::msg("kw_set on a non-object"));
                };
                m.insert(k, v);
                Ok(())
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "binop",
            |mut c: Caller<'_, State>, op: i32, a: i32, b: i32, line: i32| {
                let st = c.data_mut();
                let (a, b) = (st.get(a)?.clone(), st.get(b)?.clone());
                let v = binop(st, op, &a, &b, line)?;
                st.push(v)
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "unop",
            |mut c: Caller<'_, State>, op: i32, a: i32, line: i32| {
                let st = c.data_mut();
                let a = st.get(a)?.clone();
                let v = if op == 1 {
                    Value::Bool(!truthy(&a))
                } else {
                    match int(&a) {
                        Some(n) => json!(-n),
                        None => {
                            return Err(st.fail(line, format!("cannot negate {}", type_name(&a))));
                        }
                    }
                };
                st.push(v)
            },
        )
        .map_err(e)?;
        l.func_wrap("rt", "truthy", |c: Caller<'_, State>, a: i32| {
            Ok(i32::from(truthy(c.data().get(a)?)))
        })
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "attr",
            |mut c: Caller<'_, State>, a: i32, p: i32, n: i32, line: i32| {
                let name = read(&mut c, p, n)?;
                let st = c.data_mut();
                let v = st.get(a)?.clone();
                let key = if name == "location" {
                    "at"
                } else {
                    name.as_str()
                };
                let found = if is_part(&v) || is_place(&v) {
                    v.get(key).filter(|_| key != "type").cloned()
                } else {
                    None
                };
                match found {
                    Some(x) => st.push(x),
                    None => Err(st.fail(line, format!("{} has no `{name}`", type_name(&v)))),
                }
            },
        )
        .map_err(e)?;
        l.func_wrap("rt", "index", |mut c: Caller<'_, State>, a: i32, i: i32, line: i32| {
            let st = c.data_mut();
            let (v, i) = (st.get(a)?.clone(), st.get(i)?.clone());
            let Some(i) = int(&i) else {
                return Err(st.fail(line, "list positions must be whole numbers"));
            };
            let item = match &v {
                Value::Array(items) => {
                    let n = items.len() as i64;
                    let j = if i < 0 { n + i } else { i };
                    if j < 0 || j >= n {
                        return Err(st.fail(line, format!("position {i} is outside the list (it has {n} item{})", if n == 1 { "" } else { "s" })));
                    }
                    items[j as usize].clone()
                }
                Value::String(s) => {
                    let chars: Vec<char> = s.chars().collect();
                    let n = chars.len() as i64;
                    let j = if i < 0 { n + i } else { i };
                    if j < 0 || j >= n {
                        return Err(st.fail(line, format!("position {i} is outside the text")));
                    }
                    Value::String(chars[j as usize].to_string())
                }
                other => return Err(st.fail(line, format!("{} cannot be indexed", type_name(other)))),
            };
            st.push(item)
        })
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "snapshot",
            |mut c: Caller<'_, State>, a: i32, line: i32| {
                let st = c.data_mut();
                let v = match st.get(a)?.clone() {
                    Value::Array(x) => Value::Array(x),
                    Value::String(s) => {
                        Value::Array(s.chars().map(|ch| Value::String(ch.to_string())).collect())
                    }
                    other => {
                        return Err(st.fail(
                            line,
                            format!("cannot loop over {}; loop over a list", type_name(&other)),
                        ));
                    }
                };
                // Always a fresh value, so appending to the list inside the loop cannot extend it.
                if st.values.len() >= MAX_VALUES {
                    return Err(wasmtime::Error::msg("the program created too many values"));
                }
                st.values.push(v);
                Ok(st.values.len() as i32 - 1)
            },
        )
        .map_err(e)?;
        l.func_wrap("rt", "len", |c: Caller<'_, State>, a: i32| {
            Ok(c.data()
                .get(a)?
                .as_array()
                .map(|x| x.len() as i32)
                .unwrap_or(0))
        })
        .map_err(e)?;
        l.func_wrap("rt", "item", |mut c: Caller<'_, State>, a: i32, i: i32| {
            let st = c.data_mut();
            let v = st
                .get(a)?
                .as_array()
                .and_then(|x| x.get(i as usize))
                .cloned()
                .unwrap_or(Value::Null);
            st.push(v)
        })
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "builtin",
            |mut c: Caller<'_, State>, id: i32, args: i32, _kw: i32, line: i32| {
                let st = c.data_mut();
                let args = st.get(args)?.as_array().cloned().unwrap_or_default();
                let v = builtin(st, id, &args, line)?;
                st.push(v)
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "method",
            |mut c: Caller<'_, State>, recv: i32, p: i32, n: i32, args: i32, line: i32| {
                let name = read(&mut c, p, n)?;
                let st = c.data_mut();
                let args = st.get(args)?.as_array().cloned().unwrap_or_default();
                let r = st.get(recv)?.clone();
                let out = match (&r, name.as_str(), args.as_slice()) {
                    (Value::Array(_), "append", [v]) => {
                        if let Some(Value::Array(a)) = usize::try_from(recv)
                            .ok()
                            .and_then(|h| st.values.get_mut(h))
                        {
                            a.push(v.clone());
                        }
                        Value::Null
                    }
                    (Value::String(s), "startswith", [Value::String(x)]) => {
                        Value::Bool(s.starts_with(x.as_str()))
                    }
                    (Value::String(s), "endswith", [Value::String(x)]) => {
                        Value::Bool(s.ends_with(x.as_str()))
                    }
                    (Value::String(s), "lower", []) => Value::String(s.to_lowercase()),
                    (Value::String(s), "upper", []) => Value::String(s.to_uppercase()),
                    (Value::String(s), "strip", []) => Value::String(s.trim().to_string()),
                    _ => {
                        return Err(st.fail(
                            line,
                            format!(
                                "{} has no method .{name}() for these arguments",
                                type_name(&r)
                            ),
                        ));
                    }
                };
                st.push(out)
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "rt",
            "expect",
            |mut c: Caller<'_, State>, cond: i32, p: i32, n: i32, line: i32| {
                let desc = read(&mut c, p, n)?;
                let st = c.data_mut();
                let held = truthy(st.get(cond)?);
                match st.ext("expect", json!({"description": desc, "held": held}), line)? {
                    Err(stop) => Ok(stop),
                    Ok(_) => Ok(0),
                }
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "cap",
            "observe",
            |mut c: Caller<'_, State>, q: i32, args: i32, kw: i32, line: i32| {
                let st = c.data_mut();
                let qname = QUERIES.get(q as usize).copied().unwrap_or("parts");
                let args_v = st.get(args)?.as_array().cloned().unwrap_or_default();
                let kw_v = st.get(kw)?.as_object().cloned().unwrap_or_default();
                let obs = match st.observation(qname, line)? {
                    Ok(o) => o,
                    Err(stop) => return Ok(stop),
                };
                let v = query(st, qname, &obs, &args_v, &kw_v, line)?;
                st.push(v)
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "cap",
            "pick",
            |mut c: Caller<'_, State>, args: i32, _kw: i32, line: i32| {
                let st = c.data_mut();
                let a = st.get(args)?.as_array().cloned().unwrap_or_default();
                let part = a.first().cloned().unwrap_or(Value::Null);
                if !is_part(&part) {
                    return Err(st.fail(
                        line,
                        format!(
                            "pick() needs a part (from parts() or holding()), not {}",
                            type_name(&part)
                        ),
                    ));
                }
                let mut args = json!({"object_id": part["id"]});
                // From move(): where the part is going (the executor refuses the pick
                // if that place cannot take it). pick() itself takes one argument.
                if let Some(to) = a.get(1).filter(|v| !v.is_null()) {
                    args["to"] = json!(place_id(st, to, "the destination", line)?);
                }
                match st.ext("workspace.pick", args, line)? {
                    Err(stop) => Ok(stop),
                    Ok(row) => {
                        let ok = row.status == A_SUCCEEDED;
                        Ok(if ok { TRUE } else { FALSE })
                    }
                }
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "cap",
            "place",
            |mut c: Caller<'_, State>, args: i32, kw: i32, line: i32| {
                let st = c.data_mut();
                let a = st.get(args)?.as_array().cloned().unwrap_or_default();
                let kw = st.get(kw)?.as_object().cloned().unwrap_or_default();
                let part = a.first().cloned().unwrap_or(Value::Null);
                if !is_part(&part) {
                    return Err(st.fail(
                        line,
                        format!("place() needs a part, not {}", type_name(&part)),
                    ));
                }
                let Some(to) = a.get(1).or_else(|| kw.get("to")).cloned() else {
                    return Err(st.fail(line, "place() needs a destination"));
                };
                let to = place_id(st, &to, "the destination", line)?;
                let args = json!({"object_id": part["id"], "destination_id": to});
                match st.ext("workspace.place", args, line)? {
                    Err(stop) => Ok(stop),
                    Ok(row) => Ok(if row.status == A_SUCCEEDED {
                        TRUE
                    } else {
                        FALSE
                    }),
                }
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "human",
            "ask",
            |mut c: Caller<'_, State>, args: i32, kw: i32, line: i32| {
                let st = c.data_mut();
                let a = st.get(args)?.as_array().cloned().unwrap_or_default();
                let kw = st.get(kw)?.as_object().cloned().unwrap_or_default();
                let Some(Value::String(q)) = a.first().cloned() else {
                    return Err(st.fail(line, "ask() needs a question as text"));
                };
                let opts: Vec<String> = match a.get(1).or_else(|| kw.get("options")) {
                    Some(Value::Array(o)) => o.iter().map(show).collect(),
                    _ => vec![],
                };
                if opts.len() < 2 || opts.len() > 6 {
                    return Err(st.fail(line, "ask() needs between 2 and 6 options"));
                }
                match st.ext("human.ask", json!({"question": q, "options": opts}), line)? {
                    Err(stop) => Ok(stop),
                    Ok(row) => {
                        let ans = row
                            .result
                            .as_ref()
                            .and_then(|r| r.get("answer"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        st.push(ans)
                    }
                }
            },
        )
        .map_err(e)?;
        l.func_wrap(
            "human",
            "say",
            |mut c: Caller<'_, State>, args: i32, _kw: i32, line: i32| {
                let st = c.data_mut();
                let a = st.get(args)?.as_array().cloned().unwrap_or_default();
                let msg: String = a
                    .iter()
                    .map(show)
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(500)
                    .collect();
                match st.ext("human.say", json!({"message": msg}), line)? {
                    Err(stop) => Ok(stop),
                    Ok(_) => Ok(NONE),
                }
            },
        )
        .map_err(e)?;
        let mut allowed: Vec<(String, String)> = super::codegen::RT_NAMES
            .iter()
            .map(|n| ("rt".to_string(), n.to_string()))
            .collect();
        for (name, cap) in [
            ("observe", "workspace.observe"),
            ("pick", "workspace.pick"),
            ("place", "workspace.place"),
        ] {
            if grants.contains_key(cap) {
                allowed.push(("cap".into(), name.into()));
            }
        }
        allowed.push(("human".into(), "ask".into()));
        allowed.push(("human".into(), "say".into()));
        Ok(ScriptHost {
            engine,
            linker: l,
            allowed,
            modules: HashMap::new(),
        })
    }

    fn module(&mut self, wasm: &[u8]) -> Result<Module, String> {
        let key = crate::util::sha256_hex(wasm);
        if let Some(m) = self.modules.get(&key) {
            return Ok(m.clone());
        }
        let m = Module::new(&self.engine, wasm).map_err(|e| format!("invalid module: {e}"))?;
        for imp in m.imports() {
            if !self
                .allowed
                .iter()
                .any(|(md, n)| md == imp.module() && n == imp.name())
            {
                return Err(format!(
                    "the program imports {}.{}, which is not granted",
                    imp.module(),
                    imp.name()
                ));
            }
        }
        self.modules.insert(key, m.clone());
        Ok(m)
    }

    /// Run the program from the top until it finishes or needs something new.
    pub fn next(&mut self, wasm: &[u8], log: &[ActionRow]) -> ScriptStep {
        let module = match self.module(wasm) {
            Ok(m) => m,
            Err(e) => return ScriptStep::Refused(e),
        };
        let mut store = Store::new(&self.engine, State::new(log.to_vec()));
        if let Err(e) = store.set_fuel(FUEL) {
            return ScriptStep::Trapped(format!("fuel: {e}"));
        }
        let ran = self
            .linker
            .instantiate(&mut store, &module)
            .and_then(|i| i.get_typed_func::<(), i32>(&mut store, "run"))
            .and_then(|run| run.call(&mut store, ()));
        let mut st = store.into_data();
        match (ran, st.outcome.take()) {
            (_, Some(ScriptStep::Error { line, message })) => ScriptStep::Error { line, message },
            (Err(e), _) => ScriptStep::Trapped(format!("{e:#}")),
            (Ok(1), Some(o)) => o,
            (Ok(_), Some(ScriptStep::Diverged(r))) => ScriptStep::Diverged(r),
            (Ok(_), Some(_)) => {
                ScriptStep::Diverged("the program finished after it was told to stop".into())
            }
            (Ok(0), None) if st.next_call == st.log.len() => ScriptStep::Done,
            (Ok(0), None) => ScriptStep::Diverged(format!(
                "the program finished after {} calls, but the log has {}",
                st.next_call,
                st.log.len()
            )),
            (Ok(c), None) => {
                ScriptStep::Diverged(format!("the program stopped with code {c} but no request"))
            }
        }
    }
}
