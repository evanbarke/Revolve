//! Loopback-only browser UI server.
//!
//! The server never schedules actions itself. "Start"/"Resume" launch the same
//! `revolve resume` executor binary as a child process (so an injected crash
//! kills only that child, exactly as on the CLI); pause/cancel/correct/answer and
//! simulation controls write to the same stores through the same functions the
//! CLI uses. Status is read from persisted state on every poll.
//!
//! Request boundary: non-loopback binds are refused. Mutating requests must
//! carry `X-Revolve-Client: ui` (a custom header, so cross-site pages cannot send
//! it without a CORS preflight the server never approves), a matching Host header
//! (DNS-rebinding guard) and, if present, a matching Origin.

use crate::harness::{self, Arm, PlannerChoice};
use crate::inventory;
use crate::model::{GoalSpec, parse_restricted_request};
use crate::scenario::{self, FaultSpec, Scenario};
use crate::sim::SimWorld;
use crate::store::*;
use crate::util::{Error, Result, now_ms};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");
const MAX_BODY: u64 = 64 * 1024;

struct Ctx {
    state_dir: PathBuf,
    scenarios_dir: PathBuf,
    step_delay_ms: u64,
    children: Mutex<HashMap<String, Child>>,
    exe: PathBuf,
}

pub fn check_bind(bind: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| Error::new(format!("invalid --bind `{bind}`: {e}")))?;
    if !addr.ip().is_loopback() {
        crate::bail!(
            "refusing to bind {addr}: the v0 UI has no authentication, so it only listens on loopback. \
             For remote access (e.g. to a Jetson), use an SSH tunnel: ssh -L 8080:127.0.0.1:8080 <host>"
        );
    }
    Ok(addr)
}

pub fn serve(bind: &str, state_dir: &Path, scenarios_dir: &Path, step_delay_ms: u64) -> Result<()> {
    let addr = check_bind(bind)?;
    std::fs::create_dir_all(state_dir)?;
    let server =
        Server::http(addr).map_err(|e| Error::new(format!("cannot listen on {addr}: {e}")))?;
    let ctx = Arc::new(Ctx {
        state_dir: state_dir.to_path_buf(),
        scenarios_dir: scenarios_dir.to_path_buf(),
        step_delay_ms,
        children: Mutex::new(HashMap::new()),
        exe: std::env::current_exe()?,
    });
    eprintln!("[revolve] UI on http://{addr}/ (loopback only; Ctrl+C to stop)");
    for req in server.incoming_requests() {
        let ctx = ctx.clone();
        std::thread::spawn(move || handle(&ctx, req));
    }
    Ok(())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

fn respond(req: Request, status: u16, content_type: &str, body: String) {
    let resp = Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", content_type))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
        ));
    let _ = req.respond(resp);
}

fn json_resp(req: Request, status: u16, v: &Value) {
    respond(req, status, "application/json", v.to_string());
}

fn get_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// True for a Host/Origin authority that names the loopback interface (any port).
/// Any port is accepted so that SSH tunnels with a different local port work;
/// a DNS-rebinding attacker's hostname never matches.
fn is_loopback_authority(a: &str) -> bool {
    let host = if let Some(rest) = a.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        a.rsplit_once(':').map(|(h, _)| h).unwrap_or(a)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Boundary check for mutating requests. Exposed for tests.
pub fn mutation_allowed(
    host: Option<&str>,
    origin: Option<&str>,
    client: Option<&str>,
) -> std::result::Result<(), &'static str> {
    if client != Some("ui") {
        return Err("missing X-Revolve-Client header");
    }
    match host {
        Some(h) if is_loopback_authority(h) => {}
        _ => return Err("unexpected Host header"),
    }
    if let Some(o) = origin {
        let o = o.trim_start_matches("http://");
        if !is_loopback_authority(o) {
            return Err("cross-origin request refused");
        }
    }
    Ok(())
}

fn handle(ctx: &Ctx, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    // DNS-rebinding guard for every request, reads included: only loopback Host names.
    if !get_header(&req, "Host").is_some_and(is_loopback_authority) {
        return json_resp(req, 403, &json!({"error": "unexpected Host header"}));
    }
    if method == Method::Get {
        match path.as_str() {
            "/" | "/index.html" => {
                return respond(req, 200, "text/html; charset=utf-8", INDEX_HTML.into());
            }
            "/app.js" => return respond(req, 200, "text/javascript; charset=utf-8", APP_JS.into()),
            "/style.css" => return respond(req, 200, "text/css; charset=utf-8", STYLE_CSS.into()),
            _ => {}
        }
    }
    if method == Method::Post
        && let Err(why) = mutation_allowed(
            get_header(&req, "Host"),
            get_header(&req, "Origin"),
            get_header(&req, "X-Revolve-Client"),
        )
    {
        return json_resp(req, 403, &json!({"error": why}));
    }
    let mut body = String::new();
    if method == Method::Post
        && (req
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_string(&mut body)
            .is_err()
            || body.len() as u64 > MAX_BODY)
    {
        return json_resp(
            req,
            413,
            &json!({"error": "request body too large or not UTF-8"}),
        );
    }
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let result = match (method.clone(), parts.as_slice()) {
        (Method::Get, ["api", "scenarios"]) => list_scenarios(ctx),
        (Method::Get, ["api", "runs"]) => list_runs(ctx),
        (Method::Post, ["api", "runs"]) => create(ctx, &body),
        (Method::Get, ["api", "runs", id]) => run_view(ctx, id),
        (Method::Post, ["api", "runs", id, action]) => act(ctx, id, action, &body),
        _ => Err(Error::new("not found")),
    };
    match result {
        Ok(v) => json_resp(req, 200, &v),
        Err(e) if e.msg == "not found" => json_resp(req, 404, &json!({"error": "not found"})),
        Err(e) => json_resp(req, 400, &json!({"error": e.msg})),
    }
}

fn list_scenarios(ctx: &Ctx) -> Result<Value> {
    let mut out = vec![];
    for sub in ["demo", "development", "held-out"] {
        for f in scenario::list_dir(&ctx.scenarios_dir.join(sub)) {
            if let Ok(s) = Scenario::load(&f) {
                out.push(json!({"id": s.id, "set": sub, "family": s.family, "description": s.description, "goal": s.goal}));
            }
        }
    }
    Ok(json!(out))
}

fn list_runs(ctx: &Ctx) -> Result<Value> {
    let mut out = vec![];
    for id in harness::list_runs(&ctx.state_dir).into_iter().take(40) {
        let Ok(dir) = harness::run_dir(&ctx.state_dir, &id) else {
            continue;
        };
        let Ok(s) = RunStore::open(&dir) else {
            continue;
        };
        let (Ok(st), Ok(meta)) = (s.state(), s.meta()) else {
            continue;
        };
        out.push(json!({"run_id": id, "status": st.status, "arm": meta.arm, "planner": meta.planner, "scenario": meta.scenario_id, "created_at_ms": meta.created_at_ms}));
    }
    Ok(json!(out))
}

fn create(ctx: &Ctx, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)?;
    let name = v["scenario"].as_str().unwrap_or("nominal");
    // A scenario is chosen by name only; never by path.
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        crate::bail!("invalid scenario name");
    }
    let arm = Arm::parse(v["arm"].as_str().unwrap_or("revolve"))?;
    let mut sc = Scenario::load(&scenario::resolve(name, &ctx.scenarios_dir)?)?;
    // Optional goal override from the UI (structured form or restricted-grammar text).
    if let Some(text) = v["request"].as_str().filter(|t| !t.trim().is_empty()) {
        sc.goal = parse_restricted_request(text)?;
    } else if let Some(g) = v.get("goal").filter(|g| !g.is_null()) {
        let g: GoalSpec =
            serde_json::from_value(g.clone()).map_err(|e| Error::new(format!("goal: {e}")))?;
        g.validate()?;
        sc.goal = g;
    }
    let human = if v["scripted_human"].as_bool().unwrap_or(false) {
        "scripted"
    } else {
        "interactive"
    };
    if v["planner"].as_str().unwrap_or("scripted") != "scripted" {
        crate::bail!(
            "the UI starts scripted-planner runs; live runs require an explicit --budget on the CLI"
        );
    }
    let (run_id, _) =
        harness::create_run(&ctx.state_dir, &sc, arm, &PlannerChoice::Scripted, human)?;
    Ok(json!({"run_id": run_id}))
}

fn spawn_executor(ctx: &Ctx, run_id: &str) -> Result<()> {
    let dir = harness::run_dir(&ctx.state_dir, run_id)?;
    let mut children = ctx
        .children
        .lock()
        .map_err(|_| Error::new("lock poisoned"))?;
    if let Some(c) = children.get_mut(run_id)
        && c.try_wait().ok().flatten().is_none()
    {
        crate::bail!("an executor is already running for this run");
    }
    if OwnerLock::is_held(&dir) {
        crate::bail!("another executor owns this run");
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("executor.log"))?;
    let child = Command::new(&ctx.exe)
        .arg("resume")
        .arg("--run")
        .arg(run_id)
        .arg("--state-dir")
        .arg(&ctx.state_dir)
        .arg("--step-delay-ms")
        .arg(ctx.step_delay_ms.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()?;
    children.insert(run_id.to_string(), child);
    Ok(())
}

fn executor_state(ctx: &Ctx, run_id: &str, dir: &Path) -> Value {
    let mut exit = Value::Null;
    if let Ok(mut children) = ctx.children.lock()
        && let Some(c) = children.get_mut(run_id)
        && let Ok(Some(st)) = c.try_wait()
    {
        exit = json!(st.code());
    }
    json!({"active": OwnerLock::is_held(dir), "last_exit_code": exit, "simulated_crash_code": crate::util::SIMULATED_CRASH_EXIT_CODE})
}

fn act(ctx: &Ctx, run_id: &str, action: &str, body: &str) -> Result<Value> {
    let dir = harness::run_dir(&ctx.state_dir, run_id)?;
    let v: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body)?
    };
    match action {
        "start" | "resume" => {
            spawn_executor(ctx, run_id)?;
            Ok(json!({"ok": true, "message": "executor started"}))
        }
        "pause" => {
            RunStore::open(&dir)?.request_control("pause")?;
            Ok(
                json!({"ok": true, "message": "pause requested; in-flight outcomes will be established first"}),
            )
        }
        "cancel" => {
            RunStore::open(&dir)?.request_control("cancel")?;
            if !OwnerLock::is_held(&dir) {
                spawn_executor(ctx, run_id)?;
            }
            Ok(json!({"ok": true, "message": "cancel requested; the executor will acknowledge it"}))
        }
        "correct" => {
            let mut store = RunStore::open(&dir)?;
            let goal: GoalSpec =
                if let Some(t) = v["request"].as_str().filter(|t| !t.trim().is_empty()) {
                    parse_restricted_request(t)?
                } else {
                    let mut g: GoalSpec = serde_json::from_value(v["goal"].clone())
                        .map_err(|e| Error::new(format!("goal: {e}")))?;
                    // Decisions already made (substitutions, rework) carry over unless the form changes them.
                    let (_, cur) = store.current_goal()?;
                    if g.approved_substitutions.is_empty() {
                        g.approved_substitutions = cur.approved_substitutions;
                    }
                    if g.declined_substitutions.is_empty() {
                        g.declined_substitutions = cur.declined_substitutions;
                    }
                    if g.rework_permitted.is_none() {
                        g.rework_permitted = cur.rework_permitted;
                    }
                    g
                };
            let rev = store.submit_goal_revision(&goal, "human (ui)", "correction from the UI")?;
            Ok(json!({"ok": true, "message": format!("goal revision {rev} recorded")}))
        }
        "answer" => {
            let q = v["question_id"]
                .as_i64()
                .ok_or_else(|| Error::new("question_id required"))?;
            let choice = v["choice"]
                .as_str()
                .ok_or_else(|| Error::new("choice required"))?;
            let out = RunStore::open(&dir)?.answer(q, choice, "human (ui)")?;
            // The run was waiting only for this answer: continue it.
            if !OwnerLock::is_held(&dir) {
                spawn_executor(ctx, run_id)?;
            }
            Ok(json!({"ok": true, "message": format!("{out}; continuing")}))
        }
        "sim" => {
            let mut w = SimWorld::open(&dir.join("world.db"))?;
            let what = v["control"].as_str().unwrap_or("");
            match what {
                "restore_evidence" => w.set_blackout(false)?,
                "planner_offline" => w.set_planner_offline(true)?,
                "planner_online" => w.set_planner_offline(false)?,
                "tray_unavailable" | "tray_available" => {
                    let loc = v["location_id"]
                        .as_str()
                        .ok_or_else(|| Error::new("location_id required"))?;
                    if !loc.starts_with("tray-") {
                        crate::bail!("only trays can be toggled from the UI");
                    }
                    w.set_location_available(loc, what == "tray_available")?;
                }
                "arm_fault" => {
                    let f: FaultSpec = serde_json::from_value(v["fault"].clone())
                        .map_err(|e| Error::new(format!("fault: {e}")))?;
                    w.arm_fault(&f)?;
                }
                other => crate::bail!("unknown simulation control `{other}`"),
            }
            w.log_harness(
                "human_intervention",
                &json!({"type": "sim_control", "control": what, "via": "ui"}),
            )?;
            Ok(json!({"ok": true, "message": format!("simulation control applied: {what}")}))
        }
        _ => Err(Error::new("not found")),
    }
}

fn run_view(ctx: &Ctx, run_id: &str) -> Result<Value> {
    let dir = harness::run_dir(&ctx.state_dir, run_id)?;
    let store = RunStore::open(&dir)?;
    let meta = store.meta()?;
    let st = store.state()?;
    let world = SimWorld::open(&dir.join("world.db"))?;
    let truth = world.truth()?;
    let actions = store.actions()?;
    let plans = store.plans()?;
    let active = st.active_plan_seq.or_else(|| {
        plans
            .iter()
            .rev()
            .find(|p| p.status != "rejected" && p.status != "stale")
            .map(|p| p.plan_seq)
    });
    let steps: Vec<Value> = actions
        .iter()
        .filter(|a| Some(a.plan_seq) == Some(active) || a.origin != "plan")
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|a| {
            json!({
                "action_id": a.action_id, "plan_seq": a.plan_seq, "origin": a.origin, "step_id": a.step_id,
                "capability": a.capability, "arguments": a.resolved_arguments.clone().unwrap_or_else(|| a.arguments.clone()),
                "status": a.status, "op_key": a.op_key, "attempts": a.attempts, "error": a.error, "goal_revision": a.goal_revision
            })
        })
        .collect();
    let plan_summ: Vec<Value> = plans
        .iter()
        .map(|p| {
            json!({
                "plan_seq": p.plan_seq, "status": p.status, "goal_revision": p.goal_revision, "planner": p.planner,
                "world_revision": p.world_revision, "explanation": p.proposal.get("explanation"), "kit_assignments": p.proposal.get("kit_assignments"),
                "steps": p.proposal.get("steps").and_then(Value::as_array).map(|s| s.len()),
                "diagnostics": p.diagnostics
            })
        })
        .collect();
    let (rev, goal) = store.current_goal()?;
    let truth_locs: Vec<Value> = truth
        .locations
        .iter()
        .filter(|l| l.location_id != crate::sim::OUTSIDE)
        .map(|l| {
            let objs: Vec<Value> = truth
                .objects
                .iter()
                .filter(|o| o.location_id == l.location_id)
                .map(|o| json!({"object_id": o.object_id, "kind": o.true_kind, "observed_kind": o.observed_kind}))
                .collect();
            json!({"location_id": l.location_id, "loc_type": l.loc_type, "available": l.available, "objects": objs})
        })
        .collect();
    let removed: Vec<String> = truth
        .objects
        .iter()
        .filter(|o| o.location_id == crate::sim::OUTSIDE)
        .map(|o| o.object_id.clone())
        .collect();
    let withheld: Vec<String> = if truth.blackout {
        world.observe_withheld()
    } else {
        vec![]
    };
    Ok(json!({
        "meta": meta,
        "state": st,
        "executor": executor_state(ctx, run_id, &dir),
        "goal_revision": rev,
        "goal": goal,
        "goal_history": store.goal_history()?,
        "plans": plan_summ,
        "active_plan_seq": active,
        "steps": steps,
        "inventory": inventory::view(&store)?,
        "ledger": inventory::ledger(&store)?.into_iter().rev().take(40).collect::<Vec<_>>(),
        "questions": store.questions()?,
        "events": store.events(0, 100_000)?.into_iter().rev().take(400).collect::<Vec<_>>(),
        "planner_calls": store.planner_call_stats()?,
        "workspace_truth": {"label": "simulator ground truth (the planner never sees this view)", "locations": truth_locs, "removed": removed,
                            "evidence_withheld_from_system": withheld, "planner_offline": truth.planner_offline, "world_revision": truth.world_revision,
                            "effect_counts": world.effect_counts()?, "faults": truth.faults},
        "now_ms": now_ms(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_authority_parsing() {
        for ok in ["127.0.0.1:8080", "localhost:1", "[::1]:8080", "localhost"] {
            assert!(is_loopback_authority(ok), "{ok}");
        }
        for bad in [
            "evil.example:8080",
            "127.0.0.1.evil.example:80",
            "10.0.0.1:8080",
            "[::2]:80",
            "",
        ] {
            assert!(!is_loopback_authority(bad), "{bad}");
        }
    }

    #[test]
    fn only_loopback_binds_are_accepted() {
        assert!(check_bind("127.0.0.1:8080").is_ok());
        assert!(check_bind("[::1]:8080").is_ok());
        assert!(check_bind("0.0.0.0:8080").is_err());
        assert!(check_bind("192.168.1.20:8080").is_err());
    }

    #[test]
    fn mutations_need_the_client_header_and_loopback_host_and_origin() {
        let ui = Some("ui");
        assert!(mutation_allowed(Some("127.0.0.1:8080"), None, ui).is_ok());
        // An SSH tunnel on another local port, and the localhost / IPv6 spellings.
        assert!(
            mutation_allowed(Some("127.0.0.1:18080"), Some("http://127.0.0.1:18080"), ui).is_ok()
        );
        assert!(
            mutation_allowed(Some("localhost:8080"), Some("http://localhost:8080"), ui).is_ok()
        );
        assert!(mutation_allowed(Some("[::1]:8080"), None, ui).is_ok());
        assert!(
            mutation_allowed(Some("127.0.0.1:8080"), None, None).is_err(),
            "missing custom header"
        );
        assert!(
            mutation_allowed(Some("evil.example:8080"), None, ui).is_err(),
            "DNS rebinding"
        );
        assert!(
            mutation_allowed(Some("127.0.0.1.evil.example"), None, ui).is_err(),
            "look-alike host"
        );
        assert!(mutation_allowed(None, None, ui).is_err(), "no Host header");
        assert!(
            mutation_allowed(Some("127.0.0.1:8080"), Some("http://evil.example"), ui).is_err(),
            "cross-origin"
        );
    }
}
