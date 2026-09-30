//! Revolve Script program runs through the durable executor.

mod common;

use common::*;
use revolve::harness;
use revolve::sim::SimWorld;
use revolve::store::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn program(name: &str) -> String {
    std::fs::read_to_string(format!("examples/programs/{name}.rvs")).unwrap()
}

fn start(state: &Path, scenario: &str, source: &str) -> (String, PathBuf) {
    harness::create_program_run(
        state,
        &load(scenario),
        json!({"source": source}),
        "test",
        "interactive",
    )
    .unwrap()
}

fn supervise(state: &Path, run: &str) -> harness::SupervisionLog {
    harness::supervise_with(&exe(), state, run, 0, true).unwrap()
}

fn where_is(dir: &Path) -> BTreeMap<String, String> {
    SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .objects
        .into_iter()
        .map(|o| (o.object_id, o.location_id))
        .collect()
}

fn applied_moves(dir: &Path) -> Vec<(String, String)> {
    SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .receipts
        .into_iter()
        .filter(|r| r.outcome == "applied" && r.capability == "workspace.place")
        .map(|r| {
            (
                r.detail["object_id"].as_str().unwrap().to_string(),
                r.detail["to"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn report(dir: &Path) -> serde_json::Value {
    RunStore::open(dir)
        .unwrap()
        .state()
        .unwrap()
        .final_report
        .unwrap()
}

#[test]
fn a_program_runs_and_success_is_checked_by_looking() {
    let state = temp_state("prog-run");
    let (run, dir) = start(&state, "messy-table", &program("sort-to-bins"));
    supervise(&state, &run);
    let st = RunStore::open(&dir).unwrap().state().unwrap();
    assert_eq!(st.status, R_COMPLETED, "{:?}", st.status_reason);
    let rep = report(&dir);
    assert_eq!(rep["claims_source"], "observation");
    assert_eq!(rep["all_expectations_held"], true);
    let at = where_is(&dir);
    assert!(at.values().all(|l| l != "table"), "{at:?}");
    assert_eq!(at["red-01"], "bin-red");
    assert_eq!(at["item-01"], "tray-4");
}

#[test]
fn a_power_cut_mid_program_is_recovered_without_repeating_a_move() {
    let state = temp_state("prog-crash");
    let (run, dir) = start(&state, "messy-table-power-cut", &program("sort-to-bins"));
    let log = supervise(&state, &run);
    assert_eq!(log.simulated_crashes, 1);
    assert_eq!(log.executor_processes, 2);
    assert_eq!(
        RunStore::open(&dir).unwrap().state().unwrap().status,
        R_COMPLETED
    );
    let moves = applied_moves(&dir);
    let mut unique = moves.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(moves.len(), unique.len(), "a move was repeated: {moves:?}");
    assert_eq!(moves.len(), 11);
    assert!(
        RunStore::open(&dir)
            .unwrap()
            .events(0, 10_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "reconciled"),
        "the interrupted move was reconciled"
    );
}

#[test]
fn a_part_taken_mid_run_is_reported_and_the_program_carries_on() {
    let state = temp_state("prog-thief");
    let (run, dir) = start(&state, "messy-table-thief", &program("sort-to-bins"));
    supervise(&state, &run);
    let store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_COMPLETED);
    let failed: Vec<_> = store
        .actions()
        .unwrap()
        .into_iter()
        .filter(|a| a.status == A_FAILED)
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].arguments["object_id"], "red-03");
    assert!(
        store
            .events(0, 10_000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "program_says" && e.message.contains("red-03"))
    );
}

#[test]
fn a_question_waits_for_the_person_and_the_answer_steers_the_program() {
    let state = temp_state("prog-ask");
    let (run, dir) = start(&state, "messy-table", &program("trays-by-color"));
    supervise(&state, &run);
    let mut store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_AWAITING_HUMAN);
    let q = store.open_questions().unwrap().remove(0);
    assert!(q.prompt.contains("item-01"));
    let leave = q
        .options
        .iter()
        .find(|o| o.label == "leave it on the table")
        .unwrap()
        .id
        .clone();
    let placed_before = applied_moves(&dir).len();
    store.answer(q.question_id, &leave, "test").unwrap();
    drop(store);
    supervise(&state, &run);
    assert_eq!(
        RunStore::open(&dir).unwrap().state().unwrap().status,
        R_COMPLETED
    );
    assert_eq!(
        where_is(&dir)["item-01"],
        "table",
        "the answer was followed"
    );
    assert!(applied_moves(&dir).len() > placed_before);
}

#[test]
fn a_new_program_takes_over_mid_run_and_finished_moves_stay() {
    let state = temp_state("prog-revise");
    let (run, dir) = start(&state, "messy-table", &program("trays-by-color"));
    supervise(&state, &run); // stops at the question about item-01
    let before = applied_moves(&dir);
    assert!(!before.is_empty());
    let revised = "# Everything left on the table goes to tray 4.\nfor p in parts(at=\"table\"):\n    move(p, \"tray-4\")\nexpect count(\"table\") == 0, \"the table is clear\"\n";
    let (rev, _) = RunStore::open(&dir)
        .unwrap()
        .submit_program(
            &json!({"language": revolve::script::LANGUAGE, "source": revised}),
            "test",
            "changed my mind",
            true,
        )
        .unwrap();
    assert_eq!(rev, 2);
    assert!(
        RunStore::open(&dir)
            .unwrap()
            .open_questions()
            .unwrap()
            .is_empty(),
        "the old question is obsolete"
    );
    supervise(&state, &run);
    let store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_COMPLETED);
    let after = applied_moves(&dir);
    assert_eq!(&after[..before.len()], &before[..], "finished moves stay");
    let at = where_is(&dir);
    assert!(at.values().all(|l| l != "table"));
    assert_eq!(at["item-01"], "tray-4");
    let plans = store.plans().unwrap();
    assert_eq!(plans.iter().filter(|p| p.status == "superseded").count(), 1);
}

#[test]
fn the_rehearsal_predicts_the_real_run() {
    let state = temp_state("prog-rehearse");
    let sc = load("messy-table");
    let world_dir = state.join("w");
    std::fs::create_dir_all(&world_dir).unwrap();
    SimWorld::create(&world_dir.join("world.db"), &sc).unwrap();
    let r = harness::rehearse(
        &state,
        &world_dir.join("world.db"),
        &sc.goal,
        &program("kits-from-table"),
    )
    .unwrap();
    assert_eq!(r.status, R_COMPLETED);
    let (run, dir) = start(&state, "messy-table", &program("kits-from-table"));
    supervise(&state, &run);
    let predicted: BTreeMap<String, String> = r
        .end_state
        .iter()
        .map(|o| {
            (
                o["id"].as_str().unwrap().to_string(),
                o["at"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(predicted, where_is(&dir));
    // The source world was not touched by the rehearsal.
    assert!(where_is(&world_dir).values().all(|l| l == "table"));
}

#[test]
fn a_program_that_no_longer_fits_the_table_does_not_run() {
    let state = temp_state("prog-stale");
    // Tray 9 does not exist on this table: refused when it is compiled for the run.
    let (run, dir) = start(
        &state,
        "messy-table",
        "for p in parts(at=\"table\"):\n    move(p, \"tray-9\")\nexpect count(\"table\") == 0, \"clear\"\n",
    );
    supervise(&state, &run);
    let st = RunStore::open(&dir).unwrap().state().unwrap();
    assert_eq!(st.status, R_BLOCKED);
    assert!(st.status_reason.unwrap().contains("tray-9"));
    assert!(applied_moves(&dir).is_empty());
}

#[test]
fn a_destination_computed_at_run_time_is_still_held_to_the_policy() {
    let state = temp_state("prog-policy");
    let sneaky = "dest = \"out\" + \"side\"\nfor p in parts(at=\"table\"):\n    move(p, dest)\nexpect count(\"table\") == 0, \"clear\"\n";
    let (run, dir) = start(&state, "messy-table", sneaky);
    supervise(&state, &run);
    let st = RunStore::open(&dir).unwrap().state().unwrap();
    assert_eq!(st.status, R_BLOCKED);
    assert!(st.status_reason.unwrap().contains("forbids"));
    assert!(applied_moves(&dir).is_empty(), "nothing was placed");
    // The part it picked up is still in the arm or back where it was: never outside.
    assert!(where_is(&dir).values().all(|l| l != "outside"));
}

#[test]
fn a_move_the_destination_cannot_take_does_nothing_and_says_why() {
    let state = temp_state("prog-refused");
    let src = "# Put a red block in the green bin
reds = parts(kind=\"red_block\")
ok = move(reds[0], \"bin-green\")
say(\"refused\" if not ok else \"moved\")
expect count(\"table\", kind=\"red_block\") == 3, \"the red blocks are still on the table\"
expect holding() == None, \"the arm holds nothing\"
";
    let (run, dir) = start(&state, "messy-table", src);
    supervise(&state, &run);
    let rep = report(&dir);
    assert_eq!(rep["all_expectations_held"], true, "{rep}");
    assert_eq!(
        where_is(&dir)["red-01"],
        "table",
        "the part stayed where it was"
    );
    let picked = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .receipts
        .into_iter()
        .filter(|r| r.outcome == "applied")
        .count();
    assert_eq!(picked, 0, "the arm did nothing");
    assert_eq!(rep["refused_moves"], 1);
    let why = rep["refusals"][0]["why"].as_str().unwrap();
    assert!(
        why.contains("bin-green only accepts green blocks")
            && why.contains("nothing was picked up"),
        "{why}"
    );
    assert_eq!(rep["refusals"][0]["line"], 3);
    assert_eq!(rep["messages"], json!(["refused"]), "move() returned False");
}

#[test]
fn the_arm_goes_over_a_wall_and_a_caged_tray_is_refused_before_anything_moves() {
    let state = temp_state("prog-barriers");
    // Every move from the pile to tray 1 crosses the wall.
    let over = "for p in parts(kind=\"red_block\", at=\"table\"):
    move(p, \"tray-1\")
expect count(\"tray-1\", kind=\"red_block\") == 3, \"the red blocks are in tray 1\"
";
    let (run, dir) = start(&state, "wall-table", over);
    supervise(&state, &run);
    assert_eq!(
        report(&dir)["all_expectations_held"],
        true,
        "{}",
        report(&dir)
    );
    let kinds: Vec<String> = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .motion_log()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.iter().any(|k| k == "rise"), "went over: {kinds:?}");
    // Tray 2 is inside a cage: refused before picking anything up.
    let caged = "r = parts(kind=\"red_block\", at=\"table\")
ok = move(r[0], \"tray-2\")
expect holding() == None, \"the arm holds nothing\"
expect count(\"table\", kind=\"red_block\") == 3, \"the red blocks are still on the table\"
";
    let (run, dir) = start(&state, "caged-tray", caged);
    supervise(&state, &run);
    let rep = report(&dir);
    assert_eq!(rep["all_expectations_held"], true, "{rep}");
    let why = rep["refusals"][0]["why"].as_str().unwrap();
    assert!(
        why.contains("the cage around tray 2") && why.contains("nothing was picked up"),
        "{why}"
    );
    let applied = SimWorld::open(&dir.join("world.db"))
        .unwrap()
        .truth()
        .unwrap()
        .receipts
        .into_iter()
        .filter(|r| r.outcome == "applied")
        .count();
    assert_eq!(applied, 0, "the arm did nothing");
}

#[test]
fn a_session_keeps_the_table_between_requests() {
    let state = temp_state("prog-session");
    let first = "for p in parts(kind=\"red_block\", at=\"table\"):\n    move(p, \"tray-1\")\nexpect count(\"tray-1\", kind=\"red_block\") == 3, \"the red blocks are in tray 1\"\n";
    let (run, dir) = start(&state, "messy-table", first);
    RunStore::open(&dir)
        .unwrap()
        .set_flag("session", true)
        .unwrap();
    supervise(&state, &run);
    let st = RunStore::open(&dir).unwrap().state().unwrap();
    assert_eq!(
        st.status, R_AWAITING_PLANNER,
        "the run waits for the next request"
    );
    assert_eq!(where_is(&dir)["red-01"], "tray-1");
    // The next request acts on the same table: the red blocks are already moved.
    let second = "for p in parts(at=\"tray-1\"):\n    move(p, \"tray-2\")\nexpect count(\"tray-1\") == 0, \"tray 1 is empty\"\n";
    RunStore::open(&dir)
        .unwrap()
        .submit_program(
            &json!({"language": revolve::script::LANGUAGE, "source": second}),
            "test",
            "next request",
            true,
        )
        .unwrap();
    supervise(&state, &run);
    let store = RunStore::open(&dir).unwrap();
    assert_eq!(store.state().unwrap().status, R_AWAITING_PLANNER);
    let at = where_is(&dir);
    assert!(
        ["red-01", "red-02", "red-03"]
            .iter()
            .all(|r| at[*r] == "tray-2"),
        "{at:?}"
    );
    let done: Vec<_> = store
        .events(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "program_finished")
        .collect();
    assert_eq!(done.len(), 2);
    assert!(
        done.iter()
            .all(|e| e.data.as_ref().unwrap()["all_expectations_held"] == true)
    );
}
