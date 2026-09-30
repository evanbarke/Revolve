//! Loopback-only browser UI server.
//!
//! The server never schedules actions itself. "Start"/"Resume" launch the same
//! `revolve resume` executor binary as a child process (so an injected crash
//! kills only that child, exactly as on the CLI); pause/cancel/correct/answer and
//! simulation controls write to the same stores through the same functions the
//! CLI uses. Status is read from persisted state on every poll.
//!
//! Request boundary: by default only loopback binds are allowed. Mutating
//! requests must carry `X-Revolve-Client: ui` (a custom header, so cross-site
//! pages cannot send it without a CORS preflight the server never approves), a
//! matching Host header (DNS-rebinding guard) and, if present, a matching Origin.
//!
//! LAN mode (`serve --lan`, opt-in) adds, for every request from another device:
//! the peer must be on a private network (RFC 1918, link-local, 100.64/10 as used
//! by Tailscale, IPv6 ULA/link-local); the Host must be an IP address (names are
//! refused, so DNS rebinding cannot reach it); and the device must present the
//! access key, once in the link (`/?key=...`) and then as a SameSite=Strict,
//! HttpOnly cookie. The key is 128 random bits kept in the state directory.
//! Requests from this computer (loopback) need no key.

use crate::harness::{self, Arm, PlannerChoice};
use crate::inventory;
use crate::model::{GoalSpec, parse_restricted_request};
use crate::planner::live::{BudgetLedger, LiveConfig, LivePlanner};
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
const SCENE_JS: &str = include_str!("../web/scene.js");
// three.js r186 (MIT), vendored; see web/vendor/three/SOURCE.txt.
const THREE_MODULE: &str = include_str!("../web/vendor/three/three.module.js");
const THREE_CORE: &str = include_str!("../web/vendor/three/three.core.js");
const THREE_ORBIT: &str = include_str!("../web/vendor/three/OrbitControls.js");
const MAX_BODY: u64 = 64 * 1024;

/// Identifies the page this server serves. Every response carries it, so a page
/// left open across a rebuild can tell it is out of date instead of misreading
/// newer data.
fn build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        crate::util::sha256_hex(
            [INDEX_HTML, APP_JS, STYLE_CSS, SCENE_JS]
                .concat()
                .as_bytes(),
        )[..12]
            .to_string()
    })
}

struct Ctx {
    /// LAN mode: the access key other devices must present.
    lan_key: Option<String>,
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

/// LAN mode binds: the unspecified address, loopback, or a private address. Never public.
pub fn check_bind_lan(bind: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| Error::new(format!("invalid --bind `{bind}`: {e}")))?;
    if !(addr.ip().is_unspecified() || is_private_ip(addr.ip())) {
        crate::bail!(
            "refusing to bind {addr} in LAN mode: only private network addresses (or 0.0.0.0) are allowed"
        );
    }
    Ok(addr)
}

/// Home/office networks, link-local, Tailscale (100.64/10) and loopback.
pub fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => {
            let o = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        std::net::IpAddr::V6(a) => {
            let s0 = a.segments()[0];
            a.is_loopback()
                || (s0 & 0xfe00) == 0xfc00
                || (s0 & 0xffc0) == 0xfe80
                || a.to_ipv4_mapped()
                    .is_some_and(|v4| is_private_ip(std::net::IpAddr::V4(v4)))
        }
    }
}

/// A Host header naming an IP address (plus port), never a DNS name.
fn is_ip_authority(a: &str) -> bool {
    if let Some(rest) = a.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok());
    }
    let host = a.rsplit_once(':').map(|(h, _)| h).unwrap_or(a);
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// LAN-mode boundary for mutating requests: the custom header, an IP-literal or
/// loopback Host, and an Origin (if any) naming exactly that host.
pub fn mutation_allowed_lan(
    host: Option<&str>,
    origin: Option<&str>,
    client: Option<&str>,
) -> std::result::Result<(), &'static str> {
    if client != Some("ui") {
        return Err("missing X-Revolve-Client header");
    }
    let Some(h) = host.filter(|h| is_loopback_authority(h) || is_ip_authority(h)) else {
        return Err("unexpected Host header");
    };
    if let Some(o) = origin
        && o.trim_start_matches("http://") != h
    {
        return Err("cross-origin request refused");
    }
    Ok(())
}

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The LAN access key: created once (128 random bits) and kept in the state directory.
fn lan_key(state_dir: &Path) -> Result<String> {
    let p = state_dir.join("lan-access-key");
    if let Ok(k) = std::fs::read_to_string(&p) {
        let k = k.trim().to_string();
        if k.len() == 32 && k.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(k);
        }
    }
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf)
        .map_err(|e| Error::new(format!("no OS randomness for the access key: {e}")))?;
    let k: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&p, &k)?;
    Ok(k)
}

pub fn serve(bind: &str, state_dir: &Path, scenarios_dir: &Path, step_delay_ms: u64) -> Result<()> {
    serve_with(bind, state_dir, scenarios_dir, step_delay_ms, false)
}

pub fn serve_with(
    bind: &str,
    state_dir: &Path,
    scenarios_dir: &Path,
    step_delay_ms: u64,
    lan: bool,
) -> Result<()> {
    let addr = if lan {
        check_bind_lan(bind)?
    } else {
        check_bind(bind)?
    };
    std::fs::create_dir_all(state_dir)?;
    let key = if lan { Some(lan_key(state_dir)?) } else { None };
    let server =
        Server::http(addr).map_err(|e| Error::new(format!("cannot listen on {addr}: {e}")))?;
    let ctx = Arc::new(Ctx {
        lan_key: key.clone(),
        state_dir: state_dir.to_path_buf(),
        scenarios_dir: scenarios_dir.to_path_buf(),
        step_delay_ms,
        children: Mutex::new(HashMap::new()),
        exe: std::env::current_exe()?,
    });
    match &key {
        None => eprintln!("[revolve] UI on http://{addr}/ (loopback only; Ctrl+C to stop)"),
        Some(k) => {
            eprintln!(
                "[revolve] UI on http://{addr}/ (LAN mode: private networks only; Ctrl+C to stop)"
            );
            eprintln!(
                "[revolve] on another device open http://<this computer's IP>:{}/?key={k}  (keep this link private)",
                addr.port()
            );
        }
    }
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
        .with_header(header("X-Revolve-Build", build_id()))
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
        ));
    let _ = req.respond(resp);
}

fn respond_cached(req: Request, body: &'static str) {
    let resp = Response::from_string(body)
        .with_status_code(200)
        .with_header(header("Content-Type", "text/javascript; charset=utf-8"))
        .with_header(header(
            "Cache-Control",
            "public, max-age=31536000, immutable",
        ))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("X-Revolve-Build", build_id()));
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

fn handle(ctx: &Arc<Ctx>, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    let peer = req.remote_addr().map(|a| a.ip());
    let from_here = peer.is_some_and(|ip| ip.is_loopback());
    // DNS-rebinding guard for every request, reads included: only loopback Host
    // names (or, in LAN mode, IP addresses).
    let host_ok =
        |h: &str| is_loopback_authority(h) || (ctx.lan_key.is_some() && is_ip_authority(h));
    if !get_header(&req, "Host").is_some_and(host_ok) {
        return json_resp(req, 403, &json!({"error": "unexpected Host header"}));
    }
    if let Some(key) = &ctx.lan_key
        && !from_here
    {
        if !peer.is_some_and(is_private_ip) {
            return json_resp(
                req,
                403,
                &json!({"error": "only devices on a private network may connect"}),
            );
        }
        // The key arrives once in the link; it is then kept as a strict cookie.
        let query_key = url
            .split_once('?')
            .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("key=")))
            .map(str::to_string);
        if let Some(k) = query_key {
            if same_secret(&k, key) {
                let resp = Response::empty(303)
                    .with_header(header(
                        "Location",
                        if path.is_empty() { "/" } else { &path },
                    ))
                    .with_header(header("Cache-Control", "no-store"))
                    .with_header(header("Referrer-Policy", "no-referrer"))
                    .with_header(header(
                        "Set-Cookie",
                        &format!(
                            "revolve_key={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000"
                        ),
                    ));
                let _ = req.respond(resp);
                return;
            }
            return respond(
                req,
                401,
                "text/plain; charset=utf-8",
                "That access key is not right. Use the link printed by `revolve serve --lan`."
                    .into(),
            );
        }
        let cookie_ok = get_header(&req, "Cookie").is_some_and(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().strip_prefix("revolve_key="))
                .any(|v| same_secret(v, key))
        });
        if !cookie_ok {
            return respond(req, 401, "text/plain; charset=utf-8", "This page needs the access link printed by `revolve serve --lan` on the computer running Revolve.".into());
        }
    }
    if method == Method::Get {
        match path.as_str() {
            "/" | "/index.html" => {
                return respond(
                    req,
                    200,
                    "text/html; charset=utf-8",
                    INDEX_HTML.replace("{{BUILD}}", build_id()),
                );
            }
            "/app.js" => return respond(req, 200, "text/javascript; charset=utf-8", APP_JS.into()),
            "/scene.js" => {
                return respond(req, 200, "text/javascript; charset=utf-8", SCENE_JS.into());
            }
            // Versioned third-party files never change at a given path: cache them.
            "/vendor/three-r186/three.module.js" => return respond_cached(req, THREE_MODULE),
            "/vendor/three-r186/three.core.js" => return respond_cached(req, THREE_CORE),
            "/vendor/three-r186/OrbitControls.js" => return respond_cached(req, THREE_ORBIT),
            "/style.css" => return respond(req, 200, "text/css; charset=utf-8", STYLE_CSS.into()),
            _ => {}
        }
    }
    let boundary = if ctx.lan_key.is_some() {
        mutation_allowed_lan
    } else {
        mutation_allowed
    };
    if method == Method::Post
        && let Err(why) = boundary(
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
        (Method::Post, ["api", "interpret"]) => interpret(ctx, &body),
        (Method::Get, ["api", "programs", "examples"]) => program_examples(),
        (Method::Post, ["api", "programs", "write"]) => program_write_start(ctx, &body),
        (Method::Get, ["api", "programs", "jobs", id]) => job_status(id),
        (Method::Post, ["api", "programs", "check"]) => program_check(ctx, &body),
        (Method::Get, ["api", "runs", id]) => run_view(ctx, id),
        (Method::Get, ["api", "runs", id, "scene"]) => run_scene(ctx, id, &url),
        (Method::Get, ["api", "scenarios", id, "scene"]) => scenario_scene(ctx, id),
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
    for sub in ["playground", "demo", "development", "held-out"] {
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
    if v.get("program").is_some_and(|p| !p.is_null()) {
        return create_program(ctx, &v);
    }
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
    let choice = match v["planner"].as_str().unwrap_or("scripted") {
        "scripted" => PlannerChoice::Scripted,
        "live" => {
            // Every live run from the UI carries its own small, explicit call cap.
            let budget = v["budget"].as_u64().unwrap_or(5).clamp(1, 10) as u32;
            let cfg_path = PathBuf::from(LIVE_CONFIG);
            let cfg = LiveConfig::load(&cfg_path).map_err(|_| {
                Error::new("the AI planner is not configured (config/planner.json)")
            })?;
            if !cfg.key_present() {
                crate::bail!(
                    "the AI planner has no API key ({} is not set)",
                    cfg.api_key_env
                );
            }
            let dir = ctx.state_dir.join("budgets");
            std::fs::create_dir_all(&dir)?;
            let ledger = dir.join(format!("{}.json", crate::util::fresh_id("ui")));
            BudgetLedger::init(&ledger, budget)?;
            PlannerChoice::Live {
                config: cfg_path,
                budget: ledger,
            }
        }
        other => crate::bail!("unknown planner `{other}`"),
    };
    let engine = crate::program::ExecEngine::parse(v["engine"].as_str().unwrap_or("compiled"))
        .map_err(Error::new)?;
    let (run_id, dir) = harness::create_run_with(&ctx.state_dir, &sc, arm, &choice, human, engine)?;
    set_world_speed(&dir, &v)?;
    Ok(json!({"run_id": run_id}))
}

const LIVE_CONFIG: &str = "config/planner.json";

/// Motion speed for runs started from the UI, as a multiple of real time.
const UI_SPEED: f64 = 1.5;

fn set_world_speed(dir: &Path, v: &Value) -> Result<()> {
    let speed = v["speed"].as_f64().unwrap_or(UI_SPEED).clamp(0.25, 8.0);
    SimWorld::open(&dir.join("world.db"))?.set_speed(speed)
}

/// A scenario's table before any run: a fresh world, read and discarded.
fn scenario_scene(ctx: &Ctx, name: &str) -> Result<Value> {
    let name = scenario_name(&json!({"scenario": name}), "messy-table")?;
    let sc = Scenario::load(&scenario::resolve(&name, &ctx.scenarios_dir)?)?;
    let tmp = ctx
        .state_dir
        .join("validate")
        .join(crate::util::fresh_id("scene"));
    std::fs::create_dir_all(&tmp)?;
    let out = SimWorld::create(&tmp.join("world.db"), &sc).and_then(|w| w.scene(0));
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

/// The live robot cell for a 3D view: places, parts, the arm and its motion segments.
fn run_scene(ctx: &Ctx, run_id: &str, url: &str) -> Result<Value> {
    let dir = harness::run_dir(&ctx.state_dir, run_id)?;
    let since = url
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    SimWorld::open(&dir.join("world.db"))?.scene(since)
}

/// AI calls this server process may make in total (a safety net on top of the
/// per-request caps). Restart the server to allow more.
const MAX_AI_CALLS_PER_PROCESS: u32 = 60;
static AI_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn scenario_name(v: &Value, default: &str) -> Result<String> {
    let name = v["scenario"].as_str().unwrap_or(default);
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        crate::bail!("invalid scenario name");
    }
    Ok(name.to_string())
}

/// The table a program is written for: a running run's table as it is now, or a
/// fresh copy of a scenario's table (deleted when dropped).
struct TableSource {
    world: PathBuf,
    goal: GoalSpec,
    obs: crate::model::Observation,
    tmp: Option<PathBuf>,
}

impl Drop for TableSource {
    fn drop(&mut self) {
        if let Some(t) = &self.tmp {
            let _ = std::fs::remove_dir_all(t);
        }
    }
}

fn table_source(ctx: &Ctx, v: &Value) -> Result<TableSource> {
    if let Some(run) = v["run_id"].as_str().filter(|r| !r.is_empty()) {
        let dir = harness::run_dir(&ctx.state_dir, run)?;
        let world = dir.join("world.db");
        let (_, goal) = RunStore::open(&dir)?.current_goal()?;
        let obs = SimWorld::open(&world)?.observe().map_err(Error::new)?;
        return Ok(TableSource {
            world,
            goal,
            obs,
            tmp: None,
        });
    }
    let name = scenario_name(v, "messy-table")?;
    let sc = Scenario::load(&scenario::resolve(&name, &ctx.scenarios_dir)?)?;
    let tmp = ctx
        .state_dir
        .join("validate")
        .join(crate::util::fresh_id("t"));
    std::fs::create_dir_all(&tmp)?;
    let world = tmp.join("world.db");
    let obs = SimWorld::create(&world, &sc)?
        .observe()
        .map_err(Error::new)?;
    Ok(TableSource {
        world,
        goal: sc.goal,
        obs,
        tmp: Some(tmp),
    })
}

fn program_proposal(v: &Value) -> Result<Value> {
    let p = &v["program"];
    let source = p["source"].as_str().unwrap_or("");
    if source.trim().is_empty() || source.len() > crate::script::MAX_SOURCE {
        crate::bail!(
            "a program is required (up to {} characters)",
            crate::script::MAX_SOURCE
        );
    }
    let text = |k: &str, n: usize| {
        p[k].as_str()
            .unwrap_or("")
            .chars()
            .take(n)
            .collect::<String>()
    };
    Ok(json!({
        "language": crate::script::LANGUAGE,
        "source": source,
        "explanation": text("explanation", 600),
        "request": text("request", 800),
        "author": text("author", 80),
        "rehearsal": p.get("rehearsal").cloned().unwrap_or(Value::Null),
    }))
}

fn create_program(ctx: &Ctx, v: &Value) -> Result<Value> {
    let name = scenario_name(v, "messy-table")?;
    let sc = Scenario::load(&scenario::resolve(&name, &ctx.scenarios_dir)?)?;
    let proposal = program_proposal(v)?;
    let author = proposal["author"]
        .as_str()
        .filter(|a| !a.is_empty())
        .unwrap_or("human (ui)")
        .to_string();
    let (run_id, dir) =
        harness::create_program_run(&ctx.state_dir, &sc, proposal, &author, "interactive")?;
    set_world_speed(&dir, v)?;
    Ok(json!({"run_id": run_id}))
}

fn diag_json(d: &[crate::script::Diag], source: &str) -> Value {
    let lines: Vec<&str> = source.lines().collect();
    json!(d.iter().map(|x| json!({"line": x.line, "col": x.col, "message": x.message, "text": lines.get(x.line as usize - 1).map(|l| l.trim_end()).unwrap_or("")})).collect::<Vec<_>>())
}

/// Check, compile and rehearse a program against a table. Nothing real moves.
fn check_and_rehearse(ctx: &Ctx, t: &TableSource, source: &str) -> Result<Value> {
    let env = crate::script::CheckEnv::from(&t.obs, &crate::policy::Policy::default());
    match crate::script::compile(source, &env) {
        Err(d) => Ok(json!({"accepted": false, "diagnostics": diag_json(&d, source)})),
        Ok(c) => {
            let rehearsal = harness::rehearse(&ctx.state_dir, &t.world, &t.goal, source)?;
            Ok(
                json!({"accepted": true, "compiled": c, "rehearsal": rehearsal, "native_target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)}),
            )
        }
    }
}

fn program_examples() -> Result<Value> {
    let mut out = vec![];
    for (id, title, explanation, src) in [
        (
            "trays-by-color",
            "Sort by color into trays",
            "Red blocks go to tray 1, blue to tray 2, green to tray 3 and washers to tray 4. It asks you what to do with anything it can't identify.",
            include_str!("../examples/programs/trays-by-color.rvs"),
        ),
        (
            "sort-to-bins",
            "Put everything back in the bins",
            "Every part on the table goes back into the bin for its kind. Anything unidentified goes to tray 4, because no bin accepts it.",
            include_str!("../examples/programs/sort-to-bins.rvs"),
        ),
        (
            "kits-from-table",
            "Build kits from the table",
            "Fills one tray after another with a red block, a blue block and a washer from the table, until one of them runs out.",
            include_str!("../examples/programs/kits-from-table.rvs"),
        ),
    ] {
        out.push(json!({"id": id, "title": title, "explanation": explanation, "source": src}));
    }
    Ok(json!(out))
}

fn program_check(ctx: &Ctx, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)?;
    let source = v["source"].as_str().unwrap_or("");
    if source.trim().is_empty() || source.len() > crate::script::MAX_SOURCE {
        crate::bail!("a program is required");
    }
    let t = table_source(ctx, &v)?;
    let mut out = check_and_rehearse(ctx, &t, source)?;
    out["by"] = json!("you");
    Ok(out)
}

/// The AI writes a program for the person's words; the checker's messages go
/// back to it for up to two repairs. Then it is rehearsed on a twin. At most 3
/// calls per request, each with a cost estimate from the dated pricing config.
/// Program-writing jobs: the AI's output streams into the job's state, which the
/// page polls, so a person watches the program being written.
fn jobs() -> &'static Mutex<HashMap<String, (std::time::Instant, Value)>> {
    static JOBS: std::sync::OnceLock<Mutex<HashMap<String, (std::time::Instant, Value)>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_job(id: &str, state: Value) {
    if let Ok(mut j) = jobs().lock() {
        j.retain(|_, (t, _)| t.elapsed().as_secs() < 3600);
        j.insert(id.to_string(), (std::time::Instant::now(), state));
    }
}

fn job_status(id: &str) -> Result<Value> {
    let j = jobs().lock().map_err(|_| Error::new("lock poisoned"))?;
    j.get(id)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| Error::new("no such job (it may have expired)"))
}

fn program_write_start(ctx: &Arc<Ctx>, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)?;
    let text = v["text"].as_str().unwrap_or("").trim();
    if text.is_empty() || text.len() > 800 {
        crate::bail!("say what you want in a few sentences (up to 800 characters)");
    }
    let id = crate::util::fresh_id("job");
    set_job(&id, json!({"phase": "starting"}));
    let (ctx, body, jid) = (ctx.clone(), body.to_string(), id.clone());
    std::thread::spawn(move || {
        let progress = |p: Value| set_job(&jid, p);
        match program_write(&ctx, &body, &progress) {
            Ok(r) => set_job(&jid, json!({"phase": "done", "result": r})),
            Err(e) => set_job(&jid, json!({"phase": "error", "error": e.msg})),
        }
    });
    Ok(json!({"job": id}))
}

fn program_write(ctx: &Ctx, body: &str, progress: &dyn Fn(Value)) -> Result<Value> {
    let v: Value = serde_json::from_str(body)?;
    let text = v["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() || text.len() > 800 {
        crate::bail!("say what you want in a few sentences (up to 800 characters)");
    }
    // Changing a running program: pause it first, so nothing new moves while the
    // person decides and the new program is written and rehearsed for exactly the
    // table it will get. The move in progress finishes and is checked.
    let mut paused = false;
    if let Some(run) = v["run_id"].as_str().filter(|r| !r.is_empty()) {
        let dir = harness::run_dir(&ctx.state_dir, run)?;
        let mut store = RunStore::open(&dir)?;
        let st = store.state()?;
        if is_terminal(&st.status) {
            crate::bail!("this run has finished; start a new request instead");
        }
        if st.control == "run" {
            store.request_control("pause")?;
            paused = true;
        }
        drop(store);
        let t0 = std::time::Instant::now();
        while OwnerLock::is_held(&dir) && t0.elapsed().as_secs() < 15 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let t = table_source(ctx, &v)?;
    let cfg = LiveConfig::load(Path::new(LIVE_CONFIG)).map_err(|_| {
        Error::new("Writing programs from your words needs the AI (config/planner.json and an API key). You can still run the examples.")
    })?;
    if !cfg.key_present() {
        crate::bail!(
            "Writing programs needs an API key ({} is not set). You can still run the examples.",
            cfg.api_key_env
        );
    }
    let (pricing, model) = (cfg.pricing.clone(), cfg.model.clone());
    let dir = ctx.state_dir.join("budgets");
    std::fs::create_dir_all(&dir)?;
    let ledger = dir.join(format!("{}.json", crate::util::fresh_id("program")));
    BudgetLedger::init(&ledger, 3)?;
    let mut planner = LivePlanner::new(cfg, &ledger).map_err(|e| Error::new(e.to_string()))?;
    let summary = crate::script::table_summary(&t.obs);
    let env = crate::script::CheckEnv::from(&t.obs, &crate::policy::Policy::default());
    let started = std::time::Instant::now();
    let (mut tin, mut tout) = (0u64, 0u64);
    let mut attempts = vec![];
    let mut previous: Option<(String, Vec<String>)> = None;
    let mut last = None;
    for attempt in 1..=3 {
        progress(
            json!({"phase": "writing", "attempt": attempt, "problems": previous.as_ref().map(|(_, p)| p.clone())}),
        );
        if AI_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_AI_CALLS_PER_PROCESS {
            crate::bail!(
                "this server has used its {MAX_AI_CALLS_PER_PROCESS} AI calls; restart it to allow more"
            );
        }
        let problems = previous.as_ref().map(|(_, p)| p.clone());
        let (r, info) = planner.write_program_streaming(
            &text,
            &summary,
            previous.as_ref().map(|(s, p)| (s.as_str(), p.as_slice())),
            &mut |partial: &str| {
                progress(json!({
                    "phase": "writing",
                    "attempt": attempt,
                    "problems": problems,
                    "explanation": crate::planner::live::partial_json_string(partial, "explanation"),
                    "program": crate::planner::live::partial_json_string(partial, "program"),
                }))
            },
        );
        progress(json!({"phase": "checking", "attempt": attempt}));
        if let Some((a, b)) = info.usage {
            tin += a;
            tout += b;
        }
        let w = match r {
            Ok(w) => w,
            // A cut-off or garbled answer is one failed attempt: ask again.
            Err(crate::planner::PlannerError::Malformed { message }) if attempt < 3 => {
                attempts.push(json!({"accepted": false, "error": message}));
                continue;
            }
            Err(e) => {
                return Err(Error::new(format!(
                    "The AI could not write a program ({e})."
                )));
            }
        };
        if !w.understood && w.source.trim().is_empty() {
            attempts.push(json!({"accepted": false, "understood": false}));
            last = Some((w, None));
            break;
        }
        match crate::script::compile(&w.source, &env) {
            Ok(c) => {
                attempts.push(json!({"accepted": true}));
                last = Some((w, Some(c)));
                break;
            }
            Err(d) => {
                attempts.push(json!({"accepted": false, "diagnostics": diag_json(&d, &w.source)}));
                let lines: Vec<&str> = w.source.lines().collect();
                let problems = d
                    .iter()
                    .map(|x| {
                        format!(
                            "line {}: {} | {}",
                            x.line,
                            x.message,
                            lines
                                .get(x.line as usize - 1)
                                .map(|l| l.trim())
                                .unwrap_or("")
                        )
                    })
                    .collect();
                previous = Some((w.source.clone(), problems));
                last = Some((w, None));
            }
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let cost = match &pricing {
        Some(p) => json!(crate::planner::live::estimate_cost_usd(p, tin, tout)),
        None => Value::Null,
    };
    let Some((w, compiled)) = last else {
        crate::bail!("the AI did not answer");
    };
    let rehearsal = match &compiled {
        Some(_) => {
            progress(
                json!({"phase": "rehearsing", "program": w.source, "explanation": w.explanation}),
            );
            Some(harness::rehearse(
                &ctx.state_dir,
                &t.world,
                &t.goal,
                &w.source,
            )?)
        }
        None => None,
    };
    let diagnostics = match (&compiled, &previous) {
        (None, Some((src, _))) => crate::script::compile(src, &env)
            .err()
            .map(|d| diag_json(&d, src)),
        _ => None,
    };
    Ok(json!({
        "understood": w.understood,
        "not_possible": w.not_possible,
        "source": w.source,
        "explanation": w.explanation,
        "accepted": compiled.is_some(),
        "compiled": compiled,
        "diagnostics": diagnostics,
        "attempts": attempts,
        "rehearsal": rehearsal,
        "native_target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "by": format!("AI ({model})"),
        "paused_run": paused,
        "seconds": (seconds * 10.0).round() / 10.0,
        "usage": {"input_tokens": tin, "output_tokens": tout},
        "cost_usd_estimate": cost,
    }))
}

/// Turn a person's words into a structured request for them to confirm. Tries the
/// free offline parser first; otherwise one small AI call (with a 1-call cap).
fn interpret(ctx: &Ctx, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)?;
    let text = v["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() || text.len() > 500 {
        crate::bail!("say what you want in a sentence (up to 500 characters)");
    }
    if let Ok(g) = parse_restricted_request(&text) {
        return Ok(
            json!({"understood": true, "goal": g, "summary": "", "not_supported": "", "by": "offline parser (free)"}),
        );
    }
    let cfg = LiveConfig::load(Path::new(LIVE_CONFIG))
        .map_err(|_| Error::new("Offline, only requests like \"Prepare 2 kits. Each kit needs one red block and two washers.\" are understood. To use your own words, configure the AI planner (config/planner.json and an API key)."))?;
    if !cfg.key_present() {
        crate::bail!(
            "To use your own words the AI needs an API key ({} is not set).",
            cfg.api_key_env
        );
    }
    let pricing = cfg.pricing.clone();
    let model = cfg.model.clone();
    let dir = ctx.state_dir.join("budgets");
    std::fs::create_dir_all(&dir)?;
    let ledger = dir.join(format!("{}.json", crate::util::fresh_id("interpret")));
    BudgetLedger::init(&ledger, 1)?;
    let mut planner = LivePlanner::new(cfg, &ledger).map_err(|e| Error::new(e.to_string()))?;
    let (r, info) = planner.interpret(&text);
    let i = r.map_err(|e| Error::new(format!("The AI could not interpret that ({e}).")))?;
    let cost = match (&pricing, info.usage) {
        (Some(p), Some((ti, to))) => json!(crate::planner::live::estimate_cost_usd(p, ti, to)),
        _ => Value::Null,
    };
    Ok(json!({
        "understood": i.understood, "goal": i.goal, "summary": i.summary, "assumptions": i.assumptions, "not_supported": i.not_supported, "rejected_because": i.rejected_because,
        "by": format!("AI ({model})"), "usage": info.usage.map(|(a, b)| json!({"input_tokens": a, "output_tokens": b})), "cost_usd_estimate": cost,
    }))
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
        "program" => {
            // A new program for this run (the person changed what they want). It
            // takes over at the next step boundary; completed moves stay.
            let proposal = program_proposal(&v)?;
            let (rev, seq) = RunStore::open(&dir)?.submit_program(
                &proposal,
                "human (ui)",
                "the person changed the request",
                true,
            )?;
            if !OwnerLock::is_held(&dir) {
                spawn_executor(ctx, run_id)?;
            }
            Ok(
                json!({"ok": true, "message": format!("new program #{seq} submitted (request revision {rev}); it takes over after the current move")}),
            )
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
            if what == "speed" {
                // A viewing preference, not an intervention in the task.
                let s = v["value"]
                    .as_f64()
                    .ok_or_else(|| Error::new("value required"))?
                    .clamp(0.25, 8.0);
                w.set_speed(s)?;
                return Ok(
                    json!({"ok": true, "message": format!("the arm now moves at {s}x real time")}),
                );
            }
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

/// The program part of a run view: the active (or latest) program, what the
/// compiler established, and its call log with source lines.
fn program_view(
    store: &RunStore,
    plans: &[PlanRow],
    actions: &[ActionRow],
    active: Option<i64>,
) -> Result<Value> {
    let is_prog = |p: &PlanRow| {
        p.proposal.get("language").and_then(Value::as_str) == Some(crate::script::LANGUAGE)
    };
    let Some(p) = plans
        .iter()
        .rev()
        .find(|p| Some(p.plan_seq) == active && is_prog(p))
        .or_else(|| plans.iter().rev().find(|p| is_prog(p)))
    else {
        return Ok(Value::Null);
    };
    let built = store
        .events(0, 100_000)?
        .into_iter()
        .rev()
        .find(|e| {
            e.kind == "program_built" && e.message.starts_with(&format!("program #{} ", p.plan_seq))
        })
        .and_then(|e| e.data);
    let line_of = |a: &ActionRow| {
        a.step_id
            .trim_start_matches('L')
            .split('.')
            .next()
            .and_then(|l| l.parse::<u32>().ok())
    };
    let calls: Vec<Value> = actions
        .iter()
        .filter(|a| a.plan_seq == Some(p.plan_seq))
        .map(|a| {
            let args = a.resolved_arguments.clone().unwrap_or_else(|| a.arguments.clone());
            json!({"i": a.seq, "line": line_of(a), "capability": a.capability, "args": args, "status": a.status, "error": a.error, "action_id": a.action_id})
        })
        .collect();
    let history: Vec<Value> = plans
        .iter()
        .filter(|x| is_prog(x))
        .map(|x| json!({"plan_seq": x.plan_seq, "status": x.status, "goal_revision": x.goal_revision, "author": x.planner}))
        .collect();
    Ok(json!({
        "plan_seq": p.plan_seq,
        "status": p.status,
        "source": p.proposal["source"],
        "explanation": p.proposal["explanation"],
        "request": p.proposal["request"],
        "author": p.planner,
        "rehearsal": p.proposal.get("rehearsal"),
        "built": built,
        "diagnostics": p.diagnostics,
        "calls": calls,
        "history": history,
    }))
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
    let program = if meta.planner == "program" {
        program_view(&store, &plans, &actions, active)?
    } else {
        Value::Null
    };
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
        "program": program,
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
    fn lan_mode_accepts_private_networks_and_ip_hosts_only() {
        use std::net::IpAddr;
        for ok in [
            "192.168.1.60",
            "10.0.0.5",
            "172.20.1.1",
            "100.91.63.20",
            "127.0.0.1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::1",
        ] {
            assert!(is_private_ip(ok.parse::<IpAddr>().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "100.128.0.1", "172.32.0.1", "2001:db8::1"] {
            assert!(!is_private_ip(bad.parse::<IpAddr>().unwrap()), "{bad}");
        }
        assert!(check_bind_lan("0.0.0.0:8080").is_ok());
        assert!(check_bind_lan("192.168.1.60:8080").is_ok());
        assert!(check_bind_lan("8.8.8.8:8080").is_err());
        let ui = Some("ui");
        assert!(
            mutation_allowed_lan(
                Some("192.168.1.60:8080"),
                Some("http://192.168.1.60:8080"),
                ui
            )
            .is_ok()
        );
        assert!(mutation_allowed_lan(Some("[fd7a:115c:a1e0::1]:8080"), None, ui).is_ok());
        assert!(
            mutation_allowed_lan(Some("192.168.1.60:8080"), None, None).is_err(),
            "custom header"
        );
        assert!(
            mutation_allowed_lan(Some("evil.example:8080"), None, ui).is_err(),
            "DNS names refused"
        );
        assert!(
            mutation_allowed_lan(Some("192.168.1.60:8080"), Some("http://evil.example"), ui)
                .is_err(),
            "cross-origin"
        );
        assert!(
            mutation_allowed_lan(
                Some("192.168.1.60:8080"),
                Some("http://192.168.1.61:8080"),
                ui
            )
            .is_err(),
            "other origin"
        );
        assert!(
            same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "ab")
        );
    }

    #[test]
    fn served_page_carries_its_build() {
        assert!(
            INDEX_HTML.contains("{{BUILD}}"),
            "page template must have the build placeholder"
        );
        assert_eq!(build_id().len(), 12);
        assert!(APP_JS.contains("revolve-build") && APP_JS.contains("X-Revolve-Build"));
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
