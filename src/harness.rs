//! Run lifecycle shared by the CLI, the web server and the benchmark:
//! create a run directory, build the planner for a run, drive it in-process, or
//! supervise an executor child process (restarting after a simulated crash and
//! playing the scripted human for demo/benchmark runs).

use crate::bail;
use crate::gateway::{HumanHook, ScriptedHuman};
use crate::planner::Planner;
use crate::planner::faulty::FaultyPlanner;
use crate::planner::fixed::FixedPlanner;
use crate::planner::live::{LiveConfig, LivePlanner};
use crate::planner::scripted::ScriptedPlanner;
use crate::program::ExecEngine;
use crate::runtime::ExecOptions;
use crate::scenario::Scenario;
use crate::sim::SimWorld;
use crate::store::*;
use crate::util::{Error, Result, SIMULATED_CRASH_EXIT_CODE, fresh_id, now_ms};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    Revolve,
    Baseline,
}

impl Arm {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arm::Revolve => "revolve",
            Arm::Baseline => "baseline",
        }
    }
    pub fn parse(s: &str) -> Result<Arm> {
        match s {
            "revolve" => Ok(Arm::Revolve),
            "baseline" | "direct" => Ok(Arm::Baseline),
            other => bail!("unknown arm `{other}` (revolve | baseline)"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlannerChoice {
    Scripted,
    Fixed(PathBuf),
    /// Live provider: config file + budget ledger path.
    Live {
        config: PathBuf,
        budget: PathBuf,
    },
}

pub fn runs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("runs")
}

pub fn run_dir(state_dir: &Path, run_id: &str) -> Result<PathBuf> {
    if run_id.is_empty()
        || !run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid run id `{run_id}`");
    }
    let d = runs_dir(state_dir).join(run_id);
    if !d.join("run.db").exists() {
        bail!(
            "run `{run_id}` not found under {}",
            runs_dir(state_dir).display()
        );
    }
    Ok(d)
}

pub fn list_runs(state_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(runs_dir(state_dir))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().join("run.db").exists())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// Create a fresh run directory: scenario copy, simulator world, run store.
/// Create a run whose plans execute as compiled programs (the default engine).
pub fn create_run(
    state_dir: &Path,
    sc: &Scenario,
    arm: Arm,
    planner: &PlannerChoice,
    human_mode: &str,
) -> Result<(String, PathBuf)> {
    create_run_with(
        state_dir,
        sc,
        arm,
        planner,
        human_mode,
        ExecEngine::Compiled,
    )
}

pub fn create_run_with(
    state_dir: &Path,
    sc: &Scenario,
    arm: Arm,
    planner: &PlannerChoice,
    human_mode: &str,
    engine: ExecEngine,
) -> Result<(String, PathBuf)> {
    let run_id = fresh_id(&format!(
        "run-{}",
        sc.id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(24)
            .collect::<String>()
    ));
    let dir = runs_dir(state_dir).join(&run_id);
    if dir.exists() {
        bail!("run directory {} already exists", dir.display());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("scenario.json"), serde_json::to_string_pretty(sc)?)?;
    let label = match planner {
        PlannerChoice::Scripted => "scripted".to_string(),
        PlannerChoice::Fixed(p) => {
            let text = std::fs::read_to_string(p)
                .map_err(|e| Error::new(format!("reading plan {}: {e}", p.display())))?;
            let _: Value = serde_json::from_str(&text)
                .map_err(|e| Error::new(format!("plan {} is not JSON: {e}", p.display())))?;
            std::fs::write(dir.join("fixed-plan.json"), text)?;
            "fixed-plan".to_string()
        }
        PlannerChoice::Live { config, budget } => {
            let cfg = LiveConfig::load(config)?;
            std::fs::write(
                dir.join("planner-config.json"),
                serde_json::to_string_pretty(&cfg)?,
            )?;
            std::fs::write(dir.join("budget-ledger.path"), budget.display().to_string())?;
            format!("live:{}/{}", cfg.provider, cfg.model)
        }
    };
    let mut world = SimWorld::create(&dir.join("world.db"), sc)?;
    world.apply_untriggered()?;
    let meta = RunMeta {
        run_id: run_id.clone(),
        arm: arm.as_str().into(),
        planner: label,
        scenario_id: sc.id.clone(),
        human_mode: human_mode.into(),
        created_at_ms: now_ms(),
        seed: sc.seed,
        engine: match arm {
            Arm::Revolve => engine.as_str().into(),
            Arm::Baseline => "direct".into(),
        },
    };
    RunStore::create(&dir, &meta, &sc.goal, &sc.inventory_record())?;
    Ok((run_id, dir))
}

/// Build the planner recorded for a run. A live run never falls back to scripted.
pub fn planner_for(dir: &Path, meta: &RunMeta) -> Result<Box<dyn Planner>> {
    let inner: Box<dyn Planner> = if meta.planner == "scripted" {
        Box::new(ScriptedPlanner)
    } else if meta.planner == "fixed-plan" {
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("fixed-plan.json"))?)?;
        Box::new(FixedPlanner::new(v))
    } else if meta.planner.starts_with("live:") {
        let cfg = LiveConfig::load(&dir.join("planner-config.json"))?;
        let budget = PathBuf::from(std::fs::read_to_string(dir.join("budget-ledger.path"))?.trim());
        match LivePlanner::new(cfg, &budget) {
            Ok(p) => Box::new(p),
            Err(e) => bail!(
                "live planner selected for this run but unavailable: {e}. (Not substituting the scripted planner.)"
            ),
        }
    } else {
        bail!("unknown planner `{}` recorded for run", meta.planner)
    };
    Ok(Box::new(FaultyPlanner::new(inner, dir.join("world.db"))))
}

/// Drive a run once in this process (the executor for `resume`).
pub fn drive_in_process(dir: &Path, opts: ExecOptions) -> Result<String> {
    let store = RunStore::open(dir)?;
    let meta = store.meta()?;
    drop(store);
    let planner = planner_for(dir, &meta)?;
    let hook: Option<Box<dyn HumanHook>> = if meta.human_mode == "scripted" {
        Some(Box::new(ScriptedHuman))
    } else {
        None
    };
    match meta.arm.as_str() {
        "baseline" => crate::baseline::drive(dir, planner, hook, opts),
        _ => crate::runtime::drive(dir, planner, hook, opts),
    }
}

/// Answer an open question as the scripted human, using the scenario's answers.
/// Classification "truth" consults simulator truth — the harness is allowed to; the planner is not.
pub fn scripted_answer(dir: &Path) -> Result<Option<String>> {
    let mut store = RunStore::open(dir)?;
    let sc: Scenario = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json"))?)?;
    let Some(q) = store.open_questions()?.into_iter().next() else {
        return Ok(None);
    };
    let choice = match q.kind.as_str() {
        "classification" => {
            let obj = q
                .context
                .get("object_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let wanted = q.options.first().map(|o| o.id.clone()).unwrap_or_default();
            match sc
                .human
                .answers
                .classification
                .as_deref()
                .unwrap_or("truth")
            {
                "truth" => {
                    let truth = SimWorld::open(&dir.join("world.db"))?.truth()?;
                    let tk = truth
                        .objects
                        .iter()
                        .find(|o| o.object_id == obj)
                        .map(|o| o.true_kind.clone())
                        .unwrap_or_default();
                    if tk == wanted {
                        wanted
                    } else {
                        "not_usable".into()
                    }
                }
                other => other.to_string(),
            }
        }
        "substitution" => sc
            .human
            .answers
            .substitution
            .clone()
            .unwrap_or_else(|| "decline".into()),
        "rework" => sc
            .human
            .answers
            .rework
            .clone()
            .unwrap_or_else(|| "decline".into()),
        _ => q.options.first().map(|o| o.id.clone()).unwrap_or_default(),
    };
    let choice = if q.options.iter().any(|o| o.id == choice) {
        choice
    } else {
        q.options.last().map(|o| o.id.clone()).unwrap_or_default()
    };
    let outcome = store.answer(q.question_id, &choice, "scripted-human")?;
    let msg = format!(
        "[scripted human] answered question #{} ({}) with `{choice}` -> {outcome}",
        q.question_id, q.kind
    );
    SimWorld::open(&dir.join("world.db"))?.log_harness(
        "human_intervention",
        &json!({"type": "answer", "kind": q.kind, "choice": choice}),
    )?;
    Ok(Some(msg))
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SupervisionLog {
    pub executor_processes: u32,
    pub simulated_crashes: u32,
    pub human_interventions: Vec<String>,
    pub peak_rss_bytes: Vec<u64>,
    pub final_status: String,
    pub wall_ms: i64,
}

/// Supervise executor child processes until the run reaches a state the scripted
/// human cannot (or should not) change. `interactive` disables the scripted human.
pub fn supervise(
    state_dir: &Path,
    run_id: &str,
    step_delay_ms: u64,
    quiet: bool,
) -> Result<SupervisionLog> {
    supervise_with(
        &std::env::current_exe()?,
        state_dir,
        run_id,
        step_delay_ms,
        quiet,
    )
}

/// As `supervise`, with an explicit executor binary (tests pass the built `revolve`).
pub fn supervise_with(
    exe: &Path,
    state_dir: &Path,
    run_id: &str,
    step_delay_ms: u64,
    quiet: bool,
) -> Result<SupervisionLog> {
    let dir = run_dir(state_dir, run_id)?;
    let t0 = now_ms();
    let mut log = SupervisionLog::default();
    let meta = RunStore::open(&dir)?.meta()?;
    let scripted = meta.human_mode == "scripted";
    let sc: Scenario = serde_json::from_str(&std::fs::read_to_string(dir.join("scenario.json"))?)?;
    for _round in 0..24 {
        let mut cmd = Command::new(exe);
        cmd.arg("resume")
            .arg("--run")
            .arg(run_id)
            .arg("--state-dir")
            .arg(state_dir)
            .arg("--step-delay-ms")
            .arg(step_delay_ms.to_string())
            .arg("--report-rss");
        if quiet {
            cmd.arg("--quiet");
        }
        if !quiet {
            cmd.stderr(std::process::Stdio::inherit());
        }
        let out = cmd.output()?;
        log.executor_processes += 1;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !quiet {
            eprint!("{stderr}");
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(v) = line.strip_prefix("peak_rss_bytes=")
                && let Ok(n) = v.trim().parse()
            {
                log.peak_rss_bytes.push(n);
            }
        }
        let code = out.status.code();
        if code == Some(SIMULATED_CRASH_EXIT_CODE) {
            log.simulated_crashes += 1;
            if !quiet {
                eprintln!(
                    "[supervisor] executor process exited with the simulated-crash code {SIMULATED_CRASH_EXIT_CODE}; starting a new executor to recover"
                );
            }
            continue;
        }
        if code != Some(0) {
            bail!(
                "executor exited with {:?}: {}",
                code,
                stderr.lines().last().unwrap_or("")
            );
        }
        let status = RunStore::open(&dir)?.state()?.status;
        log.final_status = status.clone();
        if !scripted {
            break;
        }
        match status.as_str() {
            R_AWAITING_HUMAN => {
                if let Some(m) = scripted_answer(&dir)? {
                    if !quiet {
                        eprintln!("{m}");
                    }
                    log.human_interventions.push("answer".into());
                    continue;
                }
                break;
            }
            R_BLOCKED if sc.human.restore_evidence_when_blocked => {
                let mut w = SimWorld::open(&dir.join("world.db"))?;
                if !w.truth()?.blackout {
                    break;
                }
                w.set_blackout(false)?;
                w.log_harness("human_intervention", &json!({"type": "restore_evidence"}))?;
                if !quiet {
                    eprintln!(
                        "[scripted human] inspected the workspace and restored evidence (status service and observation); resuming"
                    );
                }
                log.human_interventions.push("restore_evidence".into());
            }
            R_AWAITING_PLANNER if sc.human.restore_planner_when_waiting => {
                let mut w = SimWorld::open(&dir.join("world.db"))?;
                if !w.planner_offline()? {
                    break;
                }
                w.set_planner_offline(false)?;
                w.log_harness("human_intervention", &json!({"type": "restore_planner"}))?;
                if !quiet {
                    eprintln!("[scripted human] planner connection restored; resuming");
                }
                log.human_interventions.push("restore_planner".into());
            }
            _ => break,
        }
    }
    log.wall_ms = now_ms() - t0;
    Ok(log)
}
