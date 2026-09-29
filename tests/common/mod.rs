#![allow(dead_code)]
use revolve::harness::{self, Arm, PlannerChoice};
use revolve::scenario::{self, Scenario};
use std::path::{Path, PathBuf};

pub fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_revolve"))
}

/// A fresh, project-local state directory for one test.
pub fn temp_state(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("revolve-tests")
        .join(revolve::util::fresh_id(name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn load(name: &str) -> Scenario {
    Scenario::load(&scenario::resolve(name, Path::new("scenarios")).unwrap()).unwrap()
}

pub fn new_run(state: &Path, sc: &Scenario, arm: Arm, human: &str) -> (String, PathBuf) {
    harness::create_run(state, sc, arm, &PlannerChoice::Scripted, human).unwrap()
}
