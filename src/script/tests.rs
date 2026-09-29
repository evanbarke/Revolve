//! Language tests: the checker's messages, and programs run end to end against a
//! small in-memory world that answers each outside-world call.

use super::host::{ScriptHost, ScriptStep};
use super::*;
use crate::model::{LocationType, LocationView, ObjectView, Observation};
use crate::store::*;
use serde_json::{Value, json};

struct FakeWorld {
    locs: Vec<LocationView>,
    objs: Vec<ObjectView>,
}

impl FakeWorld {
    fn new(table: &[(&str, &str)], trays: u32) -> FakeWorld {
        let mut locs = vec![LocationView {
            location_id: "gripper".into(),
            loc_type: LocationType::Gripper,
            accepts_kind: None,
            available: true,
        }];
        for (b, k) in [
            ("bin-red", "red_block"),
            ("bin-blue", "blue_block"),
            ("bin-green", "green_block"),
            ("bin-washer", "washer"),
        ] {
            locs.push(LocationView {
                location_id: b.into(),
                loc_type: LocationType::Bin,
                accepts_kind: Some(k.into()),
                available: true,
            });
        }
        for t in 1..=trays {
            locs.push(LocationView {
                location_id: format!("tray-{t}"),
                loc_type: LocationType::Tray,
                accepts_kind: None,
                available: true,
            });
        }
        locs.push(LocationView {
            location_id: "table".into(),
            loc_type: LocationType::Table,
            accepts_kind: None,
            available: true,
        });
        let objs = table
            .iter()
            .map(|(id, kind)| ObjectView {
                object_id: id.to_string(),
                observed_kind: kind.to_string(),
                location_id: "table".into(),
            })
            .collect();
        FakeWorld { locs, objs }
    }
    fn env(&self) -> CheckEnv {
        let obs = self.observe();
        CheckEnv::from(&obs, &crate::policy::Policy::default())
    }
    fn observe(&self) -> Observation {
        Observation {
            world_revision: 1,
            observed_at_ms: 0,
            source: "fake".into(),
            locations: self.locs.clone(),
            objects: self.objs.clone(),
            withheld: vec![],
        }
    }
    fn at(&self, loc: &str) -> Vec<&str> {
        self.objs
            .iter()
            .filter(|o| o.location_id == loc)
            .map(|o| o.object_id.as_str())
            .collect()
    }
    /// Carry out one call like the executor would; returns (status, result).
    fn apply(&mut self, cap: &str, args: &Value, answers: &mut Vec<&str>) -> (&'static str, Value) {
        match cap {
            "workspace.observe" => (A_SUCCEEDED, serde_json::to_value(self.observe()).unwrap()),
            "workspace.pick" => {
                let id = args["object_id"].as_str().unwrap();
                let held = self.objs.iter().any(|o| o.location_id == "gripper");
                match self.objs.iter_mut().find(|o| o.object_id == id) {
                    Some(o) if !held => {
                        o.location_id = "gripper".into();
                        (A_SUCCEEDED, json!({}))
                    }
                    _ => (A_FAILED, Value::Null),
                }
            }
            "workspace.place" => {
                let (id, to) = (
                    args["object_id"].as_str().unwrap(),
                    args["destination_id"].as_str().unwrap(),
                );
                let loc = self.locs.iter().find(|l| l.location_id == to).cloned();
                let Some(o) = self
                    .objs
                    .iter_mut()
                    .find(|o| o.object_id == id && o.location_id == "gripper")
                else {
                    return (A_FAILED, Value::Null);
                };
                match loc {
                    Some(l)
                        if l.accepts_kind
                            .as_deref()
                            .is_none_or(|k| k == o.observed_kind) =>
                    {
                        o.location_id = to.into();
                        (A_SUCCEEDED, json!({}))
                    }
                    _ => (A_FAILED, Value::Null),
                }
            }
            "human.ask" => (A_SUCCEEDED, json!({"answer": answers.remove(0)})),
            _ => (A_SUCCEEDED, Value::Null), // say, expect
        }
    }
}

fn row(i: usize, cap: &str, args: Value, status: &str, result: Value) -> ActionRow {
    ActionRow {
        action_id: i as i64 + 1,
        plan_seq: Some(1),
        origin: "plan".into(),
        step_id: format!("c{i}"),
        seq: i as i64,
        goal_revision: 1,
        capability: cap.into(),
        capability_version: "1".into(),
        arguments: args,
        step_spec: Value::Null,
        resolved_arguments: None,
        descriptor: None,
        op_key: None,
        occurrence: None,
        status: status.into(),
        attempts: 0,
        result: Some(result),
        error: None,
        claimed_at_ms: None,
        finished_at_ms: None,
    }
}

/// Run a program to the end against the fake world; returns the call log.
fn run(src: &str, world: &mut FakeWorld, mut answers: Vec<&str>) -> (ScriptStep, Vec<ActionRow>) {
    let compiled = compile(src, &world.env()).unwrap_or_else(|d| panic!("{d:?}"));
    let mut host = ScriptHost::new(&crate::policy::Policy::default().grants).unwrap();
    let mut log: Vec<ActionRow> = vec![];
    for _ in 0..2000 {
        match host.next(&compiled.wasm, &log) {
            ScriptStep::NewCall {
                index,
                capability,
                args,
                ..
            } => {
                assert_eq!(index, log.len());
                let (status, result) = world.apply(&capability, &args, &mut answers);
                log.push(row(index, &capability, args, status, result));
            }
            other => return (other, log),
        }
    }
    panic!("did not finish");
}

fn expectations(log: &[ActionRow]) -> Vec<(String, bool)> {
    log.iter()
        .filter(|r| r.capability == "expect")
        .map(|r| {
            (
                r.arguments["description"].as_str().unwrap().to_string(),
                r.arguments["held"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn says(log: &[ActionRow]) -> Vec<String> {
    log.iter()
        .filter(|r| r.capability == "human.say")
        .map(|r| r.arguments["message"].as_str().unwrap().to_string())
        .collect()
}

fn guide_example() -> String {
    let g = GUIDE;
    let start = g.find("    # Sort the table").unwrap();
    g[start..]
        .lines()
        .take_while(|l| l.starts_with("    ") || l.is_empty())
        .map(|l| l.strip_prefix("    ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_guide_example_sorts_the_table() {
    let mut w = FakeWorld::new(
        &[
            ("red-05", "red_block"),
            ("blue-03", "blue_block"),
            ("washer-09", "washer"),
            ("item-01", "unknown"),
            ("red-06", "red_block"),
        ],
        4,
    );
    let (end, log) = run(&guide_example(), &mut w, vec![]);
    assert_eq!(end, ScriptStep::Done);
    assert!(w.at("table").is_empty());
    assert_eq!(w.at("bin-red"), vec!["red-05", "red-06"]);
    assert_eq!(w.at("tray-4"), vec!["item-01"]);
    assert!(
        expectations(&log).iter().all(|(_, held)| *held),
        "{:?}",
        expectations(&log)
    );
    assert_eq!(
        log.iter()
            .filter(|r| r.capability == "workspace.place")
            .count(),
        5
    );
}

#[test]
fn control_flow_functions_and_values_behave_like_python() {
    let src = r#"
def double(x):
    return x * 2

def first_even(xs):
    for x in xs:
        if x % 2 == 0:
            return x
    return None

total = 0
for i in range(10):
    if i == 2:
        continue
    if i == 6:
        break
    total += double(i)
say(str(total))
say(str([n * n for n in range(5) if n != 3]))
say(str(first_even([3, 5, 8, 10])), str(first_even([1])))
xs = []
for x in [1, 2]:
    xs.append(x)
    xs.append(x)
say(str(len(xs)), str(sorted([3, 1, 2])), str(min([4, 2, 9])), str(max(4, 2, 9)))
say("yes" if 1 < 2 and not False else "no", str(None or 7), str(0 and 1))
say("abc".upper(), str("b" in "abc"), str(3 not in [1, 2]), str(-7 // 2), str(-7 % 2))
expect count("table") >= 0, "looked at the table"
"#;
    let mut w = FakeWorld::new(&[], 2);
    let (end, log) = run(src, &mut w, vec![]);
    assert_eq!(end, ScriptStep::Done);
    assert_eq!(
        says(&log),
        vec![
            "26",
            "[0, 1, 4, 16]",
            "8 None",
            "4 [1, 2, 3] 2 9",
            "yes 7 0",
            "ABC True True -4 1"
        ]
    );
}

#[test]
fn questions_return_the_persons_answer() {
    let src = r#"
answer = ask("Put the washer in tray 1 or tray 2?", ["tray 1", "tray 2"])
dest = "tray-1" if answer == "tray 1" else "tray-2"
for p in parts(kind="washer"):
    move(p, dest)
expect count("tray-2", kind="washer") == 1, "the washer is in tray 2"
"#;
    let mut w = FakeWorld::new(&[("washer-01", "washer")], 2);
    let (end, log) = run(src, &mut w, vec!["tray 2"]);
    assert_eq!(end, ScriptStep::Done);
    assert_eq!(w.at("tray-2"), vec!["washer-01"]);
    assert_eq!(
        expectations(&log),
        vec![("the washer is in tray 2".to_string(), true)]
    );
}

#[test]
fn a_rerun_retraces_the_log_and_a_changed_log_is_detected() {
    let mut w = FakeWorld::new(&[("red-01", "red_block"), ("red-02", "red_block")], 2);
    let src = "for p in parts(at=\"table\"):\n    move(p, \"tray-1\")\nexpect count(\"table\") == 0, \"clear\"\n";
    let (_, log) = run(src, &mut w, vec![]);
    let compiled = compile(src, &FakeWorld::new(&[], 2).env()).unwrap();
    let mut host = ScriptHost::new(&crate::policy::Policy::default().grants).unwrap();
    // Any prefix of the log leads to exactly the next logged call.
    for k in 0..log.len() {
        match host.next(&compiled.wasm, &log[..k]) {
            ScriptStep::NewCall {
                index,
                capability,
                args,
                ..
            } => {
                assert_eq!(
                    (index, capability.as_str(), &args),
                    (k, log[k].capability.as_str(), &log[k].arguments)
                );
            }
            other => panic!("prefix {k}: {other:?}"),
        }
    }
    assert_eq!(host.next(&compiled.wasm, &log), ScriptStep::Done);
    let mut tampered = log.clone();
    tampered[1].arguments = json!({"object_id": "red-99"});
    assert!(matches!(
        host.next(&compiled.wasm, &tampered),
        ScriptStep::Diverged(_)
    ));
    // A logged call not yet carried out is resumed, not re-requested.
    let mut pending = log[..3].to_vec();
    pending[2].status = A_PENDING.into();
    assert_eq!(
        host.next(&compiled.wasm, &pending),
        ScriptStep::Resume { action_id: 3 }
    );
}

#[test]
fn runtime_errors_name_the_line() {
    let src = "xs = parts(at=\"table\")\nfirst = xs[3]\nexpect count(\"table\") == 0, \"clear\"\n";
    let mut w = FakeWorld::new(&[("red-01", "red_block")], 1);
    let (end, _) = run(src, &mut w, vec![]);
    match end {
        ScriptStep::Error { line, message } => {
            assert_eq!(line, 2);
            assert!(message.contains("outside the list"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn runaway_computation_is_stopped_by_fuel() {
    let src = "n = 0\nfor a in range(100):\n    for b in range(100):\n        for c in range(100):\n            for d in range(100):\n                n += 1\nexpect count(\"table\") == 0, \"clear\"\n";
    let mut w = FakeWorld::new(&[], 1);
    let t = std::time::Instant::now();
    let (end, _) = run(src, &mut w, vec![]);
    // Stopped by a resource limit (instruction fuel or the value limit), never hangs.
    assert!(
        matches!(end, ScriptStep::Trapped(ref m) if m.to_lowercase().contains("fuel") || m.contains("too many values")),
        "{end:?}"
    );
    assert!(t.elapsed().as_secs() < 20);
}

#[test]
fn checker_messages_point_at_the_problem() {
    let env = FakeWorld::new(&[], 4).env();
    let cases = [
        (
            "for p in parts(kind=\"red\"):\n    move(p, \"tray-1\")\nexpect count(\"table\") == 0, \"x\"\n",
            1,
            "did you mean \"red_block\"",
        ),
        (
            "for p in parts():\n    move(p, \"tray-9\")\nexpect count(\"table\") == 0, \"x\"\n",
            2,
            "no place called \"tray-9\"",
        ),
        (
            "for p in parts():\n    if p.color == \"red\":\n        move(p, \"tray-1\")\nexpect count(\"table\") == 0, \"x\"\n",
            2,
            "a part has no `color`",
        ),
        (
            "def f(x):\n    return f(x)\nf(1)\nexpect count(\"table\") == 0, \"x\"\n",
            1,
            "recursion is not allowed",
        ),
        (
            "for p in parts():\n    move(p, \"tray-1\")\n",
            3,
            "must say how success is checked",
        ),
        ("n = 3\nexpect n == 3, \"x\"\n", 2, "must look at the table"),
        (
            "tray = \"tray-1\"\nmove(parts()[0], tary)\nexpect count(\"table\") == 0, \"x\"\n",
            2,
            "did you mean `tray`",
        ),
        (
            "mvoe(parts()[0], \"tray-1\")\nexpect count(\"table\") == 0, \"x\"\n",
            1,
            "did you mean move()",
        ),
        (
            "move(parts()[0])\nexpect count(\"table\") == 0, \"x\"\n",
            1,
            "needs a destination",
        ),
        (
            "for p in parts():\n    if p.kind == \"washer\":\n        move(p, \"outside\")\nexpect count(\"table\") == 0, \"x\"\n",
            3,
            "no place called \"outside\"",
        ),
    ];
    for (src, line, want) in cases {
        let diags = compile(src, &env)
            .err()
            .unwrap_or_else(|| panic!("accepted: {src}"));
        assert!(
            diags
                .iter()
                .any(|d| d.line == line && d.message.contains(want)),
            "{src}\n{diags:?}"
        );
    }
}

#[test]
fn modules_import_only_what_the_program_uses() {
    let env = FakeWorld::new(&[], 2).env();
    let look_only = compile("expect count(\"tray-1\") == 0, \"tray 1 is empty\"\n", &env).unwrap();
    assert_eq!(look_only.imports, vec!["cap.observe"]);
    let mover = compile("for p in parts(at=\"table\"):\n    move(p, \"tray-1\")\nexpect count(\"table\") == 0, \"clear\"\n", &env).unwrap();
    assert_eq!(mover.imports, vec!["cap.observe", "cap.pick", "cap.place"]);
    assert!(mover.facts.literal_destinations.contains("tray-1"));
    // Deterministic.
    let again = compile("for p in parts(at=\"table\"):\n    move(p, \"tray-1\")\nexpect count(\"table\") == 0, \"clear\"\n", &env).unwrap();
    assert_eq!(mover.wasm, again.wasm);
}
