//! Plans executed as compiled WebAssembly programs (native code via Cranelift).

mod common;

use common::*;
use revolve::harness::{self, Arm, PlannerChoice};
use revolve::program::ExecEngine;
use revolve::sim::SimWorld;
use revolve::store::*;
use revolve::util::SIMULATED_CRASH_EXIT_CODE;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn resume(state: &Path, run: &str) -> std::process::ExitStatus {
    Command::new(exe())
        .args(["resume", "--run", run, "--quiet", "--state-dir"])
        .arg(state)
        .status()
        .expect("spawn revolve")
}

/// Ids derived from the run id (operation keys, reservations) differ between any
/// two runs, also where the planner embeds them in step ids; rename them
/// consistently, in order of first appearance.
fn rename_ids(s: &str, names: &mut HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    'scan: while !rest.is_empty() {
        for p in ["op-", "res-", "rcpt-"] {
            if let Some(tail) = rest.strip_prefix(p) {
                let hex = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
                let boundary =
                    out.is_empty() || !out.ends_with(|c: char| c.is_ascii_alphanumeric());
                if hex >= 8 && boundary {
                    let id = &rest[..p.len() + hex];
                    let n = names.len() + 1;
                    out.push_str(names.entry(id.to_string()).or_insert(format!("id#{n}")));
                    rest = &rest[p.len() + hex..];
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn normalize(v: &Value, names: &mut HashMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(rename_ids(s, names)),
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize(x, names)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), normalize(x, names)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// Everything observable about a run: its final status, every action (with
/// arguments, status and result) and where every object ended up.
fn fingerprint(dir: &Path) -> (String, Value, Vec<(String, String)>) {
    let store = RunStore::open(dir).unwrap();
    let mut names = HashMap::new();
    let actions: Vec<Value> = store
        .actions()
        .unwrap()
        .iter()
        .map(|a| {
            serde_json::json!([
                a.origin,
                a.plan_seq,
                a.step_id,
                a.capability,
                a.resolved_arguments,
                a.status,
                a.result,
                a.error
            ])
        })
        .collect();
    let actions = normalize(&Value::Array(actions), &mut names);
    let truth = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap();
    let mut objects: Vec<(String, String)> = truth
        .objects
        .iter()
        .map(|o| (o.object_id.clone(), o.location_id.clone()))
        .collect();
    objects.sort();
    (store.state().unwrap().status, actions, objects)
}

#[test]
fn compiled_and_interpreted_engines_produce_identical_runs() {
    for name in [
        "nominal",
        "crash-after-effect",
        "timeout-unknown",
        "goal-change-rework",
        "cancel",
    ] {
        let sc = load(name);
        let mut prints = vec![];
        for engine in [ExecEngine::Compiled, ExecEngine::Interpreted] {
            let state = temp_state(&format!("equiv-{name}"));
            let (run, dir) = harness::create_run_with(
                &state,
                &sc,
                Arm::Revolve,
                &PlannerChoice::Scripted,
                "scripted",
                engine,
            )
            .unwrap();
            harness::supervise_with(&exe(), &state, &run, 0, true).unwrap();
            assert_eq!(
                RunStore::open(&dir).unwrap().meta().unwrap().engine,
                engine.as_str()
            );
            prints.push(fingerprint(&dir));
        }
        assert_eq!(prints[0], prints[1], "{name}: engines disagree");
    }
}

#[test]
fn each_plan_runs_as_a_program_that_imports_only_what_it_uses() {
    let state = temp_state("least-authority");
    let (run, dir) = new_run(&state, &load("goal-change"), Arm::Revolve, "scripted");
    harness::supervise_with(&exe(), &state, &run, 0, true).unwrap();
    let store = RunStore::open(&dir).unwrap();
    let accepted: Vec<i64> = store
        .plans()
        .unwrap()
        .iter()
        .filter(|p| p.status != "rejected")
        .map(|p| p.plan_seq)
        .collect();
    assert!(
        accepted.len() >= 2,
        "goal change should produce a second plan"
    );
    for seq in accepted {
        let p = store
            .program(seq)
            .unwrap()
            .expect("program recorded for every accepted plan");
        assert_eq!(revolve::util::sha256_hex(&p.wasm), p.sha256);
        assert!(p.wasm.starts_with(b"\0asm"));
        let mut used: Vec<String> = vec![];
        for a in store.plan_actions(seq).unwrap() {
            if !used.contains(&a.capability) {
                used.push(a.capability);
            }
        }
        assert_eq!(
            p.imports, used,
            "plan #{seq} imports exactly the capabilities it uses"
        );
    }
    let built = store
        .events(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "program_built")
        .count();
    assert!(built >= 2);
}

/// Crash a compiled run right after an effect, so a plan is active with its program stored.
fn crashed_run(tag: &str) -> (std::path::PathBuf, String, std::path::PathBuf, usize) {
    let state = temp_state(tag);
    let (run, dir) = new_run(
        &state,
        &load("crash-after-effect"),
        Arm::Revolve,
        "interactive",
    );
    assert_eq!(resume(&state, &run).code(), Some(SIMULATED_CRASH_EXIT_CODE));
    let applied = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .receipts
        .iter()
        .filter(|r| r.outcome == "applied")
        .count();
    (state, run, dir, applied)
}

#[test]
fn a_tampered_program_is_not_run_and_nothing_new_is_dispatched() {
    let (state, run, dir, applied_before) = crashed_run("tampered");
    let seq = RunStore::open(&dir)
        .unwrap()
        .state()
        .unwrap()
        .active_plan_seq
        .unwrap();
    {
        let db = rusqlite::Connection::open(dir.join("run.db")).unwrap();
        let mut wasm: Vec<u8> = db
            .query_row(
                "SELECT wasm FROM programs WHERE plan_seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap();
        let n = wasm.len();
        wasm[n - 1] ^= 0xff;
        db.execute(
            "UPDATE programs SET wasm = ?1 WHERE plan_seq = ?2",
            rusqlite::params![wasm, seq],
        )
        .unwrap();
    }
    resume(&state, &run);
    let st = RunStore::open(&dir).unwrap().state().unwrap();
    assert_eq!(st.status, R_BLOCKED);
    assert!(
        st.status_reason
            .as_deref()
            .unwrap_or("")
            .contains("integrity"),
        "{:?}",
        st.status_reason
    );
    let applied_after = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .receipts
        .iter()
        .filter(|r| r.outcome == "applied")
        .count();
    // The in-flight action was reconciled (status lookup, no new effect); nothing else ran.
    assert_eq!(applied_after, applied_before);
}

#[test]
fn a_plan_without_a_stored_program_is_compiled_on_resume() {
    let (state, run, dir, _) = crashed_run("regenerate");
    let seq = RunStore::open(&dir)
        .unwrap()
        .state()
        .unwrap()
        .active_plan_seq
        .unwrap();
    rusqlite::Connection::open(dir.join("run.db"))
        .unwrap()
        .execute("DELETE FROM programs WHERE plan_seq = ?1", [seq])
        .unwrap();
    assert!(resume(&state, &run).success());
    let store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_COMPLETED);
    assert!(store.program(seq).unwrap().is_some());
    let v = revolve::eval::evaluate_run(&dir).unwrap();
    assert!(v.success, "{}", revolve::eval::summarize_verdict(&v));
}
