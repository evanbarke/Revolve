//! Executor-owned persistent state (`run.db`): run status, goal revisions, plans,
//! actions, events, questions, human-supplied facts and the local inventory ledger.
//!
//! Transaction boundaries are explicit: every function here is one short SQLite
//! transaction. None is held open across a planner request or an adapter call.

use crate::bail;
use crate::model::{Classification, GoalSpec, KindCounts, Observation, Substitution};
use crate::util::{now_ms, sha256_hex, Error, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

// Action statuses (see docs/recovery.md for the state machine).
pub const A_PENDING: &str = "pending";
pub const A_DISPATCH_RECORDED: &str = "dispatch_recorded"; // claimed; in flight until the outcome is established
pub const A_ACKNOWLEDGED: &str = "acknowledged"; // adapter says applied; awaiting verification
pub const A_SUCCEEDED: &str = "succeeded"; // effect verified
pub const A_FAILED: &str = "failed"; // known: no effect
pub const A_OUTCOME_UNKNOWN: &str = "outcome_unknown";
pub const A_RETRY_PENDING: &str = "retry_pending"; // established no effect; will re-dispatch with the same key
pub const A_SATISFIED: &str = "satisfied"; // postconditions already held; not dispatched
pub const A_SUPERSEDED: &str = "superseded"; // invalidated before dispatch (goal revision / replan)
pub const A_CANCELLED: &str = "cancelled"; // invalidated before dispatch by cancel
pub const A_DENIED: &str = "denied"; // policy denied at dispatch; no effect

pub const UNSETTLED: &[&str] = &[A_DISPATCH_RECORDED, A_ACKNOWLEDGED, A_OUTCOME_UNKNOWN, A_RETRY_PENDING];

// Run statuses.
pub const R_CREATED: &str = "created";
pub const R_RUNNING: &str = "running";
pub const R_PAUSED: &str = "paused";
pub const R_AWAITING_HUMAN: &str = "awaiting_human";
pub const R_AWAITING_PLANNER: &str = "awaiting_planner";
pub const R_BLOCKED: &str = "blocked";
pub const R_COMPLETED: &str = "completed";
pub const R_PARTIAL: &str = "partial";
pub const R_STOPPED: &str = "stopped";
pub const R_FAILED: &str = "failed";

pub fn is_terminal(status: &str) -> bool {
    matches!(status, R_COMPLETED | R_PARTIAL | R_STOPPED | R_FAILED)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS run_state (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  status TEXT NOT NULL,
  status_reason TEXT,
  goal_id TEXT NOT NULL,
  current_goal_revision INTEGER NOT NULL,
  control TEXT NOT NULL DEFAULT 'run' CHECK (control IN ('run','pause','cancel')),
  active_plan_seq INTEGER,
  planner_decisions INTEGER NOT NULL DEFAULT 0,
  active_ms INTEGER NOT NULL DEFAULT 0,
  owner_pid INTEGER,
  owner_since_ms INTEGER,
  updated_at_ms INTEGER NOT NULL,
  final_report TEXT
);
CREATE TABLE IF NOT EXISTS goal_revisions (
  revision INTEGER PRIMARY KEY,
  spec TEXT NOT NULL,
  author TEXT NOT NULL,
  reason TEXT,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS plans (
  plan_seq INTEGER PRIMARY KEY,
  goal_revision INTEGER NOT NULL,
  world_revision INTEGER,
  status TEXT NOT NULL CHECK (status IN ('received','rejected','active','superseded','finished','stale')),
  proposal TEXT NOT NULL,
  planner TEXT NOT NULL,
  diagnostics TEXT NOT NULL DEFAULT '[]',
  received_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS actions (
  action_id INTEGER PRIMARY KEY,
  plan_seq INTEGER,
  origin TEXT NOT NULL CHECK (origin IN ('plan','runtime','agent')),
  step_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  goal_revision INTEGER NOT NULL,
  capability TEXT NOT NULL,
  capability_version TEXT NOT NULL,
  arguments TEXT NOT NULL,
  step_spec TEXT NOT NULL,
  resolved_arguments TEXT,
  descriptor TEXT,
  op_key TEXT UNIQUE,
  occurrence INTEGER,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  result TEXT,
  error TEXT,
  claimed_at_ms INTEGER,
  finished_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS actions_descriptor ON actions(descriptor);
CREATE TABLE IF NOT EXISTS events (
  event_id INTEGER PRIMARY KEY,
  at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  goal_revision INTEGER,
  action_id INTEGER,
  message TEXT NOT NULL,
  data TEXT
);
CREATE TABLE IF NOT EXISTS questions (
  question_id INTEGER PRIMARY KEY,
  goal_revision INTEGER NOT NULL,
  kind TEXT NOT NULL,
  prompt TEXT NOT NULL,
  options TEXT NOT NULL,
  context TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('open','answered','obsolete')),
  answer TEXT,
  asked_at_ms INTEGER NOT NULL,
  answered_at_ms INTEGER
);
CREATE TABLE IF NOT EXISTS facts (
  fact_id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  subject TEXT NOT NULL,
  value TEXT NOT NULL,
  provenance TEXT NOT NULL,
  at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS observations (
  world_revision INTEGER NOT NULL,
  at_ms INTEGER NOT NULL,
  stock_counts TEXT NOT NULL,
  withheld TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS planner_calls (
  call_id INTEGER PRIMARY KEY,
  at_ms INTEGER NOT NULL,
  arm TEXT NOT NULL,
  planner TEXT NOT NULL,
  goal_revision INTEGER NOT NULL,
  outcome TEXT NOT NULL,
  latency_ms INTEGER NOT NULL,
  transport_attempts INTEGER NOT NULL DEFAULT 1,
  input_tokens INTEGER,
  output_tokens INTEGER,
  detail TEXT
);
-- Local inventory record (adapter state, transactional).
CREATE TABLE IF NOT EXISTS inventory (
  kind TEXT PRIMARY KEY,
  on_hand INTEGER NOT NULL CHECK (on_hand >= 0),
  reserved INTEGER NOT NULL DEFAULT 0 CHECK (reserved >= 0),
  CHECK (reserved <= on_hand)
);
CREATE TABLE IF NOT EXISTS reservations (
  reservation_id TEXT PRIMARY KEY,
  counts TEXT NOT NULL,
  remaining TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('open','released')),
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS inventory_ops (
  op_key TEXT PRIMARY KEY,
  capability TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK (outcome IN ('applied','rejected')),
  category TEXT,
  result TEXT NOT NULL,
  at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ledger (
  entry_id INTEGER PRIMARY KEY,
  op_key TEXT NOT NULL,
  entry_type TEXT NOT NULL CHECK (entry_type IN ('initial','reserve','consume','release','return','adjust')),
  kind TEXT NOT NULL,
  delta_on_hand INTEGER NOT NULL,
  delta_reserved INTEGER NOT NULL,
  reservation_id TEXT,
  evidence TEXT,
  at_ms INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    /// "revolve" or "baseline"
    pub arm: String,
    /// "scripted", "fixed-plan" or "live:<provider>/<model>"
    pub planner: String,
    pub scenario_id: String,
    /// "scripted" (harness plays the human) or "interactive"
    pub human_mode: String,
    pub created_at_ms: i64,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunState {
    pub status: String,
    pub status_reason: Option<String>,
    pub goal_id: String,
    pub goal_revision: u32,
    pub control: String,
    pub active_plan_seq: Option<i64>,
    pub planner_decisions: u32,
    pub active_ms: i64,
    pub owner_pid: Option<i64>,
    pub updated_at_ms: i64,
    pub final_report: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionRow {
    pub action_id: i64,
    pub plan_seq: Option<i64>,
    pub origin: String,
    pub step_id: String,
    pub seq: i64,
    pub goal_revision: u32,
    pub capability: String,
    pub capability_version: String,
    pub arguments: Value,
    pub step_spec: Value,
    pub resolved_arguments: Option<Value>,
    pub descriptor: Option<String>,
    pub op_key: Option<String>,
    pub occurrence: Option<i64>,
    pub status: String,
    pub attempts: i64,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub claimed_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub event_id: i64,
    pub at_ms: i64,
    pub kind: String,
    pub goal_revision: Option<u32>,
    pub action_id: Option<i64>,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionRow {
    pub question_id: i64,
    pub goal_revision: u32,
    pub kind: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub context: Value,
    pub status: String,
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanRow {
    pub plan_seq: i64,
    pub goal_revision: u32,
    pub world_revision: Option<i64>,
    pub status: String,
    pub proposal: Value,
    pub planner: String,
    pub diagnostics: Value,
    pub received_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    Claimed { op_key: String, occurrence: i64 },
    /// The action's goal revision is no longer current; it was superseded.
    StaleRevision { current: u32 },
    /// A pause or cancel request is in force.
    ControlHold(String),
    /// The action is not in a claimable state.
    NotClaimable(String),
    /// Another action with the same logical effect has an unestablished outcome.
    SameEffectUnsettled { action_id: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScheme {
    /// Revolve: key = H(run, logical effect descriptor, occurrence).
    LogicalEffect,
    /// Direct-tool baseline: a fresh key per tool call (retries reuse the row's key).
    PerCall,
}

pub struct RunStore {
    pub conn: Connection,
    pub run_id: String,
    pub dir: PathBuf,
}

fn get_json(s: Option<String>) -> Option<Value> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}

impl RunStore {
    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }

    pub fn create(dir: &Path, meta: &RunMeta, goal: &GoalSpec, inventory: &KindCounts) -> Result<RunStore> {
        let path = dir.join("run.db");
        if path.exists() {
            bail!("run store {} already exists; refusing to overwrite", path.display());
        }
        goal.validate()?;
        let mut conn = Self::open_conn(&path)?;
        conn.execute_batch(SCHEMA)?;
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO meta(key, value) VALUES ('run', ?1)", params![serde_json::to_string(meta)?])?;
        tx.execute("INSERT INTO meta(key, value) VALUES ('schema', 'revolve.run-store/v1')", [])?;
        // Deterministic per scenario so handwritten plans can bind to it; unique within a run.
        let goal_id = format!("goal-{}", meta.scenario_id);
        tx.execute(
            "INSERT INTO run_state(id, status, goal_id, current_goal_revision, updated_at_ms) VALUES (1, ?1, ?2, 1, ?3)",
            params![R_CREATED, goal_id, now_ms()],
        )?;
        tx.execute(
            "INSERT INTO goal_revisions(revision, spec, author, reason, created_at_ms) VALUES (1, ?1, 'human', 'initial goal', ?2)",
            params![serde_json::to_string(goal)?, now_ms()],
        )?;
        for (k, n) in inventory {
            tx.execute("INSERT INTO inventory(kind, on_hand, reserved) VALUES (?1, ?2, 0)", params![k, *n as i64])?;
            tx.execute(
                "INSERT INTO ledger(op_key, entry_type, kind, delta_on_hand, delta_reserved, evidence, at_ms)
                 VALUES ('initial', 'initial', ?1, ?2, 0, 'inventory record at run start', ?3)",
                params![k, *n as i64, now_ms()],
            )?;
        }
        tx.execute(
            "INSERT INTO events(at_ms, kind, goal_revision, message) VALUES (?1, 'run_created', 1, ?2)",
            params![now_ms(), format!("run created: {} (arm {}, planner {})", goal.summary(), meta.arm, meta.planner)],
        )?;
        tx.commit()?;
        Ok(RunStore { conn, run_id: meta.run_id.clone(), dir: dir.to_path_buf() })
    }

    pub fn open(dir: &Path) -> Result<RunStore> {
        let path = dir.join("run.db");
        if !path.exists() {
            bail!("no run store at {} (is the run id correct?)", path.display());
        }
        let conn = Self::open_conn(&path)?;
        let meta: String = conn.query_row("SELECT value FROM meta WHERE key = 'run'", [], |r| r.get(0))?;
        let meta: RunMeta = serde_json::from_str(&meta)?;
        Ok(RunStore { conn, run_id: meta.run_id, dir: dir.to_path_buf() })
    }

    pub fn meta(&self) -> Result<RunMeta> {
        let meta: String = self.conn.query_row("SELECT value FROM meta WHERE key = 'run'", [], |r| r.get(0))?;
        Ok(serde_json::from_str(&meta)?)
    }

    pub fn state(&self) -> Result<RunState> {
        Ok(self.conn.query_row(
            "SELECT status, status_reason, goal_id, current_goal_revision, control, active_plan_seq, planner_decisions,
                    active_ms, owner_pid, updated_at_ms, final_report FROM run_state WHERE id = 1",
            [],
            |r| {
                Ok(RunState {
                    status: r.get(0)?,
                    status_reason: r.get(1)?,
                    goal_id: r.get(2)?,
                    goal_revision: r.get::<_, i64>(3)? as u32,
                    control: r.get(4)?,
                    active_plan_seq: r.get(5)?,
                    planner_decisions: r.get::<_, i64>(6)? as u32,
                    active_ms: r.get(7)?,
                    owner_pid: r.get(8)?,
                    updated_at_ms: r.get(9)?,
                    final_report: get_json(r.get(10)?),
                })
            },
        )?)
    }

    pub fn set_status(&self, status: &str, reason: Option<&str>) -> Result<()> {
        let prev = self.state()?.status;
        self.conn.execute(
            "UPDATE run_state SET status = ?1, status_reason = ?2, updated_at_ms = ?3 WHERE id = 1",
            params![status, reason, now_ms()],
        )?;
        if prev != status {
            self.event("run_status", &format!("run status {prev} -> {status}{}", reason.map(|r| format!(": {r}")).unwrap_or_default()), None, None)?;
        }
        Ok(())
    }

    pub fn set_owner(&self, pid: Option<u32>) -> Result<()> {
        self.conn.execute(
            "UPDATE run_state SET owner_pid = ?1, owner_since_ms = ?2 WHERE id = 1",
            params![pid.map(|p| p as i64), pid.map(|_| now_ms())],
        )?;
        Ok(())
    }

    pub fn add_active_ms(&self, ms: i64) -> Result<()> {
        self.conn.execute("UPDATE run_state SET active_ms = active_ms + ?1 WHERE id = 1", params![ms])?;
        Ok(())
    }

    pub fn set_final_report(&self, report: &Value) -> Result<()> {
        self.conn.execute(
            "UPDATE run_state SET final_report = ?1, updated_at_ms = ?2 WHERE id = 1",
            params![report.to_string(), now_ms()],
        )?;
        Ok(())
    }

    pub fn set_active_plan(&self, plan_seq: Option<i64>) -> Result<()> {
        self.conn.execute("UPDATE run_state SET active_plan_seq = ?1 WHERE id = 1", params![plan_seq])?;
        Ok(())
    }

    pub fn bump_planner_decisions(&self) -> Result<u32> {
        self.conn.execute("UPDATE run_state SET planner_decisions = planner_decisions + 1 WHERE id = 1", [])?;
        Ok(self.state()?.planner_decisions)
    }

    // ------------------------------------------------------------------ events

    pub fn event(&self, kind: &str, message: &str, action_id: Option<i64>, data: Option<Value>) -> Result<i64> {
        let rev: i64 = self.conn.query_row("SELECT current_goal_revision FROM run_state WHERE id = 1", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO events(at_ms, kind, goal_revision, action_id, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now_ms(), kind, rev, action_id, message, data.map(|d| d.to_string())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn events(&self, since: i64, limit: i64) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, at_ms, kind, goal_revision, action_id, message, data FROM events
             WHERE event_id > ?1 ORDER BY event_id LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![since, limit], |r| {
                Ok(EventRow {
                    event_id: r.get(0)?,
                    at_ms: r.get(1)?,
                    kind: r.get(2)?,
                    goal_revision: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                    action_id: r.get(4)?,
                    message: r.get(5)?,
                    data: get_json(r.get(6)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count_events(&self, kind: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM events WHERE kind = ?1", params![kind], |r| r.get(0))?)
    }

    // ------------------------------------------------------------------ goals & control

    pub fn goal(&self, revision: u32) -> Result<GoalSpec> {
        let s: String = self
            .conn
            .query_row("SELECT spec FROM goal_revisions WHERE revision = ?1", params![revision as i64], |r| r.get(0))
            .map_err(|_| Error::new(format!("no goal revision {revision}")))?;
        Ok(serde_json::from_str(&s)?)
    }

    pub fn current_goal(&self) -> Result<(u32, GoalSpec)> {
        let rev = self.state()?.goal_revision;
        Ok((rev, self.goal(rev)?))
    }

    pub fn goal_history(&self) -> Result<Vec<(u32, GoalSpec, String, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare("SELECT revision, spec, author, reason, created_at_ms FROM goal_revisions ORDER BY revision")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(rev, s, a, reason, t)| Ok((rev, serde_json::from_str(&s)?, a, reason, t)))
            .collect()
    }

    /// Commit a new goal revision. Committed atomically with the run's current
    /// revision pointer so that any later dispatch claim sees it (see `claim`).
    pub fn submit_goal_revision(&mut self, spec: &GoalSpec, author: &str, reason: &str) -> Result<u32> {
        spec.validate()?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cur, status): (i64, String) =
            tx.query_row("SELECT current_goal_revision, status FROM run_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if is_terminal(&status) {
            bail!("run already finished with status `{status}`; start a new run instead of correcting this one");
        }
        let next = cur + 1;
        tx.execute(
            "INSERT INTO goal_revisions(revision, spec, author, reason, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![next, serde_json::to_string(spec)?, author, reason, now_ms()],
        )?;
        tx.execute("UPDATE run_state SET current_goal_revision = ?1, updated_at_ms = ?2 WHERE id = 1", params![next, now_ms()])?;
        // Open questions against the old revision are now obsolete.
        tx.execute("UPDATE questions SET status = 'obsolete' WHERE status = 'open' AND goal_revision < ?1", params![next])?;
        tx.execute(
            "INSERT INTO events(at_ms, kind, goal_revision, message, data) VALUES (?1, 'goal_revised', ?2, ?3, ?4)",
            params![now_ms(), next, format!("goal revision {next} by {author}: {} ({reason})", spec.summary()), serde_json::to_string(spec)?],
        )?;
        tx.commit()?;
        Ok(next as u32)
    }

    pub fn request_control(&self, control: &str) -> Result<()> {
        if !matches!(control, "run" | "pause" | "cancel") {
            bail!("unknown control request `{control}`");
        }
        let st = self.state()?;
        if is_terminal(&st.status) {
            bail!("run already finished with status `{}`", st.status);
        }
        if st.control == "cancel" && control != "cancel" {
            bail!("a cancel request is already in force; it cannot be withdrawn");
        }
        self.conn.execute("UPDATE run_state SET control = ?1, updated_at_ms = ?2 WHERE id = 1", params![control, now_ms()])?;
        self.event("control_requested", &format!("control request: {control}"), None, None)?;
        Ok(())
    }

    // ------------------------------------------------------------------ plans

    pub fn insert_plan(&self, goal_revision: u32, world_revision: Option<u64>, proposal: &Value, planner: &str, status: &str, diagnostics: &Value) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO plans(goal_revision, world_revision, status, proposal, planner, diagnostics, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![goal_revision as i64, world_revision.map(|w| w as i64), status, proposal.to_string(), planner, diagnostics.to_string(), now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_plan_status(&self, plan_seq: i64, status: &str) -> Result<()> {
        self.conn.execute("UPDATE plans SET status = ?1 WHERE plan_seq = ?2", params![status, plan_seq])?;
        Ok(())
    }

    pub fn plans(&self) -> Result<Vec<PlanRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT plan_seq, goal_revision, world_revision, status, proposal, planner, diagnostics, received_at_ms FROM plans ORDER BY plan_seq",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PlanRow {
                    plan_seq: r.get(0)?,
                    goal_revision: r.get::<_, i64>(1)? as u32,
                    world_revision: r.get(2)?,
                    status: r.get(3)?,
                    proposal: get_json(r.get(4)?).unwrap_or(Value::Null),
                    planner: r.get(5)?,
                    diagnostics: get_json(r.get(6)?).unwrap_or(Value::Null),
                    received_at_ms: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------ actions

    /// Insert pending actions for a validated plan (one transaction).
    pub fn materialize(&mut self, plan_seq: Option<i64>, origin: &str, goal_revision: u32, steps: &[(String, String, String, Value, Value)]) -> Result<Vec<i64>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut ids = vec![];
        for (i, (step_id, cap, ver, args, spec)) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO actions(plan_seq, origin, step_id, seq, goal_revision, capability, capability_version, arguments, step_spec, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending')",
                params![plan_seq, origin, step_id, i as i64, goal_revision as i64, cap, ver, args.to_string(), spec.to_string()],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    fn row_to_action(r: &rusqlite::Row) -> rusqlite::Result<ActionRow> {
        Ok(ActionRow {
            action_id: r.get(0)?,
            plan_seq: r.get(1)?,
            origin: r.get(2)?,
            step_id: r.get(3)?,
            seq: r.get(4)?,
            goal_revision: r.get::<_, i64>(5)? as u32,
            capability: r.get(6)?,
            capability_version: r.get(7)?,
            arguments: get_json(r.get(8)?).unwrap_or(Value::Null),
            step_spec: get_json(r.get(9)?).unwrap_or(Value::Null),
            resolved_arguments: get_json(r.get(10)?),
            descriptor: r.get(11)?,
            op_key: r.get(12)?,
            occurrence: r.get(13)?,
            status: r.get(14)?,
            attempts: r.get(15)?,
            result: get_json(r.get(16)?),
            error: r.get(17)?,
            claimed_at_ms: r.get(18)?,
            finished_at_ms: r.get(19)?,
        })
    }

    const ACTION_COLS: &'static str = "action_id, plan_seq, origin, step_id, seq, goal_revision, capability, capability_version, arguments, step_spec,
         resolved_arguments, descriptor, op_key, occurrence, status, attempts, result, error, claimed_at_ms, finished_at_ms";

    pub fn actions(&self) -> Result<Vec<ActionRow>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {} FROM actions ORDER BY action_id", Self::ACTION_COLS))?;
        let rows = stmt.query_map([], Self::row_to_action)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn action(&self, id: i64) -> Result<ActionRow> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {} FROM actions WHERE action_id = ?1", Self::ACTION_COLS), params![id], Self::row_to_action)?)
    }

    pub fn plan_actions(&self, plan_seq: i64) -> Result<Vec<ActionRow>> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {} FROM actions WHERE plan_seq = ?1 ORDER BY seq", Self::ACTION_COLS))?;
        let rows = stmt.query_map(params![plan_seq], Self::row_to_action)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn unsettled_actions(&self) -> Result<Vec<ActionRow>> {
        Ok(self.actions()?.into_iter().filter(|a| UNSETTLED.contains(&a.status.as_str())).collect())
    }

    /// The dispatch claim. One IMMEDIATE transaction that:
    /// 1. refuses if a pause/cancel is in force;
    /// 2. supersedes the action if its goal revision is no longer current;
    /// 3. refuses if another action with the same logical effect is unsettled;
    /// 4. assigns (or reuses) the operation key and marks the action dispatch_recorded.
    ///
    /// Because a goal correction commits in its own IMMEDIATE transaction, SQLite
    /// serialises the two: a correction committed first makes this claim fail; a
    /// claim committed first is in flight and must be reconciled after the correction.
    /// `cleanup` permits local bookkeeping (e.g. releasing reservations) while a
    /// cancel is being processed; physical actions never pass `cleanup = true`.
    pub fn claim(&mut self, action_id: i64, resolved_args: &Value, descriptor: &str, scheme: KeyScheme, cleanup: bool) -> Result<ClaimResult> {
        let run_id = self.run_id.clone();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cur_rev, control): (i64, String) =
            tx.query_row("SELECT current_goal_revision, control FROM run_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if control != "run" && !(cleanup && control == "cancel") {
            return Ok(ClaimResult::ControlHold(control));
        }
        let (status, rev, existing_key, occurrence): (String, i64, Option<String>, Option<i64>) = tx.query_row(
            "SELECT status, goal_revision, op_key, occurrence FROM actions WHERE action_id = ?1",
            params![action_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if status != A_PENDING && status != A_RETRY_PENDING {
            return Ok(ClaimResult::NotClaimable(status));
        }
        if rev != cur_rev {
            tx.execute(
                "UPDATE actions SET status = 'superseded', finished_at_ms = ?1 WHERE action_id = ?2",
                params![now_ms(), action_id],
            )?;
            tx.execute(
                "INSERT INTO events(at_ms, kind, goal_revision, action_id, message) VALUES (?1, 'claim_refused_stale', ?2, ?3, ?4)",
                params![now_ms(), cur_rev, action_id, format!("action from goal revision {rev} not dispatched: current revision is {cur_rev}")],
            )?;
            tx.commit()?;
            return Ok(ClaimResult::StaleRevision { current: cur_rev as u32 });
        }
        let (op_key, occ) = if let (Some(k), Some(o)) = (existing_key, occurrence) {
            // Retry under the same identity.
            (k, o)
        } else {
            match scheme {
                KeyScheme::LogicalEffect => {
                    let mut stmt = tx.prepare(
                        "SELECT action_id, status FROM actions WHERE descriptor = ?1 AND op_key IS NOT NULL AND action_id != ?2",
                    )?;
                    let prior: Vec<(i64, String)> = stmt
                        .query_map(params![descriptor, action_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<std::result::Result<_, _>>()?;
                    drop(stmt);
                    if let Some((aid, _)) = prior.iter().find(|(_, s)| UNSETTLED.contains(&s.as_str())) {
                        return Ok(ClaimResult::SameEffectUnsettled { action_id: *aid });
                    }
                    let occ = prior.len() as i64;
                    let key = format!("op-{}", &sha256_hex(format!("{run_id}|{descriptor}|{occ}").as_bytes())[..20]);
                    (key, occ)
                }
                KeyScheme::PerCall => {
                    let key = format!("op-{}", &sha256_hex(format!("{run_id}|call|{action_id}").as_bytes())[..20]);
                    (key, 0)
                }
            }
        };
        tx.execute(
            "UPDATE actions SET status = 'dispatch_recorded', op_key = ?1, occurrence = ?2, descriptor = ?3,
                    resolved_arguments = ?4, attempts = attempts + 1, claimed_at_ms = ?5 WHERE action_id = ?6",
            params![op_key, occ, descriptor, resolved_args.to_string(), now_ms(), action_id],
        )?;
        tx.execute(
            "INSERT INTO events(at_ms, kind, goal_revision, action_id, message) VALUES (?1, 'dispatch_recorded', ?2, ?3, ?4)",
            params![now_ms(), cur_rev, action_id, format!("dispatch recorded under {op_key}")],
        )?;
        tx.commit()?;
        Ok(ClaimResult::Claimed { op_key, occurrence: occ })
    }

    pub fn set_action_status(&self, action_id: i64, status: &str, result: Option<&Value>, error: Option<&str>) -> Result<()> {
        let finished = if UNSETTLED.contains(&status) || status == A_PENDING { None } else { Some(now_ms()) };
        self.conn.execute(
            "UPDATE actions SET status = ?1, result = COALESCE(?2, result), error = ?3, finished_at_ms = ?4 WHERE action_id = ?5",
            params![status, result.map(|r| r.to_string()), error, finished, action_id],
        )?;
        Ok(())
    }

    /// Invalidate every pending action (not yet claimed). Returns how many.
    pub fn invalidate_pending(&self, new_status: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE actions SET status = ?1, finished_at_ms = ?2 WHERE status = 'pending'",
            params![new_status, now_ms()],
        )?)
    }

    // ------------------------------------------------------------------ questions & facts

    pub fn ask(&self, goal_revision: u32, kind: &str, prompt: &str, options: &[QuestionOption], context: &Value) -> Result<i64> {
        // Do not ask the same open question twice.
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT question_id FROM questions WHERE status = 'open' AND kind = ?1 AND context = ?2",
                params![kind, context.to_string()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO questions(goal_revision, kind, prompt, options, context, status, asked_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
            params![goal_revision as i64, kind, prompt, serde_json::to_string(options)?, context.to_string(), now_ms()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.event("question_asked", &format!("question #{id}: {prompt}"), None, Some(json!({"question_id": id, "kind": kind})))?;
        Ok(id)
    }

    pub fn questions(&self) -> Result<Vec<QuestionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT question_id, goal_revision, kind, prompt, options, context, status, answer FROM questions ORDER BY question_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(QuestionRow {
                    question_id: r.get(0)?,
                    goal_revision: r.get::<_, i64>(1)? as u32,
                    kind: r.get(2)?,
                    prompt: r.get(3)?,
                    options: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    context: get_json(r.get(5)?).unwrap_or(Value::Null),
                    status: r.get(6)?,
                    answer: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn open_questions(&self) -> Result<Vec<QuestionRow>> {
        Ok(self.questions()?.into_iter().filter(|q| q.status == "open").collect())
    }

    /// Answer a question. Substitution/rework decisions become a new goal revision;
    /// classifications become a human-provenance fact. Answers never change grants.
    pub fn answer(&mut self, question_id: i64, choice: &str, author: &str) -> Result<String> {
        let q = self
            .questions()?
            .into_iter()
            .find(|q| q.question_id == question_id)
            .ok_or_else(|| Error::new(format!("no question #{question_id}")))?;
        if q.status != "open" {
            bail!("question #{question_id} is {} and cannot be answered", q.status);
        }
        if !q.options.iter().any(|o| o.id == choice) {
            let ids: Vec<&str> = q.options.iter().map(|o| o.id.as_str()).collect();
            bail!("`{choice}` is not an option for question #{question_id} (options: {})", ids.join(", "));
        }
        self.conn.execute(
            "UPDATE questions SET status = 'answered', answer = ?1, answered_at_ms = ?2 WHERE question_id = ?3",
            params![choice, now_ms(), question_id],
        )?;
        self.event("question_answered", &format!("question #{question_id} answered by {author}: {choice}"), None, Some(json!({"question_id": question_id, "choice": choice})))?;
        let outcome = match q.kind.as_str() {
            "classification" => {
                let obj = q.context.get("object_id").and_then(Value::as_str).unwrap_or_default().to_string();
                self.conn.execute(
                    "INSERT INTO facts(kind, subject, value, provenance, at_ms) VALUES ('classification', ?1, ?2, ?3, ?4)",
                    params![obj, choice, format!("{author} (question #{question_id})"), now_ms()],
                )?;
                format!("recorded classification {obj} = {choice}")
            }
            "substitution" => {
                let (_, mut goal) = self.current_goal()?;
                let sub = Substitution {
                    replace: q.context.get("replace").and_then(Value::as_str).unwrap_or_default().into(),
                    with: q.context.get("with").and_then(Value::as_str).unwrap_or_default().into(),
                };
                if choice == "approve" {
                    goal.approved_substitutions.push(sub.clone());
                } else {
                    goal.declined_substitutions.push(sub.clone());
                }
                let rev = self.submit_goal_revision(&goal, author, &format!("answer to question #{question_id}: {choice} substitution {} -> {}", sub.replace, sub.with))?;
                format!("goal revision {rev}")
            }
            "rework" => {
                let (_, mut goal) = self.current_goal()?;
                goal.rework_permitted = Some(choice == "approve");
                let rev = self.submit_goal_revision(&goal, author, &format!("answer to question #{question_id}: rework {choice}"))?;
                format!("goal revision {rev}")
            }
            _ => "answer recorded".into(),
        };
        Ok(outcome)
    }

    pub fn classifications(&self) -> Result<Vec<Classification>> {
        let mut stmt = self.conn.prepare("SELECT subject, value, provenance FROM facts WHERE kind = 'classification' ORDER BY fact_id")?;
        let rows = stmt
            .query_map([], |r| Ok(Classification { object_id: r.get(0)?, kind: r.get(1)?, provenance: r.get(2)? }))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------ observations (evidence)

    pub fn record_observation(&self, obs: &Observation) -> Result<KindCounts> {
        let classes = self.classifications()?;
        let consumed = crate::inventory::consumed_evidence(self)?;
        let counts = unconsumed_counts(obs, &classes, &consumed);
        self.conn.execute(
            "INSERT INTO observations(world_revision, at_ms, stock_counts, withheld) VALUES (?1, ?2, ?3, ?4)",
            params![obs.world_revision as i64, obs.observed_at_ms, serde_json::to_string(&counts)?, serde_json::to_string(&obs.withheld)?],
        )?;
        Ok(counts)
    }

    pub fn observed_counts_at(&self, world_revision: u64) -> Result<Option<KindCounts>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT stock_counts FROM observations WHERE world_revision = ?1 ORDER BY at_ms DESC LIMIT 1",
                params![world_revision as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match s {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn count_observations(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------------ planner call accounting

    #[allow(clippy::too_many_arguments)]
    pub fn record_planner_call(&self, arm: &str, planner: &str, goal_revision: u32, outcome: &str, latency_ms: i64, transport_attempts: u32, usage: Option<(u64, u64)>, detail: Option<&Value>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO planner_calls(at_ms, arm, planner, goal_revision, outcome, latency_ms, transport_attempts, input_tokens, output_tokens, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                now_ms(), arm, planner, goal_revision as i64, outcome, latency_ms, transport_attempts as i64,
                usage.map(|u| u.0 as i64), usage.map(|u| u.1 as i64), detail.map(|d| d.to_string())
            ],
        )?;
        Ok(())
    }

    pub fn planner_call_stats(&self) -> Result<Value> {
        let (n, lat, ti, to, ta): (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), SUM(latency_ms), SUM(input_tokens), SUM(output_tokens), SUM(transport_attempts) FROM planner_calls",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let mut by_outcome = serde_json::Map::new();
        let mut stmt = self.conn.prepare("SELECT outcome, COUNT(*) FROM planner_calls GROUP BY outcome")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (o, c) = row?;
            by_outcome.insert(o, json!(c));
        }
        Ok(json!({
            "requests": n, "total_latency_ms": lat.unwrap_or(0), "transport_attempts": ta.unwrap_or(0),
            "input_tokens": ti, "output_tokens": to, "by_outcome": by_outcome
        }))
    }
}

/// Parts available for kits: units in the kind's stock bin (after human
/// classification) plus a unit in the gripper not recorded as consumed (in
/// transit from stock). Unclassified items are not counted; kinds whose bin or
/// the gripper is withheld are omitted.
pub fn available_stock(obs: &Observation, classes: &[Classification], consumed: &[String]) -> KindCounts {
    let mut counts = KindCounts::new();
    if obs.is_withheld(crate::model::GRIPPER) {
        return counts;
    }
    for k in crate::model::KNOWN_KINDS {
        let bin = crate::model::bin_for_kind(k);
        if obs.is_withheld(&bin) {
            continue;
        }
        let mut n = obs.objects_at(&bin).filter(|o| crate::model::effective_kind(o, classes) == *k).count() as u32;
        if let Some(g) = obs.gripper_contents() {
            if crate::model::effective_kind(g, classes) == *k && !consumed.iter().any(|c| c == &g.object_id) {
                n += 1;
            }
        }
        counts.insert(k.to_string(), n);
    }
    counts
}

/// What the inventory record's `on_hand` should equal under the accounting model:
/// known parts not yet recorded as consumed, wherever they are in the workspace
/// (stock bin, gripper, or placed in a tray awaiting its consumption record).
/// Returns an empty map if any location is withheld (cannot be established).
pub fn unconsumed_counts(obs: &Observation, classes: &[Classification], consumed: &[String]) -> KindCounts {
    let mut counts = KindCounts::new();
    if !obs.withheld.is_empty() {
        return counts;
    }
    for k in crate::model::KNOWN_KINDS {
        counts.insert(k.to_string(), 0);
    }
    for o in &obs.objects {
        let k = crate::model::effective_kind(o, classes);
        if !crate::model::is_known_kind(&k) || consumed.iter().any(|c| c == &o.object_id) {
            continue;
        }
        let in_place = match obs.location(&o.location_id).map(|l| l.loc_type) {
            Some(crate::model::LocationType::Bin) => o.location_id == crate::model::bin_for_kind(&k),
            Some(crate::model::LocationType::Tray) | Some(crate::model::LocationType::Gripper) => true,
            _ => false,
        };
        if in_place {
            *counts.entry(k).or_default() += 1;
        }
    }
    counts
}

// ---------------------------------------------------------------------- owner lock

/// Exclusive executor ownership of a run directory, via an OS file lock that is
/// released automatically if the process dies. Recovery takes this lock before
/// inspecting persisted state and resuming dispatch.
pub struct OwnerLock {
    _file: std::fs::File,
}

impl OwnerLock {
    pub fn acquire(run_dir: &Path) -> Result<OwnerLock> {
        let path = run_dir.join("executor.lock");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(OwnerLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("run {} is owned by another executor process (lock {} is held)", run_dir.display(), path.display())
            }
            Err(std::fs::TryLockError::Error(e)) => bail!("could not lock {}: {e}", path.display()),
        }
    }

    /// Whether some process currently owns the run (probe without keeping the lock).
    pub fn is_held(run_dir: &Path) -> bool {
        let path = run_dir.join("executor.lock");
        let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(_) => true,
        }
    }
}
