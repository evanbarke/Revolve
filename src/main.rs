use clap::{Parser, Subcommand};
use revolve::harness::{self, Arm, PlannerChoice};
use revolve::model::parse_restricted_request;
use revolve::planner::live::{BudgetLedger, LiveConfig};
use revolve::program::ExecEngine;
use revolve::runtime::ExecOptions;
use revolve::scenario::{self, Scenario};
use revolve::sim::SimWorld;
use revolve::store::*;
use revolve::util::{Error, Result};
use revolve::{capability, codegen, compiler, eval, inventory, program, server};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit code for "feature unavailable" (e.g. live planner not configured).
const EXIT_UNAVAILABLE: u8 = 3;

#[derive(Parser)]
#[command(
    name = "revolve",
    version,
    about = "Revolve: explicit-intent execution runtime prototype (offline simulator)"
)]
struct Cli {
    /// Project-local state directory (runs, benchmark output). Env: REVOLVE_STATE_DIR.
    #[arg(
        long,
        global = true,
        env = "REVOLVE_STATE_DIR",
        default_value = "state"
    )]
    state_dir: PathBuf,
    /// Scenario directory. Env: REVOLVE_SCENARIOS_DIR.
    #[arg(
        long,
        global = true,
        env = "REVOLVE_SCENARIOS_DIR",
        default_value = "scenarios"
    )]
    scenarios_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the environment and report what is available (read-only).
    Doctor,
    /// List scenarios.
    Scenarios,
    /// Print capability contracts.
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Show how the scripted mode's restricted grammar parses a request (no AI interpretation).
    ParseRequest { text: String },
    /// Create a fresh run and drive it to a stable state (scripted human; restarts after simulated crashes).
    Demo {
        #[arg(long)]
        scenario: String,
        /// scripted | configured
        #[arg(long, default_value = "scripted")]
        planner: String,
        /// revolve | baseline
        #[arg(long, default_value = "revolve")]
        arm: String,
        #[arg(long, default_value = "config/planner.json")]
        planner_config: PathBuf,
        /// Explicit API-call budget (required for --planner configured).
        #[arg(long)]
        budget: Option<u32>,
        #[arg(long, default_value_t = 0)]
        step_delay_ms: u64,
        #[arg(long)]
        quiet: bool,
        /// How Revolve runs plans: compiled (Wasm program, native code) | interpreted
        #[arg(long, default_value = "compiled")]
        engine: String,
    },
    /// Create a run without executing it (for the UI or manual CLI control). Prints the run id.
    New {
        #[arg(long)]
        scenario: String,
        #[arg(long, default_value = "scripted")]
        planner: String,
        #[arg(long, default_value = "revolve")]
        arm: String,
        #[arg(long)]
        plan: Option<PathBuf>,
        #[arg(long, default_value = "config/planner.json")]
        planner_config: PathBuf,
        #[arg(long)]
        budget: Option<u32>,
        /// Let the harness play the human (answers, scenario corrections).
        #[arg(long)]
        scripted_human: bool,
        /// How Revolve runs plans: compiled (Wasm program, native code) | interpreted
        #[arg(long, default_value = "compiled")]
        engine: String,
    },
    /// Execute a handwritten Action IR plan on a fresh scenario (fixed-plan mode).
    Run {
        #[arg(long)]
        scenario: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = 0)]
        step_delay_ms: u64,
        /// How Revolve runs plans: compiled (Wasm program, native code) | interpreted
        #[arg(long, default_value = "compiled")]
        engine: String,
    },
    /// Resume (or start) the executor for an existing run in this process.
    Resume {
        #[arg(long)]
        run: String,
        #[arg(long, default_value_t = 0)]
        step_delay_ms: u64,
        #[arg(long)]
        quiet: bool,
        /// Print `peak_rss_bytes=<n>` on stdout at exit (used by the supervisor).
        #[arg(long, hide = true)]
        report_rss: bool,
    },
    /// Score an existing run against simulator truth (independent evaluator).
    Evaluate {
        #[arg(long)]
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Show run status.
    Status {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Submit a goal correction (new goal revision).
    Correct {
        #[arg(long)]
        run: String,
        #[arg(long)]
        goal_file: Option<PathBuf>,
        /// Restricted-grammar request text instead of a goal file.
        #[arg(long)]
        request: Option<String>,
    },
    /// Answer an open question.
    Answer {
        #[arg(long)]
        run: String,
        #[arg(long)]
        question: i64,
        #[arg(long)]
        choice: String,
    },
    /// Request a pause (no new dispatch; in-flight outcomes are established first).
    Pause {
        #[arg(long)]
        run: String,
    },
    /// Request cancellation; acknowledged by the executor with the resulting state.
    Cancel {
        #[arg(long)]
        run: String,
        /// Seconds to wait for an active executor to acknowledge.
        #[arg(long, default_value_t = 10)]
        wait: u64,
    },
    /// Simulation controls (harness; not visible to planners).
    Sim {
        #[arg(long)]
        run: String,
        /// restore-evidence | blackout | planner-offline | planner-online | unavailable | available | arm-fault
        action: String,
        #[arg(long)]
        location: Option<String>,
        /// Fault JSON for arm-fault, e.g. '{"kind":"crash_after_effect","on":{"capability":"workspace.place","nth":2}}'
        #[arg(long)]
        fault: Option<String>,
    },
    /// Validate (compile) a plan against a scenario's initial state without executing it.
    Validate {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "nominal")]
        scenario: String,
    },
    /// Check a plan against a scenario and compile it to a WebAssembly program
    /// (optionally also to native code for this machine). Nothing is executed.
    Build {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "nominal")]
        scenario: String,
        /// Where to write the .wasm program.
        #[arg(long)]
        out: PathBuf,
        /// Also write the ahead-of-time native code (.cwasm) for this machine.
        #[arg(long)]
        native: Option<PathBuf>,
    },
    /// Serve the browser UI (loopback only).
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        #[arg(long, default_value_t = 400)]
        step_delay_ms: u64,
    },
    /// Run the Revolve-vs-direct-tool benchmark.
    Benchmark {
        /// development | held-out | <directory>
        #[arg(long, default_value = "development")]
        suite: String,
        #[arg(long, default_value = "scripted")]
        planner: String,
        #[arg(long, default_value = "config/planner.json")]
        planner_config: PathBuf,
        #[arg(long)]
        budget: Option<u32>,
        #[arg(long, default_value_t = 1)]
        trials: u32,
        /// Output directory (default: <state-dir>/benchmarks/<suite>-<timestamp>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// revolve,baseline (default both)
        #[arg(long, default_value = "revolve,baseline")]
        arms: String,
        /// How Revolve runs plans: compiled (Wasm program, native code) | interpreted
        #[arg(long, default_value = "compiled")]
        engine: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            if e.msg.contains("unavailable") && e.msg.contains("live planner")
                || e.msg.contains("not set")
            {
                ExitCode::from(EXIT_UNAVAILABLE)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn planner_choice(
    planner: &str,
    plan: Option<&PathBuf>,
    config: &Path,
    budget: Option<u32>,
    state_dir: &Path,
) -> Result<PlannerChoice> {
    if let Some(p) = plan {
        return Ok(PlannerChoice::Fixed(p.clone()));
    }
    match planner {
        "scripted" => Ok(PlannerChoice::Scripted),
        "configured" | "live" => {
            if !config.exists() {
                return Err(Error::new(format!(
                    "live planner unavailable: no planner config at {} (copy config/planner.example.json). Offline modes remain available.",
                    config.display()
                )));
            }
            let cfg = LiveConfig::load(config)?;
            if !cfg.key_present() {
                return Err(Error::new(format!(
                    "live planner unavailable: environment variable {} is not set. Offline modes remain available.",
                    cfg.api_key_env
                )));
            }
            let Some(b) = budget else {
                return Err(Error::new(
                    "live planner requires an explicit --budget <max HTTP calls>; no calls were made",
                ));
            };
            let dir = state_dir.join("budgets");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.json", revolve::util::fresh_id("budget")));
            BudgetLedger::init(&path, b)?;
            eprintln!(
                "[revolve] live planner {} with a budget of {b} HTTP call(s); ledger {}",
                cfg.model,
                path.display()
            );
            Ok(PlannerChoice::Live {
                config: config.to_path_buf(),
                budget: path,
            })
        }
        other => Err(Error::new(format!(
            "unknown planner `{other}` (scripted | configured)"
        ))),
    }
}

fn load_scenario(name: &str, dir: &Path) -> Result<Scenario> {
    Scenario::load(&scenario::resolve(name, dir)?)
}

fn real_main(cli: Cli) -> Result<ExitCode> {
    let sd = &cli.state_dir;
    match cli.cmd {
        Cmd::Doctor => doctor(sd, &cli.scenarios_dir),
        Cmd::Scenarios => {
            for sub in ["demo", "development", "held-out"] {
                let files = scenario::list_dir(&cli.scenarios_dir.join(sub));
                println!("{sub} ({}):", files.len());
                for f in files {
                    match Scenario::load(&f) {
                        Ok(s) => println!("  {:<34} [{}] {}", s.id, s.family, s.description),
                        Err(e) => println!("  {} INVALID: {e}", f.display()),
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Capabilities { json } => {
            let reg = capability::registry();
            if json {
                println!("{}", serde_json::to_string_pretty(&reg)?);
            } else {
                for c in reg {
                    println!(
                        "{} v{} [{:?}; idempotency_key={}, status_lookup={}, completion={:?}, plan_step={}]\n    {}\n    pre: {}\n    effects: {}",
                        c.id,
                        c.version,
                        c.effect_class,
                        c.idempotency_key,
                        c.status_lookup,
                        c.completion,
                        c.plan_step_allowed,
                        c.summary,
                        c.preconditions,
                        c.effects
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::ParseRequest { text } => match parse_restricted_request(&text) {
            Ok(g) => {
                println!("{}", serde_json::to_string_pretty(&g)?);
                println!("(parsed by the scripted restricted grammar — not an AI interpretation)");
                Ok(ExitCode::SUCCESS)
            }
            Err(e) => {
                eprintln!("not understood: {e}");
                Ok(ExitCode::from(1))
            }
        },
        Cmd::Demo {
            scenario: name,
            planner,
            arm,
            planner_config,
            budget,
            step_delay_ms,
            quiet,
            engine,
        } => {
            let sc = load_scenario(&name, &cli.scenarios_dir)?;
            let choice = planner_choice(&planner, None, &planner_config, budget, sd)?;
            let engine = ExecEngine::parse(&engine).map_err(Error::new)?;
            let (run_id, dir) =
                harness::create_run_with(sd, &sc, Arm::parse(&arm)?, &choice, "scripted", engine)?;
            println!(
                "run {run_id}  scenario {}  arm {arm}  planner {planner}  engine {}",
                sc.id,
                if arm == "baseline" {
                    "direct"
                } else {
                    engine.as_str()
                }
            );
            println!("state: {}", dir.display());
            let log = harness::supervise(sd, &run_id, step_delay_ms, quiet)?;
            print_status(&dir, false)?;
            println!(
                "supervisor: {} executor process(es), {} simulated crash(es), human interventions: [{}]",
                log.executor_processes,
                log.simulated_crashes,
                log.human_interventions.join(", ")
            );
            let ev = eval::evaluate_run(&dir)?;
            println!("\nindependent evaluation (simulator truth):");
            println!("{}", eval::summarize_verdict(&ev));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::New {
            scenario: name,
            planner,
            arm,
            plan,
            planner_config,
            budget,
            scripted_human,
            engine,
        } => {
            let sc = load_scenario(&name, &cli.scenarios_dir)?;
            let choice = planner_choice(&planner, plan.as_ref(), &planner_config, budget, sd)?;
            let (run_id, _dir) = harness::create_run_with(
                sd,
                &sc,
                Arm::parse(&arm)?,
                &choice,
                if scripted_human {
                    "scripted"
                } else {
                    "interactive"
                },
                ExecEngine::parse(&engine).map_err(Error::new)?,
            )?;
            println!("{run_id}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run {
            scenario: name,
            plan,
            step_delay_ms,
            engine,
        } => {
            let sc = load_scenario(&name, &cli.scenarios_dir)?;
            let engine = ExecEngine::parse(&engine).map_err(Error::new)?;
            let (run_id, dir) = harness::create_run_with(
                sd,
                &sc,
                Arm::Revolve,
                &PlannerChoice::Fixed(plan),
                "scripted",
                engine,
            )?;
            println!(
                "run {run_id}  scenario {}  planner fixed-plan  engine {}",
                sc.id,
                engine.as_str()
            );
            let log = harness::supervise(sd, &run_id, step_delay_ms, false)?;
            print_status(&dir, false)?;
            println!(
                "supervisor: {} executor process(es), {} simulated crash(es)",
                log.executor_processes, log.simulated_crashes
            );
            let ev = eval::evaluate_run(&dir)?;
            println!(
                "\nindependent evaluation (simulator truth):\n{}",
                eval::summarize_verdict(&ev)
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Resume {
            run,
            step_delay_ms,
            quiet,
            report_rss,
        } => {
            let dir = harness::run_dir(sd, &run)?;
            let opts = ExecOptions {
                step_delay_ms,
                quiet,
                ..Default::default()
            };
            let status = harness::drive_in_process(&dir, opts)?;
            if report_rss && let Some(b) = revolve::util::peak_rss_bytes() {
                println!("peak_rss_bytes={b}");
            }
            if !quiet {
                eprintln!("[revolve] executor exiting with run status `{status}`");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Evaluate { run, json } => {
            let dir = harness::run_dir(sd, &run)?;
            let v = eval::evaluate_run(&dir)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", eval::summarize_verdict(&v));
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Status { run, json } => {
            let run = match run {
                Some(r) => r,
                None => {
                    for r in harness::list_runs(sd).iter().take(20) {
                        let dir = harness::run_dir(sd, r)?;
                        let s = RunStore::open(&dir)?;
                        let (st, meta) = (s.state()?, s.meta()?);
                        println!(
                            "{r}  {:<17} {:<9} {:<10} {}",
                            st.status, meta.arm, meta.planner, meta.scenario_id
                        );
                    }
                    return Ok(ExitCode::SUCCESS);
                }
            };
            let dir = harness::run_dir(sd, &run)?;
            print_status(&dir, json)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Correct {
            run,
            goal_file,
            request,
        } => {
            let dir = harness::run_dir(sd, &run)?;
            let mut store = RunStore::open(&dir)?;
            let goal = match (goal_file, request) {
                (Some(f), None) => {
                    let text = std::fs::read_to_string(&f)
                        .map_err(|e| Error::new(format!("reading {}: {e}", f.display())))?;
                    serde_json::from_str(&text)
                        .map_err(|e| Error::new(format!("goal file {}: {e}", f.display())))?
                }
                (None, Some(t)) => parse_restricted_request(&t)?,
                _ => return Err(Error::new("give exactly one of --goal-file or --request")),
            };
            let rev = store.submit_goal_revision(&goal, "human (cli)", "correction")?;
            println!("goal revision {rev} recorded: {}", goal.summary());
            if OwnerLock::is_held(&dir) {
                println!(
                    "an executor owns this run and will apply the correction before its next dispatch"
                );
            } else {
                println!(
                    "no executor is running; `revolve resume --run {run}` will replan against revision {rev}"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Answer {
            run,
            question,
            choice,
        } => {
            let dir = harness::run_dir(sd, &run)?;
            let mut store = RunStore::open(&dir)?;
            let out = store.answer(question, &choice, "human (cli)")?;
            println!(
                "answered question #{question}: {out}; run `revolve resume --run {run}` to continue"
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pause { run } => {
            let dir = harness::run_dir(sd, &run)?;
            RunStore::open(&dir)?.request_control("pause")?;
            println!("pause requested; no new actions will be claimed");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cancel { run, wait } => {
            let dir = harness::run_dir(sd, &run)?;
            RunStore::open(&dir)?.request_control("cancel")?;
            if !OwnerLock::is_held(&dir) {
                println!("cancel requested; no executor running — processing it now");
                harness::drive_in_process(
                    &dir,
                    ExecOptions {
                        quiet: true,
                        ..Default::default()
                    },
                )?;
            } else {
                println!("cancel requested; waiting up to {wait}s for the executor to acknowledge");
                let t = std::time::Instant::now();
                while t.elapsed().as_secs() < wait && OwnerLock::is_held(&dir) {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
            let st = RunStore::open(&dir)?.state()?;
            println!(
                "run status: {} ({})",
                st.status,
                st.status_reason.unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Sim {
            run,
            action,
            location,
            fault,
        } => {
            let dir = harness::run_dir(sd, &run)?;
            let mut w = SimWorld::open(&dir.join("world.db"))?;
            match action.as_str() {
                "restore-evidence" => w.set_blackout(false)?,
                "blackout" => w.set_blackout(true)?,
                "planner-offline" => w.set_planner_offline(true)?,
                "planner-online" => w.set_planner_offline(false)?,
                "unavailable" | "available" => {
                    let loc = location.ok_or_else(|| Error::new("--location required"))?;
                    w.set_location_available(&loc, action == "available")?;
                }
                "arm-fault" => {
                    let f: revolve::scenario::FaultSpec = serde_json::from_str(
                        &fault.ok_or_else(|| Error::new("--fault required"))?,
                    )?;
                    let id = w.arm_fault(&f)?;
                    println!("armed fault #{id}");
                }
                other => return Err(Error::new(format!("unknown sim action `{other}`"))),
            }
            w.log_harness(
                "human_intervention",
                &json!({"type": "sim_control", "action": action}),
            )?;
            println!("simulation control applied: {action}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Validate {
            plan,
            scenario: name,
        } => validate(&plan, &name, &cli.scenarios_dir, sd),
        Cmd::Build {
            plan,
            scenario: name,
            out,
            native,
        } => build(
            &plan,
            &name,
            &cli.scenarios_dir,
            sd,
            &out,
            native.as_deref(),
        ),
        Cmd::Serve {
            bind,
            step_delay_ms,
        } => {
            server::serve(&bind, sd, &cli.scenarios_dir, step_delay_ms)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Benchmark {
            suite,
            planner,
            planner_config,
            budget,
            trials,
            out,
            arms,
            engine,
        } => {
            let engine = ExecEngine::parse(&engine).map_err(Error::new)?;
            let dir = match suite.as_str() {
                "development" | "held-out" | "demo" => cli.scenarios_dir.join(&suite),
                other => PathBuf::from(other),
            };
            let choice = planner_choice(&planner, None, &planner_config, budget, sd)?;
            let arms: Vec<Arm> = arms
                .split(',')
                .map(|a| Arm::parse(a.trim()))
                .collect::<Result<_>>()?;
            let out = out.unwrap_or_else(|| {
                sd.join("benchmarks").join(format!(
                    "{}-{}",
                    suite.replace(['/', '\\'], "_"),
                    revolve::util::now_ms()
                ))
            });
            let summary = eval::run_benchmark(sd, &dir, &choice, &arms, trials, &out, engine)?;
            println!("{summary}");
            println!("results written to {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn doctor(sd: &Path, scen: &Path) -> Result<ExitCode> {
    println!(
        "revolve {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("sqlite (bundled): {}", rusqlite::version());
    let tegra = Path::new("/etc/nv_tegra_release");
    if tegra.exists() {
        let first = std::fs::read_to_string(tegra)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        println!("jetson: {first}");
    } else {
        println!("jetson: not detected (no /etc/nv_tegra_release)");
    }
    std::fs::create_dir_all(sd)?;
    let probe = sd.join(".doctor-probe");
    let writable = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    println!(
        "state dir {}: {}",
        sd.display(),
        if writable { "writable" } else { "NOT writable" }
    );
    for sub in ["demo", "development", "held-out"] {
        println!(
            "scenarios/{sub}: {} file(s)",
            scenario::list_dir(&scen.join(sub)).len()
        );
    }
    let cfg = Path::new("config/planner.json");
    if cfg.exists() {
        match LiveConfig::load(cfg) {
            Ok(c) => println!(
                "live planner: config present ({} / {}); key env {} is {}",
                c.provider,
                c.model,
                c.api_key_env,
                if c.key_present() { "set" } else { "NOT set" }
            ),
            Err(e) => println!("live planner: config invalid: {e}"),
        }
    } else {
        println!(
            "live planner: not configured (config/planner.json absent) — scripted mode only; live validation not performed"
        );
    }
    if let Some(b) = revolve::util::peak_rss_bytes() {
        println!("peak RSS of this process: {} KiB", b / 1024);
    }
    println!("robot hardware: none (simulator only)");
    Ok(ExitCode::SUCCESS)
}

/// Read a plan file and check it against a scenario's initial state (throwaway
/// world copy). `Ok(None)` means the file is not JSON (already reported).
fn check_plan(
    plan: &Path,
    name: &str,
    scen: &Path,
    sd: &Path,
) -> Result<Option<std::result::Result<compiler::CompiledPlan, Vec<compiler::Diagnostic>>>> {
    let sc = load_scenario(name, scen)?;
    let text = std::fs::read_to_string(plan)
        .map_err(|e| Error::new(format!("reading {}: {e}", plan.display())))?;
    let raw: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            println!("INVALID [Syntax] not JSON: {e}");
            return Ok(None);
        }
    };
    let tmp = sd.join("validate").join(revolve::util::fresh_id("v"));
    std::fs::create_dir_all(&tmp)?;
    let result =
        (|| -> Result<std::result::Result<compiler::CompiledPlan, Vec<compiler::Diagnostic>>> {
            let mut world = SimWorld::create(&tmp.join("world.db"), &sc)?;
            let obs = world.observe().map_err(Error::new)?;
            let inv = inventory::InventoryView {
                on_hand: sc.inventory_record(),
                reserved: Default::default(),
                reservations: vec![],
            };
            let policy = revolve::policy::Policy::default();
            let goal_id = format!("goal-{}", sc.id);
            Ok(compiler::parse_proposal(&raw).and_then(|p| {
                compiler::compile(
                    &p,
                    &compiler::CompileContext {
                        goal_id: &goal_id,
                        goal_revision: 1,
                        goal: &sc.goal,
                        observation: &obs,
                        classifications: &[],
                        inventory: &inv,
                        consumed_evidence: &[],
                        policy: &policy,
                        limits: compiler::Limits::default(),
                    },
                )
            }))
        })();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(Some(result?))
}

fn print_diagnostics(diags: &[compiler::Diagnostic]) {
    println!("INVALID: {} diagnostic(s)", diags.len());
    for d in diags {
        println!(
            "  [{:?}] {}{}: {}",
            d.category,
            d.step_id
                .as_deref()
                .map(|s| format!("step `{s}` "))
                .unwrap_or_default(),
            d.path,
            d.message
        );
    }
}

fn build(
    plan: &Path,
    name: &str,
    scen: &Path,
    sd: &Path,
    out: &Path,
    native: Option<&Path>,
) -> Result<ExitCode> {
    let cp = match check_plan(plan, name, scen, sd)? {
        None => return Ok(ExitCode::from(1)),
        Some(Err(diags)) => {
            print_diagnostics(&diags);
            return Ok(ExitCode::from(1));
        }
        Some(Ok(cp)) => cp,
    };
    let steps: Vec<codegen::StepSrc> = cp.steps.iter().map(codegen::StepSrc::from_step).collect();
    let prog = codegen::generate(&steps).map_err(Error::new)?;
    std::fs::write(out, &prog.wasm)?;
    println!(
        "checked: {} step(s); program {} ({} bytes, {}, sha256 {})",
        cp.steps.len(),
        out.display(),
        prog.wasm.len(),
        codegen::CODEGEN,
        prog.sha256
    );
    println!("imports only: {}", prog.imports.join(", "));
    if let Some(n) = native {
        let code = program::precompile(&prog.wasm).map_err(Error::new)?;
        std::fs::write(n, &code)?;
        println!(
            "native code for {}-{} (Cranelift, wasmtime 47): {} ({} bytes)",
            std::env::consts::ARCH,
            std::env::consts::OS,
            n.display(),
            code.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn validate(plan: &Path, name: &str, scen: &Path, sd: &Path) -> Result<ExitCode> {
    let result = match check_plan(plan, name, scen, sd)? {
        None => return Ok(ExitCode::from(1)),
        Some(r) => r,
    };
    match result {
        Ok(cp) => {
            println!(
                "VALID: {} step(s) in dispatch order; predicted complete kits: [{}]",
                cp.steps.len(),
                cp.predicted_complete_trays.join(", ")
            );
            println!(
                "(checked: schema, structure, binding, authorisation, budget, belief-level feasibility — not execution)"
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(diags) => {
            print_diagnostics(&diags);
            Ok(ExitCode::from(1))
        }
    }
}

fn print_status(dir: &Path, as_json: bool) -> Result<()> {
    let store = RunStore::open(dir)?;
    let st = store.state()?;
    let meta = store.meta()?;
    let (rev, goal) = store.current_goal()?;
    let actions = store.actions()?;
    let inv = inventory::view(&store)?;
    if as_json {
        let v = json!({
            "meta": meta, "state": st, "goal_revision": rev, "goal": goal,
            "inventory": inv, "questions": store.questions()?,
            "actions": actions, "plans": store.plans()?.iter().map(|p| json!({"plan_seq": p.plan_seq, "status": p.status, "goal_revision": p.goal_revision, "diagnostics": p.diagnostics})).collect::<Vec<_>>(),
            "planner_calls": store.planner_call_stats()?,
            "executor_active": OwnerLock::is_held(dir),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("\n== run {} ==", meta.run_id);
    println!(
        "scenario {} | arm {} | planner {} | human {}",
        meta.scenario_id, meta.arm, meta.planner, meta.human_mode
    );
    println!(
        "status: {}{}",
        st.status,
        st.status_reason
            .as_ref()
            .map(|r| format!(" — {r}"))
            .unwrap_or_default()
    );
    println!("goal r{rev}: {}", goal.summary());
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for a in &actions {
        *counts.entry(a.status.clone()).or_default() += 1;
    }
    println!(
        "actions: {}",
        counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "inventory record: {}",
        inv.on_hand
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for q in store.open_questions()? {
        println!(
            "OPEN QUESTION #{}: {}  options: {}",
            q.question_id,
            q.prompt,
            q.options
                .iter()
                .map(|o| o.id.clone())
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    if let Some(r) = &st.final_report {
        if r.get("claims_source").and_then(Value::as_str) == Some("agent") {
            println!("agent claims complete kits: {}", r["kits_claimed_complete"]);
        } else {
            println!("verified complete kits: {}", r["kits_verified_complete"]);
            println!("missing to finish: {}", r["missing_to_finish"]);
            if let Some(d) = r["inventory_discrepancies"].as_array()
                && !d.is_empty()
            {
                println!("inventory discrepancies: {}", r["inventory_discrepancies"]);
            }
            if let Some(n) = r["notes"].as_array() {
                for note in n {
                    println!("note: {}", note.as_str().unwrap_or(""));
                }
            }
        }
    }
    println!("planner calls: {}", store.planner_call_stats()?);
    println!("executor active: {}", OwnerLock::is_held(dir));
    Ok(())
}
