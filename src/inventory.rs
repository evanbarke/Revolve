//! Inventory record adapter. Local and transactional: each operation commits its
//! ledger rows and its idempotency record together in `run.db`, so its outcome can
//! always be established by status lookup.
//!
//! Accounting model: `on_hand` counts known parts that have not been recorded as
//! consumed — in a stock bin, in the gripper, or placed in a tray but not yet
//! recorded. Consumption is recorded only after a part is observed in a kit tray;
//! a return (rework) re-adds it. `reserved` never exceeds `on_hand` (CHECK constraint).

use crate::capability::counts_from_value;
use crate::model::KindCounts;
use crate::sim::{AdapterOutcome, StatusResult};
use crate::store::RunStore;
use crate::util::{Result, fingerprint, now_ms};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReservationView {
    pub reservation_id: String,
    pub counts: KindCounts,
    pub remaining: KindCounts,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InventoryView {
    pub on_hand: KindCounts,
    pub reserved: KindCounts,
    pub reservations: Vec<ReservationView>,
}

impl InventoryView {
    pub fn open_reservations(&self) -> impl Iterator<Item = &ReservationView> {
        self.reservations.iter().filter(|r| r.status == "open")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerEntry {
    pub entry_id: i64,
    pub op_key: String,
    pub entry_type: String,
    pub kind: String,
    pub delta_on_hand: i64,
    pub delta_reserved: i64,
    pub reservation_id: Option<String>,
    pub evidence: Option<String>,
    pub at_ms: i64,
}

pub fn view(store: &RunStore) -> Result<InventoryView> {
    let mut v = InventoryView::default();
    let mut stmt = store
        .conn
        .prepare("SELECT kind, on_hand, reserved FROM inventory ORDER BY kind")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (k, oh, rs) = row?;
        v.on_hand.insert(k.clone(), oh as u32);
        v.reserved.insert(k, rs as u32);
    }
    let mut stmt = store.conn.prepare(
        "SELECT reservation_id, counts, remaining, status FROM reservations ORDER BY created_at_ms",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (id, c, rem, st) = row?;
        v.reservations.push(ReservationView {
            reservation_id: id,
            counts: serde_json::from_str(&c).unwrap_or_default(),
            remaining: serde_json::from_str(&rem).unwrap_or_default(),
            status: st,
        });
    }
    Ok(v)
}

pub fn ledger(store: &RunStore) -> Result<Vec<LedgerEntry>> {
    let mut stmt = store.conn.prepare(
        "SELECT entry_id, op_key, entry_type, kind, delta_on_hand, delta_reserved, reservation_id, evidence, at_ms FROM ledger ORDER BY entry_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LedgerEntry {
                entry_id: r.get(0)?,
                op_key: r.get(1)?,
                entry_type: r.get(2)?,
                kind: r.get(3)?,
                delta_on_hand: r.get(4)?,
                delta_reserved: r.get(5)?,
                reservation_id: r.get(6)?,
                evidence: r.get(7)?,
                at_ms: r.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Objects whose consumption is currently recorded (consumed and not returned).
pub fn consumed_evidence(store: &RunStore) -> Result<Vec<String>> {
    let mut set: Vec<String> = vec![];
    for e in ledger(store)? {
        match (e.entry_type.as_str(), e.evidence) {
            ("consume", Some(obj)) => set.push(obj),
            ("return", Some(obj)) => set.retain(|o| o != &obj),
            _ => {}
        }
    }
    Ok(set)
}

pub fn status(store: &RunStore, op_key: &str) -> StatusResult {
    let row: rusqlite::Result<Option<(String, Option<String>, String)>> = store
        .conn
        .query_row(
            "SELECT outcome, category, result FROM inventory_ops WHERE op_key = ?1",
            params![op_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional();
    match row {
        Err(e) => StatusResult::Unavailable {
            message: e.to_string(),
        },
        Ok(None) => StatusResult::NotFound,
        Ok(Some((o, _, res))) if o == "applied" => StatusResult::Applied {
            receipt: serde_json::from_str(&res).unwrap_or(Value::Null),
        },
        Ok(Some((_, c, res))) => StatusResult::Rejected {
            category: c.unwrap_or_default(),
            message: serde_json::from_str::<Value>(&res)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
                .unwrap_or_default(),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn ledger_row(
    tx: &Transaction,
    op_key: &str,
    ty: &str,
    kind: &str,
    d_on: i64,
    d_res: i64,
    res: Option<&str>,
    evidence: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO ledger(op_key, entry_type, kind, delta_on_hand, delta_reserved, reservation_id, evidence, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![op_key, ty, kind, d_on, d_res, res, evidence, now_ms()],
    )?;
    Ok(())
}

fn load_reservation(tx: &Transaction, id: &str) -> Result<Option<(KindCounts, String)>> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT remaining, status FROM reservations WHERE reservation_id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(rem, st)| (serde_json::from_str(&rem).unwrap_or_default(), st)))
}

/// Apply an inventory capability under an operation key.
pub fn apply(store: &mut RunStore, op_key: &str, capability: &str, args: &Value) -> AdapterOutcome {
    match apply_inner(store, op_key, capability, args) {
        Ok(o) => o,
        Err(e) => AdapterOutcome::Unavailable {
            message: format!("inventory store error (rolled back): {}", e.msg),
        },
    }
}

fn apply_inner(
    store: &mut RunStore,
    op_key: &str,
    capability: &str,
    args: &Value,
) -> Result<AdapterOutcome> {
    let fp = fingerprint(&json!({"capability": capability, "args": args}));
    // Evidence for adjust_count lives in the same store; read it before the write transaction.
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<(String, String, Option<String>, String)> = tx
        .query_row(
            "SELECT fingerprint, outcome, category, result FROM inventory_ops WHERE op_key = ?1",
            params![op_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((efp, outcome, cat, res)) = existing {
        if efp != fp {
            return Ok(AdapterOutcome::Rejected {
                category: "key_conflict".into(),
                message: format!(
                    "operation key {op_key} was already used with different arguments; no effect"
                ),
            });
        }
        let res: Value = serde_json::from_str(&res)?;
        return Ok(if outcome == "applied" {
            AdapterOutcome::Applied {
                receipt: res,
                replayed: true,
            }
        } else {
            AdapterOutcome::Rejected {
                category: cat.unwrap_or_default(),
                message: res
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }
        });
    }

    let on_hand = |tx: &Transaction, k: &str| -> Result<(i64, i64)> {
        Ok(tx
            .query_row(
                "SELECT on_hand, reserved FROM inventory WHERE kind = ?1",
                params![k],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0)))
    };
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let q = |k: &str| args.get(k).and_then(Value::as_u64).unwrap_or(0) as i64;

    let result: std::result::Result<Value, (String, String)> = match capability {
        "inventory.reserve" => {
            let counts = counts_from_value(&args["counts"]);
            let mut short = vec![];
            for (k, n) in &counts {
                let (oh, rs) = on_hand(&tx, k)?;
                if oh - rs < *n as i64 {
                    short.push(format!("{k}: requested {n}, reservable {}", oh - rs));
                }
            }
            if !short.is_empty() {
                Err((
                    "insufficient_stock".into(),
                    format!("cannot reserve: {}", short.join("; ")),
                ))
            } else {
                let rid = format!("res-{}", &op_key[3..op_key.len().min(13)]);
                tx.execute(
                    "INSERT INTO reservations(reservation_id, counts, remaining, status, created_at_ms) VALUES (?1, ?2, ?2, 'open', ?3)",
                    params![rid, serde_json::to_string(&counts)?, now_ms()],
                )?;
                for (k, n) in &counts {
                    tx.execute(
                        "UPDATE inventory SET reserved = reserved + ?1 WHERE kind = ?2",
                        params![*n as i64, k],
                    )?;
                    ledger_row(&tx, op_key, "reserve", k, 0, *n as i64, Some(&rid), None)?;
                }
                Ok(json!({"reservation_id": rid, "counts": counts}))
            }
        }
        "inventory.consume" => {
            let rid = s("reservation_id");
            let kind = s("part_kind");
            let qty = q("quantity");
            let evidence = s("evidence_object_id");
            match load_reservation(&tx, &rid)? {
                None => Err((
                    "unknown_reservation".into(),
                    format!("no reservation {rid}"),
                )),
                Some((_, st)) if st != "open" => Err((
                    "unknown_reservation".into(),
                    format!("reservation {rid} is {st}"),
                )),
                Some((mut rem, _)) => {
                    let have = rem.get(&kind).copied().unwrap_or(0) as i64;
                    if have < qty {
                        Err((
                            "insufficient_stock".into(),
                            format!(
                                "reservation {rid} has {have} {kind} remaining, {qty} requested"
                            ),
                        ))
                    } else {
                        rem.insert(kind.clone(), (have - qty) as u32);
                        tx.execute(
                            "UPDATE reservations SET remaining = ?1 WHERE reservation_id = ?2",
                            params![serde_json::to_string(&rem)?, rid],
                        )?;
                        tx.execute(
                            "UPDATE inventory SET on_hand = on_hand - ?1, reserved = reserved - ?1 WHERE kind = ?2",
                            params![qty, kind],
                        )?;
                        ledger_row(
                            &tx,
                            op_key,
                            "consume",
                            &kind,
                            -qty,
                            -qty,
                            Some(&rid),
                            Some(&evidence),
                        )?;
                        Ok(
                            json!({"reservation_id": rid, "part_kind": kind, "quantity": qty, "evidence_object_id": evidence}),
                        )
                    }
                }
            }
        }
        "inventory.release" => {
            let rid = s("reservation_id");
            match load_reservation(&tx, &rid)? {
                None => Err((
                    "unknown_reservation".into(),
                    format!("no reservation {rid}"),
                )),
                Some((_, st)) if st != "open" => {
                    Ok(json!({"reservation_id": rid, "released": {}, "note": "already released"}))
                }
                Some((rem, _)) => {
                    for (k, n) in &rem {
                        if *n > 0 {
                            tx.execute(
                                "UPDATE inventory SET reserved = reserved - ?1 WHERE kind = ?2",
                                params![*n as i64, k],
                            )?;
                            ledger_row(
                                &tx,
                                op_key,
                                "release",
                                k,
                                0,
                                -(*n as i64),
                                Some(&rid),
                                None,
                            )?;
                        }
                    }
                    tx.execute(
                        "UPDATE reservations SET status = 'released' WHERE reservation_id = ?1",
                        params![rid],
                    )?;
                    let released: KindCounts = rem.into_iter().filter(|(_, n)| *n > 0).collect();
                    Ok(json!({"reservation_id": rid, "released": released}))
                }
            }
        }
        "inventory.return" => {
            let kind = s("part_kind");
            let qty = q("quantity");
            let evidence = s("evidence_object_id");
            tx.execute(
                "UPDATE inventory SET on_hand = on_hand + ?1 WHERE kind = ?2",
                params![qty, kind],
            )?;
            ledger_row(&tx, op_key, "return", &kind, qty, 0, None, Some(&evidence))?;
            Ok(json!({"part_kind": kind, "quantity": qty, "evidence_object_id": evidence}))
        }
        "inventory.adjust_count" => {
            let kind = s("part_kind");
            let observed = q("observed_count");
            let rev = args
                .get("observation_revision")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            let cited: Option<String> = tx
                .query_row(
                    "SELECT stock_counts FROM observations WHERE world_revision = ?1 ORDER BY at_ms DESC LIMIT 1",
                    params![rev as i64],
                    |r| r.get(0),
                )
                .optional()?;
            let cited: Option<KindCounts> = cited.and_then(|s| serde_json::from_str(&s).ok());
            let (oh, rs) = on_hand(&tx, &kind)?;
            match cited.as_ref().and_then(|c| c.get(&kind)) {
                None => Err((
                    "evidence_mismatch".into(),
                    format!(
                        "no recorded observation at world revision {rev} with a count for {kind}"
                    ),
                )),
                Some(n) if *n as i64 != observed => Err((
                    "evidence_mismatch".into(),
                    format!("observation at revision {rev} shows {n} {kind}, not {observed}"),
                )),
                Some(_) if observed < rs => Err((
                    "insufficient_stock".into(),
                    format!(
                        "cannot set {kind} on_hand to {observed} below reserved {rs}; release reservations first"
                    ),
                )),
                Some(_) => {
                    let delta = observed - oh;
                    tx.execute(
                        "UPDATE inventory SET on_hand = ?1 WHERE kind = ?2",
                        params![observed, kind],
                    )?;
                    ledger_row(
                        &tx,
                        op_key,
                        "adjust",
                        &kind,
                        delta,
                        0,
                        None,
                        Some(&format!("observation@rev{rev}")),
                    )?;
                    Ok(
                        json!({"part_kind": kind, "previous": oh, "on_hand": observed, "delta": delta, "observation_revision": rev}),
                    )
                }
            }
        }
        other => Err((
            "malformed".into(),
            format!("inventory adapter does not implement {other}"),
        )),
    };

    let (outcome, cat, res) = match &result {
        Ok(v) => ("applied", None, v.clone()),
        Err((c, m)) => ("rejected", Some(c.clone()), json!({"message": m})),
    };
    tx.execute(
        "INSERT INTO inventory_ops(op_key, capability, fingerprint, outcome, category, result, at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![op_key, capability, fp, outcome, cat, res.to_string(), now_ms()],
    )?;
    tx.commit()?;
    Ok(match result {
        Ok(v) => AdapterOutcome::Applied {
            receipt: v,
            replayed: false,
        },
        Err((c, m)) => AdapterOutcome::Rejected {
            category: c,
            message: m,
        },
    })
}
