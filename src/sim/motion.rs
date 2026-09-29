//! The simulated robot controller: every pick and place is a real motion.
//!
//! A pick runs: open (if needed), approach and descend to the part, close the
//! gripper, then the grip is committed as the effect (unchanged: one world.db
//! transaction with the receipt and fault logic), then lift. A place runs:
//! carry to a free slot, descend, then the release is committed as the effect,
//! then open and lift. Replayed receipts and blackouts do not move the arm.
//! Destinations that are out of reach are refused before anything moves.
//!
//! Each segment is logged in `motions` with its wall-clock start and playback
//! duration (the nominal 1x duration divided by the world's speed; speed 0 means
//! instant, which tests and benchmarks use). The call blocks for the motion, as
//! a controller API would. A controller finishes a commanded segment even if the
//! software that commanded it dies, so the arm's pose is the end of the last
//! logged segment. Viewers animate the logged segments against the clock.

use super::*;
use crate::cell::{self, Arm, P3, Segment};

/// An object's kind, location and resting position (x, y, z).
type ObjectRow = (String, String, Option<f64>, Option<f64>, Option<f64>);

/// A slot chosen for a place before the arm moves there.
#[derive(Debug, Clone)]
pub struct Placement {
    pub object_id: String,
    pub destination: String,
    pub slot: i64,
    pub pos: P3,
}

pub(super) const MOTION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS motions (
  seq INTEGER PRIMARY KEY,
  at_ms INTEGER NOT NULL,
  play_ms INTEGER NOT NULL,
  nominal_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  from_arm TEXT NOT NULL,
  to_arm TEXT NOT NULL,
  carrying TEXT
);
"#;

fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(names.iter().any(|n| n == col))
}

/// Bring an older world up to date (positions, arm, motion log). Idempotent.
pub(super) fn migrate(conn: &mut Connection) -> Result<()> {
    if has_column(conn, "objects", "slot")? && has_column(conn, "world_meta", "arm")? {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (table, col, ty) in [
        ("objects", "x", "REAL"),
        ("objects", "y", "REAL"),
        ("objects", "z", "REAL"),
        ("objects", "slot", "INTEGER"),
        ("world_meta", "speed", "REAL NOT NULL DEFAULT 0"),
        ("world_meta", "arm", "TEXT"),
    ] {
        if !has_column(&tx, table, col)? {
            tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"))?;
        }
    }
    tx.execute_batch(MOTION_SCHEMA)?;
    assign_slots(&tx)?;
    tx.execute(
        "UPDATE world_meta SET arm = ?1 WHERE id = 1 AND arm IS NULL",
        params![serde_json::to_string(&cell::home())?],
    )?;
    tx.commit()?;
    Ok(())
}

/// The geometry of a location, if it has one in the cell.
pub fn place_of(conn: &Connection, id: &str) -> Result<Option<cell::Place>> {
    let t: Option<String> = conn
        .query_row(
            "SELECT loc_type FROM locations WHERE location_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(t.and_then(|t| cell::place_for(id, &t)))
}

fn free_slot(conn: &Connection, place: &str, except: Option<&str>) -> Result<i64> {
    let mut stmt = conn.prepare(
        "SELECT slot FROM objects WHERE location_id = ?1 AND slot IS NOT NULL AND object_id != ?2",
    )?;
    let used: Vec<i64> = stmt
        .query_map(params![place, except.unwrap_or("")], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok((0..).find(|i| !used.contains(i)).unwrap_or(0))
}

/// Give every resting part without a position a slot in its place.
pub(super) fn assign_slots(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT object_id, true_kind, location_id FROM objects WHERE slot IS NULL AND location_id NOT IN ('gripper', 'outside') ORDER BY object_id",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (id, kind, loc) in rows {
        let Some(p) = place_of(conn, &loc)? else {
            continue;
        };
        let slot = free_slot(conn, &loc, None)?;
        let pos = cell::slot_position(&p, slot as usize, cell::part_height(&kind));
        conn.execute(
            "UPDATE objects SET slot = ?1, x = ?2, y = ?3, z = ?4 WHERE object_id = ?5",
            params![slot, pos.x, pos.y, pos.z, id],
        )?;
    }
    Ok(())
}

impl SimWorld {
    fn arm(&self) -> Result<Arm> {
        let s: Option<String> =
            self.conn
                .query_row("SELECT arm FROM world_meta WHERE id = 1", [], |r| r.get(0))?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(cell::home))
    }

    pub fn speed(&self) -> Result<f64> {
        Ok(self
            .conn
            .query_row("SELECT speed FROM world_meta WHERE id = 1", [], |r| {
                r.get(0)
            })?)
    }

    /// Playback speed of motions: 0 = instant (no waiting), 1 = real time, 2 = twice as fast.
    pub fn set_speed(&mut self, speed: f64) -> Result<()> {
        let s = if speed.is_finite() {
            speed.clamp(0.0, 20.0)
        } else {
            0.0
        };
        self.conn
            .execute("UPDATE world_meta SET speed = ?1 WHERE id = 1", params![s])?;
        Ok(())
    }

    /// Command segments, log them, and wait while they play.
    fn run_motion(&mut self, segs: &[Segment]) -> Result<()> {
        if segs.is_empty() {
            return Ok(());
        }
        let speed = self.speed()?;
        let start = now_ms();
        let mut t = start;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for s in segs {
            let play = if speed > 0.0 {
                (s.nominal_ms as f64 / speed).round() as i64
            } else {
                0
            };
            tx.execute(
                "INSERT INTO motions(at_ms, play_ms, nominal_ms, kind, from_arm, to_arm, carrying) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![t, play, s.nominal_ms, s.kind, serde_json::to_string(&s.from)?, serde_json::to_string(&s.to)?, s.carrying],
            )?;
            t += play;
        }
        let last = segs.last().expect("non-empty").to;
        tx.execute(
            "UPDATE world_meta SET arm = ?1 WHERE id = 1",
            params![serde_json::to_string(&last)?],
        )?;
        tx.commit()?;
        if t > start {
            std::thread::sleep(Duration::from_millis((t - now_ms()).max(0) as u64));
        }
        Ok(())
    }

    fn receipt_exists(&self, op_key: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM receipts WHERE op_key = ?1",
                params![op_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn object_row(&self, id: &str) -> Result<Option<ObjectRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT true_kind, location_id, x, y, z FROM objects WHERE object_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
    }

    fn held(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT object_id FROM objects WHERE location_id = 'gripper'",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn available(&self, loc: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT available FROM locations WHERE location_id = ?1",
                params![loc],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some_and(|a| a != 0))
    }

    /// Refuse, before anything moves, a command the arm cannot reach.
    fn refuse_unreachable(
        &mut self,
        op_key: &str,
        capability: &str,
        args: &Value,
        why: &str,
    ) -> Result<AdapterOutcome> {
        let fp = fingerprint(&json!({"capability": capability, "args": args}));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rev = Self::revision(&tx)?;
        tx.execute(
            "INSERT OR IGNORE INTO receipts(op_key, capability, fingerprint, outcome, category, detail, world_revision, at_ms)
             VALUES (?1, ?2, ?3, 'rejected', 'unreachable', ?4, ?5, ?6)",
            params![op_key, capability, fp, json!({"message": why, "args": args}).to_string(), rev, now_ms()],
        )?;
        tx.commit()?;
        Ok(AdapterOutcome::Rejected {
            category: "unreachable".into(),
            message: why.to_string(),
        })
    }

    pub fn pick(
        &mut self,
        op_key: &str,
        object_id: &str,
        source_id: &str,
    ) -> (AdapterOutcome, Vec<FiredHumanAction>) {
        let args = json!({"object_id": object_id, "source_id": source_id});
        match self.pick_motion(op_key, object_id, source_id, &args) {
            Ok(Some(refused)) => (refused, vec![]),
            Ok(None) => {
                let out = self.effect(op_key, "workspace.pick", &args);
                // Lift, with the part if the grip took it.
                let carrying = self.held().ok().flatten();
                if let Ok(arm) = self.arm() {
                    let mut segs = vec![];
                    if carrying.is_none() && arm.grip < 1.0 {
                        segs.push(cell::plan_grip(arm, 1.0, None));
                    }
                    let from = segs.last().map(|s| s.to).unwrap_or(arm);
                    segs.extend(cell::plan_lift(from, carrying.as_deref()));
                    let _ = self.run_motion(&segs);
                }
                out
            }
            Err(e) => (
                AdapterOutcome::Unavailable {
                    message: format!("controller error: {}", e.msg),
                },
                vec![],
            ),
        }
    }

    /// Move to the part and close the gripper. `Some(outcome)` refuses the command.
    fn pick_motion(
        &mut self,
        op_key: &str,
        object_id: &str,
        source_id: &str,
        args: &Value,
    ) -> Result<Option<AdapterOutcome>> {
        if self.receipt_exists(op_key)? || Self::blackout(&self.conn)?.0 {
            return Ok(None); // replay, or a controller that is not accepting commands
        }
        // Only move for a command the adapter will accept; otherwise it refuses without moving.
        let Some((kind, loc, Some(x), Some(y), Some(z))) = self.object_row(object_id)? else {
            return Ok(None);
        };
        if loc != source_id || self.held()?.is_some() || !self.available(source_id)? {
            return Ok(None);
        }
        let arm = self.arm()?;
        let top = z + cell::part_height(&kind) / 2.0;
        let mut segs = vec![];
        if arm.grip < 1.0 {
            segs.push(cell::plan_grip(arm, 1.0, None));
        }
        let from = segs.last().map(|s| s.to).unwrap_or(arm);
        match cell::plan_to(from, x, y, top, None) {
            Ok(path) => segs.extend(path),
            Err(why) => {
                return Ok(Some(self.refuse_unreachable(
                    op_key,
                    "workspace.pick",
                    args,
                    &why,
                )?));
            }
        }
        let at = segs.last().expect("path").to;
        segs.push(cell::plan_grip(at, 0.0, Some(object_id)));
        self.run_motion(&segs)?;
        Ok(None)
    }

    pub fn place(
        &mut self,
        op_key: &str,
        object_id: &str,
        destination_id: &str,
    ) -> (AdapterOutcome, Vec<FiredHumanAction>) {
        let args = json!({"object_id": object_id, "destination_id": destination_id});
        match self.place_motion(op_key, object_id, destination_id, &args) {
            Ok(Some(refused)) => (refused, vec![]),
            Ok(None) => {
                let out = self.effect(op_key, "workspace.place", &args);
                self.pending_place = None;
                let carrying = self.held().ok().flatten();
                if let Ok(arm) = self.arm() {
                    let mut segs = vec![];
                    if carrying.is_none() && arm.grip < 1.0 {
                        segs.push(cell::plan_grip(arm, 1.0, None));
                    }
                    let from = segs.last().map(|s| s.to).unwrap_or(arm);
                    segs.extend(cell::plan_lift(from, carrying.as_deref()));
                    let _ = self.run_motion(&segs);
                }
                out
            }
            Err(e) => (
                AdapterOutcome::Unavailable {
                    message: format!("controller error: {}", e.msg),
                },
                vec![],
            ),
        }
    }

    /// Carry the part to a free slot at the destination and lower it.
    fn place_motion(
        &mut self,
        op_key: &str,
        object_id: &str,
        dest: &str,
        args: &Value,
    ) -> Result<Option<AdapterOutcome>> {
        self.pending_place = None;
        if self.receipt_exists(op_key)? || Self::blackout(&self.conn)?.0 {
            return Ok(None);
        }
        if self.held()?.as_deref() != Some(object_id) || !self.available(dest)? {
            return Ok(None);
        }
        let Some((kind, _, _, _, _)) = self.object_row(object_id)? else {
            return Ok(None);
        };
        let accepts: Option<String> = self
            .conn
            .query_row(
                "SELECT accepts_kind FROM locations WHERE location_id = ?1",
                params![dest],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if accepts.is_some_and(|a| a != kind) {
            return Ok(None); // the adapter refuses it (wrong bin) without moving
        }
        let Some(place) = place_of(&self.conn, dest)? else {
            return Ok(Some(self.refuse_unreachable(
                op_key,
                "workspace.place",
                args,
                &format!("{dest} has no place in the cell the arm can reach"),
            )?));
        };
        let slot = free_slot(&self.conn, dest, Some(object_id))?;
        let h = cell::part_height(&kind);
        let pos = cell::slot_position(&place, slot as usize, h);
        let arm = self.arm()?;
        match cell::plan_to(arm, pos.x, pos.y, pos.z + h / 2.0, Some(object_id)) {
            Ok(path) => self.run_motion(&path)?,
            Err(why) => {
                return Ok(Some(self.refuse_unreachable(
                    op_key,
                    "workspace.place",
                    args,
                    &why,
                )?));
            }
        }
        self.pending_place = Some(Placement {
            object_id: object_id.into(),
            destination: dest.into(),
            slot,
            pos,
        });
        Ok(None)
    }
}

/// Inside the effect transaction: where a picked or placed part now rests.
pub(super) fn settle_position(
    tx: &Transaction,
    pending: Option<&Placement>,
    capability: &str,
    object_id: &str,
    to: &str,
) -> Result<()> {
    {
        if capability == "workspace.pick" {
            tx.execute(
                "UPDATE objects SET slot = NULL, x = NULL, y = NULL, z = NULL WHERE object_id = ?1",
                params![object_id],
            )?;
            return Ok(());
        }
        let planned = pending.filter(|p| p.object_id == object_id && p.destination == to);
        let planned = match planned {
            Some(p) if !slot_taken(tx, to, p.slot, object_id)? => Some(p),
            _ => None,
        };
        let (slot, pos) = match planned {
            Some(p) => (p.slot, Some(p.pos)),
            None => {
                let slot = free_slot(tx, to, Some(object_id))?;
                let kind: String = tx.query_row(
                    "SELECT true_kind FROM objects WHERE object_id = ?1",
                    params![object_id],
                    |r| r.get(0),
                )?;
                let pos = place_of(tx, to)?
                    .map(|p| cell::slot_position(&p, slot as usize, cell::part_height(&kind)));
                (slot, pos)
            }
        };
        match pos {
            Some(p) => tx.execute(
                "UPDATE objects SET slot = ?1, x = ?2, y = ?3, z = ?4 WHERE object_id = ?5",
                params![slot, p.x, p.y, p.z, object_id],
            )?,
            None => tx.execute(
                "UPDATE objects SET slot = NULL, x = NULL, y = NULL, z = NULL WHERE object_id = ?1",
                params![object_id],
            )?,
        };
        Ok(())
    }
}

impl SimWorld {
    /// Everything a 3D viewer needs: the places, where each part is, the arm and
    /// its recent motion segments (all segments after `since`, else the last few).
    pub fn scene(&self, since: i64) -> Result<Value> {
        let mut stmt = self.conn.prepare("SELECT location_id, loc_type, accepts_kind, available FROM locations ORDER BY location_id")?;
        let places: Vec<Value> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(id, t, acc, av)| {
                cell::place_for(&id, &t).map(|p| json!({"id": p.id, "kind": p.kind, "cx": p.cx, "cy": p.cy, "yaw": p.yaw, "w": p.w, "d": p.d, "wall": p.wall, "floor": p.floor, "accepts": acc, "available": av != 0}))
            })
            .collect();
        let mut stmt = self.conn.prepare("SELECT object_id, true_kind, observed_kind, location_id, x, y, z FROM objects ORDER BY object_id")?;
        let objects: Vec<Value> = stmt
            .query_map([], |r| {
                Ok(json!({"id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "observed": r.get::<_, String>(2)?,
                           "at": r.get::<_, String>(3)?, "x": r.get::<_, Option<f64>>(4)?, "y": r.get::<_, Option<f64>>(5)?, "z": r.get::<_, Option<f64>>(6)?}))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let last: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM motions", [], |r| {
                    r.get(0)
                })?;
        // since < 0: everything; 0: the last few; > 0: everything after that segment.
        let from = if since < 0 {
            0
        } else if since > 0 {
            since
        } else {
            (last - 12).max(0)
        };
        let mut stmt = self.conn.prepare(
            "SELECT seq, at_ms, play_ms, nominal_ms, kind, from_arm, to_arm, carrying FROM motions WHERE seq > ?1 ORDER BY seq LIMIT 5000",
        )?;
        let motions: Vec<Value> = stmt
            .query_map(params![from], |r| {
                let f: String = r.get(5)?;
                let t: String = r.get(6)?;
                Ok(json!({"seq": r.get::<_, i64>(0)?, "at_ms": r.get::<_, i64>(1)?, "play_ms": r.get::<_, i64>(2)?, "nominal_ms": r.get::<_, i64>(3)?,
                          "kind": r.get::<_, String>(4)?, "from": serde_json::from_str::<Value>(&f).unwrap_or(Value::Null),
                          "to": serde_json::from_str::<Value>(&t).unwrap_or(Value::Null), "carrying": r.get::<_, Option<String>>(7)?}))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(json!({
            "now_ms": now_ms(),
            "speed": self.speed()?,
            "arm": self.arm()?,
            "places": places,
            "objects": objects,
            "motions": motions,
            "last_seq": last,
            "cell": {"shoulder_h": cell::SHOULDER_H, "l1": cell::L1, "l2": cell::L2, "l3": cell::L3},
        }))
    }

    /// Every motion segment so far (for replaying a rehearsal).
    pub fn motion_log(&self) -> Result<Vec<Value>> {
        let v = self.scene(-1)?;
        Ok(v["motions"].as_array().cloned().unwrap_or_default())
    }
}

fn slot_taken(conn: &Connection, place: &str, slot: i64, except: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM objects WHERE location_id = ?1 AND slot = ?2 AND object_id != ?3",
            params![place, slot, except],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}
