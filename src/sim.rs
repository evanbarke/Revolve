//! Deterministic discrete workspace simulator with an adapter-owned SQLite store.
//!
//! Transaction boundary (the point of this module): each effect commits the
//! world mutation, the operation-key uniqueness check, the argument fingerprint,
//! the effect receipt, the effect counter and any fault/trigger consumption in ONE
//! transaction in `world.db`. The executor records its own view in `run.db`
//! separately, so an effect can survive a crash that happens before the executor
//! records completion.
//!
//! Observations are synthesised from simulator state. They are not perception.

use crate::bail;
use crate::model::{
    GRIPPER, KNOWN_KINDS, LocationType, LocationView, ObjectView, Observation, bin_for_kind,
};
use crate::scenario::{FaultSpec, HumanAction, Scenario, Trigger};
use crate::util::{Result, SIMULATED_CRASH_EXIT_CODE, fingerprint, now_ms};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

pub const OUTSIDE: &str = "outside";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AdapterOutcome {
    /// Effect applied (or previously applied under this key: `replayed`).
    Applied { receipt: Value, replayed: bool },
    /// Request accepted and refused; no effect. Recorded under the key.
    Rejected { category: String, message: String },
    /// Caller did not learn the outcome.
    Timeout,
    /// Request not accepted (adapter degraded); no effect.
    Unavailable { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StatusResult {
    Applied { receipt: Value },
    Rejected { category: String, message: String },
    NotFound,
    Unavailable { message: String },
}

/// A harness-owned human action whose trigger fired during an effect.
#[derive(Debug, Clone)]
pub struct FiredHumanAction {
    pub fault_id: i64,
    pub action: HumanAction,
}

pub struct SimWorld {
    conn: Connection,
    /// The slot a place command is heading for (set while the arm moves there).
    pending_place: Option<motion::Placement>,
}

mod motion;
pub use motion::Placement;

/// Ground truth for the evaluator and the UI's "simulator truth" panel.
#[derive(Debug, Clone, Serialize)]
pub struct TruthObject {
    pub object_id: String,
    pub true_kind: String,
    pub observed_kind: String,
    pub location_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub op_key: String,
    pub capability: String,
    pub outcome: String,
    pub detail: Value,
    pub world_revision: i64,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaultRow {
    pub fault_id: i64,
    pub spec: Value,
    pub state: String,
    pub fired_at_ms: Option<i64>,
    pub fired_op_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Truth {
    pub world_revision: i64,
    pub locations: Vec<LocationView>,
    pub objects: Vec<TruthObject>,
    pub receipts: Vec<Receipt>,
    pub faults: Vec<FaultRow>,
    pub initial_object_count: i64,
    pub blackout: bool,
    pub planner_offline: bool,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS world_meta (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  revision INTEGER NOT NULL,
  seed INTEGER NOT NULL,
  scenario_id TEXT NOT NULL,
  initial_object_count INTEGER NOT NULL,
  blackout INTEGER NOT NULL DEFAULT 0,
  blackout_withhold TEXT NOT NULL DEFAULT '[]',
  planner_offline INTEGER NOT NULL DEFAULT 0,
  speed REAL NOT NULL DEFAULT 0,
  arm TEXT
);
CREATE TABLE IF NOT EXISTS locations (
  location_id TEXT PRIMARY KEY,
  loc_type TEXT NOT NULL,
  accepts_kind TEXT,
  available INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS objects (
  object_id TEXT PRIMARY KEY,
  true_kind TEXT NOT NULL,
  observed_kind TEXT NOT NULL,
  location_id TEXT NOT NULL REFERENCES locations(location_id),
  x REAL,
  y REAL,
  z REAL,
  slot INTEGER,
  -- How the part is turned (a quaternion); NULL = not turned.
  qx REAL,
  qy REAL,
  qz REAL,
  qw REAL
);
-- The gripper holds at most one object (single location per object is structural).
CREATE UNIQUE INDEX IF NOT EXISTS gripper_single ON objects(location_id) WHERE location_id = 'gripper';
CREATE TABLE IF NOT EXISTS receipts (
  op_key TEXT PRIMARY KEY,
  capability TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK (outcome IN ('applied','rejected')),
  category TEXT,
  detail TEXT NOT NULL,
  world_revision INTEGER NOT NULL,
  at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS effect_counters (
  capability TEXT PRIMARY KEY,
  applied INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS faults (
  fault_id INTEGER PRIMARY KEY,
  spec TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('armed','fired','consumed','cleared')),
  fired_at_ms INTEGER,
  fired_op_key TEXT
);
CREATE TABLE IF NOT EXISTS harness_log (
  id INTEGER PRIMARY KEY,
  at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  detail TEXT NOT NULL
);
"#;

fn prefix_for(kind: &str) -> &str {
    match kind {
        "red_block" => "red",
        "blue_block" => "blue",
        "green_block" => "green",
        "washer" => "washer",
        other => other,
    }
}

fn loc_type_str(t: LocationType) -> &'static str {
    match t {
        LocationType::Bin => "bin",
        LocationType::Tray => "tray",
        LocationType::Gripper => "gripper",
        LocationType::Outside => "outside",
        LocationType::Table => "table",
    }
}

fn parse_loc_type(s: &str) -> LocationType {
    match s {
        "bin" => LocationType::Bin,
        "tray" => LocationType::Tray,
        "gripper" => LocationType::Gripper,
        "table" => LocationType::Table,
        _ => LocationType::Outside,
    }
}

impl SimWorld {
    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(conn)
    }

    /// Create a fresh world from a scenario. Refuses to overwrite an existing store.
    pub fn create(path: &Path, sc: &Scenario) -> Result<SimWorld> {
        if path.exists() {
            bail!(
                "world store {} already exists; refusing to overwrite",
                path.display()
            );
        }
        let mut conn = Self::open_conn(path)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(motion::MOTION_SCHEMA)?;
        let tx = conn.transaction()?;
        let mut locs: Vec<(String, LocationType, Option<String>)> =
            vec![(GRIPPER.into(), LocationType::Gripper, None)];
        for k in KNOWN_KINDS {
            locs.push((bin_for_kind(k), LocationType::Bin, Some(k.to_string())));
        }
        for t in 1..=sc.world.trays {
            locs.push((format!("tray-{t}"), LocationType::Tray, None));
        }
        let has_table = !sc.world.table.is_empty()
            || sc
                .world
                .unknown_items
                .iter()
                .any(|u| u.bin == crate::model::TABLE);
        if has_table {
            locs.push((crate::model::TABLE.into(), LocationType::Table, None));
        }
        locs.push((OUTSIDE.into(), LocationType::Outside, None));
        for (id, t, acc) in &locs {
            tx.execute(
                "INSERT INTO locations(location_id, loc_type, accepts_kind, available) VALUES (?1, ?2, ?3, 1)",
                params![id, loc_type_str(*t), acc],
            )?;
        }
        let mut count = 0i64;
        for (kind, n) in &sc.world.stock {
            if !KNOWN_KINDS.contains(&kind.as_str()) {
                bail!("scenario stock: unknown kind `{kind}`");
            }
            for i in 1..=*n {
                tx.execute(
                    "INSERT INTO objects(object_id, true_kind, observed_kind, location_id) VALUES (?1, ?2, ?2, ?3)",
                    params![format!("{}-{:02}", prefix_for(kind), i), kind, bin_for_kind(kind)],
                )?;
                count += 1;
            }
        }
        // Parts on the table are numbered after the stock of the same kind.
        for (kind, n) in &sc.world.table {
            if !KNOWN_KINDS.contains(&kind.as_str()) {
                bail!("scenario table: unknown kind `{kind}`");
            }
            let start = sc.world.stock.get(kind).copied().unwrap_or(0);
            for i in 1..=*n {
                tx.execute(
                    "INSERT INTO objects(object_id, true_kind, observed_kind, location_id) VALUES (?1, ?2, ?2, ?3)",
                    params![format!("{}-{:02}", prefix_for(kind), start + i), kind, crate::model::TABLE],
                )?;
                count += 1;
            }
        }
        for (i, u) in sc.world.unknown_items.iter().enumerate() {
            tx.execute(
                "INSERT INTO objects(object_id, true_kind, observed_kind, location_id) VALUES (?1, ?2, 'unknown', ?3)",
                params![format!("item-{:02}", i + 1), u.true_kind, u.bin],
            )?;
            count += 1;
        }
        tx.execute(
            "INSERT INTO world_meta(id, revision, seed, scenario_id, initial_object_count) VALUES (1, 0, ?1, ?2, ?3)",
            params![sc.seed as i64, sc.id, count],
        )?;
        for f in &sc.faults {
            tx.execute(
                "INSERT INTO faults(spec, state) VALUES (?1, 'armed')",
                params![serde_json::to_string(&json!({"fault": f}))?],
            )?;
        }
        for a in &sc.human.actions {
            tx.execute(
                "INSERT INTO faults(spec, state) VALUES (?1, 'armed')",
                params![serde_json::to_string(&json!({"human": a}))?],
            )?;
        }
        for c in ["workspace.pick", "workspace.place"] {
            tx.execute(
                "INSERT INTO effect_counters(capability, applied) VALUES (?1, 0)",
                params![c],
            )?;
        }
        for b in &sc.world.barriers {
            tx.execute(
                "INSERT INTO barriers(barrier_id, label, x, y, w, d, h, yaw) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![b.id, b.label, b.x as f64 / 1000.0, b.y as f64 / 1000.0, b.w as f64 / 1000.0, b.d as f64 / 1000.0, b.h as f64 / 1000.0, (b.yaw as f64).to_radians()],
            )?;
        }
        motion::assign_slots(&tx)?;
        motion::settle_new_table(&tx)?;
        tx.execute(
            "UPDATE world_meta SET arm = ?1 WHERE id = 1",
            params![serde_json::to_string(&crate::cell::home())?],
        )?;
        tx.commit()?;
        Ok(SimWorld {
            conn,
            pending_place: None,
        })
    }

    /// Copy this world as a consistent snapshot for a rehearsal (a digital twin):
    /// the same table, with no future faults, scripted human actions or blackouts.
    pub fn snapshot_twin(&self, path: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        let c = Connection::open(path)?;
        c.execute_batch(
            "DELETE FROM faults; UPDATE world_meta SET blackout = 0, blackout_withhold = '[]', planner_offline = 0, speed = 0 WHERE id = 1;",
        )?;
        Ok(())
    }

    pub fn open(path: &Path) -> Result<SimWorld> {
        if !path.exists() {
            bail!("world store {} does not exist", path.display());
        }
        let mut conn = Self::open_conn(path)?;
        motion::migrate(&mut conn)?;
        Ok(SimWorld {
            conn,
            pending_place: None,
        })
    }

    fn revision(tx: &Transaction) -> Result<i64> {
        Ok(
            tx.query_row("SELECT revision FROM world_meta WHERE id = 1", [], |r| {
                r.get(0)
            })?,
        )
    }

    fn bump_revision(tx: &Transaction) -> Result<i64> {
        tx.execute(
            "UPDATE world_meta SET revision = revision + 1 WHERE id = 1",
            [],
        )?;
        Self::revision(tx)
    }

    fn blackout(conn: &Connection) -> Result<(bool, Vec<String>)> {
        let (b, w): (i64, String) = conn.query_row(
            "SELECT blackout, blackout_withhold FROM world_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((b != 0, serde_json::from_str(&w).unwrap_or_default()))
    }

    fn log(tx: &Transaction, kind: &str, detail: &Value) -> Result<()> {
        tx.execute(
            "INSERT INTO harness_log(at_ms, kind, detail) VALUES (?1, ?2, ?3)",
            params![now_ms(), kind, detail.to_string()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- observe

    pub fn observe(&mut self) -> std::result::Result<Observation, String> {
        self.observe_inner().map_err(|e| e.msg)
    }

    fn observe_inner(&mut self) -> Result<Observation> {
        let (blackout, withhold) = Self::blackout(&self.conn)?;
        let rev: i64 =
            self.conn
                .query_row("SELECT revision FROM world_meta WHERE id = 1", [], |r| {
                    r.get(0)
                })?;
        let mut stmt = self
            .conn
            .prepare("SELECT location_id, loc_type, accepts_kind, available FROM locations WHERE loc_type != 'outside' ORDER BY location_id")?;
        let locations: Vec<LocationView> = stmt
            .query_map([], |r| {
                Ok(LocationView {
                    location_id: r.get(0)?,
                    loc_type: parse_loc_type(&r.get::<_, String>(1)?),
                    accepts_kind: r.get(2)?,
                    available: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let withheld: Vec<String> = if blackout { withhold } else { vec![] };
        let mut stmt = self.conn.prepare(
            "SELECT object_id, observed_kind, location_id FROM objects WHERE location_id != 'outside' ORDER BY object_id",
        )?;
        let objects: Vec<ObjectView> = stmt
            .query_map([], |r| {
                Ok(ObjectView {
                    object_id: r.get(0)?,
                    observed_kind: r.get(1)?,
                    location_id: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|o| !withheld.contains(&o.location_id))
            .collect();
        Ok(Observation {
            world_revision: rev as u64,
            observed_at_ms: now_ms(),
            source: "simulator-synthetic (derived from simulator state; not perception)".into(),
            locations,
            objects,
            withheld,
        })
    }

    // ---------------------------------------------------------------- status

    pub fn status(&mut self, op_key: &str) -> StatusResult {
        match self.status_inner(op_key) {
            Ok(s) => s,
            Err(e) => StatusResult::Unavailable { message: e.msg },
        }
    }

    fn status_inner(&mut self, op_key: &str) -> Result<StatusResult> {
        let (blackout, _) = Self::blackout(&self.conn)?;
        if blackout {
            return Ok(StatusResult::Unavailable {
                message: "operation status service not responding".into(),
            });
        }
        let row: Option<(String, Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT outcome, category, detail FROM receipts WHERE op_key = ?1",
                params![op_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            None => StatusResult::NotFound,
            Some((o, _, d)) if o == "applied" => StatusResult::Applied {
                receipt: serde_json::from_str(&d)?,
            },
            Some((_, c, d)) => StatusResult::Rejected {
                category: c.unwrap_or_default(),
                message: serde_json::from_str::<Value>(&d)?
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
        })
    }

    // ---------------------------------------------------------------- effects

    fn effect(
        &mut self,
        op_key: &str,
        capability: &str,
        args: &Value,
    ) -> (AdapterOutcome, Vec<FiredHumanAction>) {
        match self.effect_inner(op_key, capability, args) {
            Ok(r) => r,
            // A local storage error is treated as "not accepted": the transaction rolled back.
            Err(e) => (
                AdapterOutcome::Unavailable {
                    message: format!("adapter store error: {}", e.msg),
                },
                vec![],
            ),
        }
    }

    fn effect_inner(
        &mut self,
        op_key: &str,
        capability: &str,
        args: &Value,
    ) -> Result<(AdapterOutcome, Vec<FiredHumanAction>)> {
        let fp = fingerprint(&json!({"capability": capability, "args": args}));
        let pending = self.pending_place.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let (blackout, _) = Self::blackout(&tx)?;
        if blackout {
            return Ok((
                AdapterOutcome::Unavailable {
                    message: "workspace controller not accepting commands".into(),
                },
                vec![],
            ));
        }

        // Idempotency: an existing receipt under this key is returned, never re-applied.
        let existing: Option<(String, String, Option<String>, String)> = tx
            .query_row(
                "SELECT fingerprint, outcome, category, detail FROM receipts WHERE op_key = ?1",
                params![op_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((efp, outcome, cat, detail)) = existing {
            if efp != fp {
                return Ok((
                    AdapterOutcome::Rejected {
                        category: "key_conflict".into(),
                        message: format!(
                            "operation key {op_key} was already used with different arguments; no effect"
                        ),
                    },
                    vec![],
                ));
            }
            let detail: Value = serde_json::from_str(&detail)?;
            return Ok((
                if outcome == "applied" {
                    AdapterOutcome::Applied {
                        receipt: detail,
                        replayed: true,
                    }
                } else {
                    AdapterOutcome::Rejected {
                        category: cat.unwrap_or_default(),
                        message: detail
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    }
                },
                vec![],
            ));
        }

        let applied_so_far: i64 = tx.query_row(
            "SELECT applied FROM effect_counters WHERE capability = ?1",
            params![capability],
            |r| r.get(0),
        )?;
        let nth_next = (applied_so_far + 1) as u32;

        // Faults that fire *before* the effect.
        let armed = Self::armed(&tx)?;
        for (fid, spec) in &armed {
            let Some(FaultOrHuman::Fault(f)) = spec else {
                continue;
            };
            match f {
                FaultSpec::CrashBeforeEffect { on } if Self::matches(on, capability, nth_next) => {
                    Self::fire(&tx, *fid, op_key)?;
                    Self::log(
                        &tx,
                        "fault_fired",
                        &json!({"fault_id": fid, "kind": "crash_before_effect", "op_key": op_key}),
                    )?;
                    tx.commit()?;
                    eprintln!(
                        "[sim] injected crash before effect of {op_key} ({capability} #{nth_next}); exiting"
                    );
                    std::process::exit(SIMULATED_CRASH_EXIT_CODE);
                }
                FaultSpec::TimeoutBeforeEffect { on, blackout }
                    if Self::matches(on, capability, nth_next) =>
                {
                    Self::fire(&tx, *fid, op_key)?;
                    if *blackout {
                        Self::set_blackout_tx(&tx, true, &Self::default_withhold(&tx, args)?)?;
                    }
                    Self::log(
                        &tx,
                        "fault_fired",
                        &json!({"fault_id": fid, "kind": "timeout_before_effect", "op_key": op_key}),
                    )?;
                    tx.commit()?;
                    return Ok((AdapterOutcome::Timeout, vec![]));
                }
                _ => {}
            }
        }

        // Preconditions are enforced by the adapter itself, independently of the planner.
        if let Err((cat, msg)) = Self::check_preconditions(&tx, capability, args)? {
            let rev = Self::revision(&tx)?;
            tx.execute(
                "INSERT INTO receipts(op_key, capability, fingerprint, outcome, category, detail, world_revision, at_ms)
                 VALUES (?1, ?2, ?3, 'rejected', ?4, ?5, ?6, ?7)",
                params![op_key, capability, fp, cat, json!({"message": msg, "args": args}).to_string(), rev, now_ms()],
            )?;
            tx.commit()?;
            return Ok((
                AdapterOutcome::Rejected {
                    category: cat,
                    message: msg,
                },
                vec![],
            ));
        }

        // Apply the effect.
        let object_id = args["object_id"].as_str().unwrap_or_default();
        let (from, to) = match capability {
            "workspace.pick" => (
                args["source_id"].as_str().unwrap_or_default().to_string(),
                GRIPPER.to_string(),
            ),
            _ => (
                GRIPPER.to_string(),
                args["destination_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
        };
        tx.execute(
            "UPDATE objects SET location_id = ?1 WHERE object_id = ?2",
            params![to, object_id],
        )?;
        let rests = motion::settle_position(&tx, pending.as_ref(), capability, object_id, &to)?;
        let rev = Self::bump_revision(&tx)?;
        tx.execute(
            "UPDATE effect_counters SET applied = applied + 1 WHERE capability = ?1",
            params![capability],
        )?;
        let mut receipt = json!({
            "op_key": op_key, "capability": capability, "object_id": object_id,
            "from": from, "to": to, "world_revision": rev
        });
        // The release worked, but the part came to rest somewhere else.
        if let Some(r) = rests.filter(|r| *r != to) {
            receipt["rests_at"] = json!(r);
        }
        tx.execute(
            "INSERT INTO receipts(op_key, capability, fingerprint, outcome, category, detail, world_revision, at_ms)
             VALUES (?1, ?2, ?3, 'applied', NULL, ?4, ?5, ?6)",
            params![op_key, capability, fp, receipt.to_string(), rev, now_ms()],
        )?;

        // Triggers that fire *after* the effect, in the same transaction.
        let mut crash = false;
        let mut timeout = false;
        let mut human = vec![];
        for (fid, spec) in &armed {
            match spec {
                Some(FaultOrHuman::Fault(f)) => {
                    let Some(on) = f.trigger() else { continue };
                    if !Self::matches(on, capability, nth_next) {
                        continue;
                    }
                    match f {
                        FaultSpec::CrashAfterEffect { .. } => crash = true,
                        FaultSpec::TimeoutAfterEffect { blackout, .. } => {
                            timeout = true;
                            if *blackout {
                                Self::set_blackout_tx(
                                    &tx,
                                    true,
                                    &Self::default_withhold(&tx, args)?,
                                )?;
                            }
                        }
                        FaultSpec::DestinationUnavailable { location_id, .. } => {
                            tx.execute(
                                "UPDATE locations SET available = 0 WHERE location_id = ?1",
                                params![location_id],
                            )?;
                            Self::bump_revision(&tx)?;
                        }
                        FaultSpec::RemoveObject { object_id, .. } => {
                            tx.execute(
                                "UPDATE objects SET location_id = 'outside', slot = NULL, x = NULL, y = NULL, z = NULL WHERE object_id = ?1 AND location_id != 'gripper'",
                                params![object_id],
                            )?;
                            Self::bump_revision(&tx)?;
                        }
                        FaultSpec::PlannerUnavailable { .. } => {
                            tx.execute(
                                "UPDATE world_meta SET planner_offline = 1 WHERE id = 1",
                                [],
                            )?;
                        }
                        FaultSpec::InjectUnauthorizedAction { .. } => {
                            // Becomes due for the planner wrapper; consumed there.
                            tx.execute("UPDATE faults SET state = 'fired', fired_at_ms = ?1 WHERE fault_id = ?2", params![now_ms(), fid])?;
                            continue;
                        }
                        _ => continue,
                    }
                    Self::fire(&tx, *fid, op_key)?;
                    Self::log(
                        &tx,
                        "fault_fired",
                        &json!({"fault_id": fid, "fault": f, "op_key": op_key}),
                    )?;
                }
                Some(FaultOrHuman::Human(a)) => {
                    if Self::matches(a.trigger(), capability, nth_next) {
                        Self::fire(&tx, *fid, op_key)?;
                        Self::log(
                            &tx,
                            "human_action_due",
                            &json!({"fault_id": fid, "action": a, "op_key": op_key}),
                        )?;
                        human.push(FiredHumanAction {
                            fault_id: *fid,
                            action: a.clone(),
                        });
                    }
                }
                None => {}
            }
        }
        tx.commit()?;
        if crash {
            eprintln!(
                "[sim] injected crash after effect of {op_key} ({capability} #{nth_next}) committed; exiting before the executor records it"
            );
            std::process::exit(SIMULATED_CRASH_EXIT_CODE);
        }
        if timeout {
            return Ok((AdapterOutcome::Timeout, human));
        }
        Ok((
            AdapterOutcome::Applied {
                receipt,
                replayed: false,
            },
            human,
        ))
    }

    fn matches(on: &Trigger, capability: &str, nth: u32) -> bool {
        on.capability == capability && on.nth == nth
    }

    fn armed(tx: &Transaction) -> Result<Vec<(i64, Option<FaultOrHuman>)>> {
        let mut stmt = tx
            .prepare("SELECT fault_id, spec FROM faults WHERE state = 'armed' ORDER BY fault_id")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, s)| (id, FaultOrHuman::parse(&s)))
            .collect())
    }

    fn fire(tx: &Transaction, fid: i64, op_key: &str) -> Result<()> {
        tx.execute(
            "UPDATE faults SET state = 'fired', fired_at_ms = ?1, fired_op_key = ?2 WHERE fault_id = ?3",
            params![now_ms(), op_key, fid],
        )?;
        Ok(())
    }

    fn default_withhold(tx: &Transaction, args: &Value) -> Result<Vec<String>> {
        let mut w = vec![GRIPPER.to_string()];
        for k in ["source_id", "destination_id"] {
            if let Some(s) = args.get(k).and_then(Value::as_str) {
                w.push(s.to_string());
            }
        }
        // Also withhold wherever the object currently is, so observation cannot settle it.
        if let Some(obj) = args.get("object_id").and_then(Value::as_str)
            && let Some(loc) = tx
                .query_row(
                    "SELECT location_id FROM objects WHERE object_id = ?1",
                    params![obj],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            && !w.contains(&loc)
        {
            w.push(loc);
        }
        Ok(w)
    }

    fn set_blackout_tx(tx: &Transaction, on: bool, withhold: &[String]) -> Result<()> {
        tx.execute(
            "UPDATE world_meta SET blackout = ?1, blackout_withhold = ?2 WHERE id = 1",
            params![on as i64, serde_json::to_string(withhold)?],
        )?;
        Ok(())
    }

    fn check_preconditions(
        tx: &Transaction,
        capability: &str,
        args: &Value,
    ) -> Result<std::result::Result<(), (String, String)>> {
        let pf = |m: String| Ok(Err(("precondition_failed".to_string(), m)));
        let object_id = args
            .get("object_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let obj: Option<(String, String)> = tx
            .query_row(
                "SELECT location_id, true_kind FROM objects WHERE object_id = ?1",
                params![object_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((obj_loc, true_kind)) = obj else {
            return pf(format!("object {object_id} is not in the workspace"));
        };
        let loc_info = |id: &str| -> Result<Option<(String, Option<String>, bool)>> {
            Ok(tx
                .query_row("SELECT loc_type, accepts_kind, available FROM locations WHERE location_id = ?1", params![id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0))
                })
                .optional()?)
        };
        let held: Option<String> = tx
            .query_row(
                "SELECT object_id FROM objects WHERE location_id = 'gripper'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match capability {
            "workspace.pick" => {
                let src = args
                    .get("source_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match loc_info(src)? {
                    None => return pf(format!("unknown location {src}")),
                    Some((t, _, _)) if t == "outside" || t == "gripper" => {
                        return pf(format!("{src} is not a pick source"));
                    }
                    Some((_, _, false)) => return pf(format!("{src} is unavailable")),
                    _ => {}
                }
                if let Some(h) = held {
                    return pf(format!("gripper is not empty (holds {h})"));
                }
                if obj_loc != src {
                    return pf(format!("object {object_id} is not at {src}"));
                }
            }
            "workspace.place" => {
                let dst = args
                    .get("destination_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if held.as_deref() != Some(object_id) {
                    return pf(format!("gripper does not hold {object_id}"));
                }
                match loc_info(dst)? {
                    None => return pf(format!("unknown location {dst}")),
                    Some((t, _, _)) if t == "outside" || t == "gripper" => {
                        return pf(format!("{dst} is not a destination"));
                    }
                    Some((_, _, false)) => return pf(format!("destination {dst} is unavailable")),
                    Some((t, Some(acc), _)) if t == "bin" && acc != true_kind => {
                        return pf(format!("bin {dst} only accepts {acc}"));
                    }
                    _ => {}
                }
            }
            other => {
                return Ok(Err((
                    "malformed".into(),
                    format!("simulator does not implement {other}"),
                )));
            }
        }
        Ok(Ok(()))
    }

    // ---------------------------------------------------------------- harness / simulation controls

    pub fn set_blackout(&mut self, on: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let withhold = if on {
            vec![GRIPPER.to_string()]
        } else {
            vec![]
        };
        Self::set_blackout_tx(&tx, on, &withhold)?;
        Self::log(
            &tx,
            if on {
                "blackout_on"
            } else {
                "evidence_restored"
            },
            &json!({}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_location_available(&mut self, location_id: &str, available: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE locations SET available = ?1 WHERE location_id = ?2",
            params![available as i64, location_id],
        )?;
        if n == 0 {
            bail!("unknown location {location_id}");
        }
        Self::bump_revision(&tx)?;
        Self::log(
            &tx,
            "location_availability",
            &json!({"location_id": location_id, "available": available}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_planner_offline(&mut self, offline: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE world_meta SET planner_offline = ?1 WHERE id = 1",
            params![offline as i64],
        )?;
        Self::log(
            &tx,
            if offline {
                "planner_offline"
            } else {
                "planner_restored"
            },
            &json!({}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn planner_offline(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT planner_offline FROM world_meta WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }

    /// Arm an additional fault at runtime (UI simulation controls).
    pub fn arm_fault(&mut self, f: &FaultSpec) -> Result<i64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Start-of-run planner faults with no trigger apply immediately.
        if let FaultSpec::PlannerUnavailable { on: None } = f {
            tx.execute("UPDATE world_meta SET planner_offline = 1 WHERE id = 1", [])?;
        }
        tx.execute(
            "INSERT INTO faults(spec, state) VALUES (?1, 'armed')",
            params![json!({"fault": f}).to_string()],
        )?;
        let id = tx.last_insert_rowid();
        Self::log(&tx, "fault_armed", &json!({"fault_id": id, "fault": f}))?;
        tx.commit()?;
        Ok(id)
    }

    /// Apply scenario faults that have no trigger (e.g. planner offline from the start).
    pub fn apply_untriggered(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (fid, spec) in Self::armed(&tx)? {
            if let Some(FaultOrHuman::Fault(FaultSpec::PlannerUnavailable { on: None })) = spec {
                tx.execute("UPDATE world_meta SET planner_offline = 1 WHERE id = 1", [])?;
                Self::fire(&tx, fid, "")?;
            }
            if let Some(FaultOrHuman::Fault(FaultSpec::InjectUnauthorizedAction { on: None })) =
                spec
            {
                tx.execute(
                    "UPDATE faults SET state = 'fired', fired_at_ms = ?1 WHERE fault_id = ?2",
                    params![now_ms(), fid],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Human actions whose trigger fired but whose delivery was not recorded
    /// (e.g. the executor crashed right after the effect that fired them).
    pub fn undelivered_human_actions(&self) -> Result<Vec<FiredHumanAction>> {
        let mut stmt = self
            .conn
            .prepare("SELECT fault_id, spec FROM faults WHERE state = 'fired' ORDER BY fault_id")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, s)| match FaultOrHuman::parse(&s) {
                Some(FaultOrHuman::Human(action)) => Some(FiredHumanAction {
                    fault_id: id,
                    action,
                }),
                _ => None,
            })
            .collect())
    }

    /// Record that fired human actions were delivered to the harness hook.
    pub fn mark_delivered(&mut self, ids: &[i64]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            tx.execute(
                "UPDATE faults SET state = 'consumed' WHERE fault_id = ?1 AND state = 'fired'",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One-shot consumption of a due "inject unauthorized action" fault by the planner wrapper.
    pub fn take_due_injection(&mut self) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut due = None;
        {
            let mut stmt = tx.prepare(
                "SELECT fault_id, spec FROM faults WHERE state = 'fired' ORDER BY fault_id",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (id, s) in rows {
                if let Some(FaultOrHuman::Fault(FaultSpec::InjectUnauthorizedAction { .. })) =
                    FaultOrHuman::parse(&s)
                {
                    due = Some(id);
                    break;
                }
            }
        }
        if let Some(id) = due {
            tx.execute(
                "UPDATE faults SET state = 'consumed' WHERE fault_id = ?1",
                params![id],
            )?;
            Self::log(
                &tx,
                "fault_consumed",
                &json!({"fault_id": id, "kind": "inject_unauthorized_action"}),
            )?;
        }
        tx.commit()?;
        Ok(due.is_some())
    }

    pub fn log_harness(&mut self, kind: &str, detail: &Value) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::log(&tx, kind, detail)?;
        tx.commit()?;
        Ok(())
    }

    pub fn harness_log(&self) -> Result<Vec<(i64, String, Value)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT at_ms, kind, detail FROM harness_log ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(t, k, d)| (t, k, serde_json::from_str(&d).unwrap_or(Value::Null)))
            .collect())
    }

    /// Locations currently withheld from observers (empty unless an evidence blackout is active).
    pub fn observe_withheld(&self) -> Vec<String> {
        match Self::blackout(&self.conn) {
            Ok((true, w)) => w,
            _ => vec![],
        }
    }

    /// Applied-effect counters per capability (the basis of event-triggered faults).
    pub fn effect_counts(&self) -> Result<std::collections::BTreeMap<String, i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT capability, applied FROM effect_counters")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<std::collections::BTreeMap<_, _>, _>>()?;
        Ok(rows)
    }

    /// Authoritative truth. For the evaluator and the UI's labelled truth panel only.
    pub fn truth(&self) -> Result<Truth> {
        let (rev, init, blackout, poff): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT revision, initial_object_count, blackout, planner_offline FROM world_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut stmt = self.conn.prepare("SELECT location_id, loc_type, accepts_kind, available FROM locations ORDER BY location_id")?;
        let locations = stmt
            .query_map([], |r| {
                Ok(LocationView {
                    location_id: r.get(0)?,
                    loc_type: parse_loc_type(&r.get::<_, String>(1)?),
                    accepts_kind: r.get(2)?,
                    available: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut stmt =
            self.conn.prepare("SELECT object_id, true_kind, observed_kind, location_id FROM objects ORDER BY object_id")?;
        let objects = stmt
            .query_map([], |r| {
                Ok(TruthObject {
                    object_id: r.get(0)?,
                    true_kind: r.get(1)?,
                    observed_kind: r.get(2)?,
                    location_id: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut stmt = self
            .conn
            .prepare("SELECT op_key, capability, outcome, detail, world_revision, at_ms FROM receipts ORDER BY at_ms, rowid")?;
        let receipts = stmt
            .query_map([], |r| {
                Ok(Receipt {
                    op_key: r.get(0)?,
                    capability: r.get(1)?,
                    outcome: r.get(2)?,
                    detail: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                    world_revision: r.get(4)?,
                    at_ms: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut stmt = self.conn.prepare(
            "SELECT fault_id, spec, state, fired_at_ms, fired_op_key FROM faults ORDER BY fault_id",
        )?;
        let faults = stmt
            .query_map([], |r| {
                Ok(FaultRow {
                    fault_id: r.get(0)?,
                    spec: serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or(Value::Null),
                    state: r.get(2)?,
                    fired_at_ms: r.get(3)?,
                    fired_op_key: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Truth {
            world_revision: rev,
            locations,
            objects,
            receipts,
            faults,
            initial_object_count: init,
            blackout: blackout != 0,
            planner_offline: poff != 0,
        })
    }
}

enum FaultOrHuman {
    Fault(FaultSpec),
    Human(HumanAction),
}

impl FaultOrHuman {
    fn parse(s: &str) -> Option<FaultOrHuman> {
        let v: Value = serde_json::from_str(s).ok()?;
        if let Some(f) = v.get("fault") {
            return serde_json::from_value(f.clone())
                .ok()
                .map(FaultOrHuman::Fault);
        }
        if let Some(h) = v.get("human") {
            return serde_json::from_value(h.clone())
                .ok()
                .map(FaultOrHuman::Human);
        }
        None
    }
}
