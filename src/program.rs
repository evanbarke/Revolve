//! Runs a plan's compiled program (see `codegen.rs`) under the durable executor.
//!
//! The Wasm module is compiled to native code by Cranelift (wasmtime) and runs with
//! a fuel limit, a fixed memory and no imports beyond the ones listed below.
//!
//! Replay model: every run of the program starts at its first step. Steps whose
//! outcome is already recorded return at once (their outputs come from the
//! record), so recovery after a crash needs nothing special: the program runs
//! again and reaches the first step that is not done. At that step the host
//! records the request and tells the program to stop; the program never performs
//! an effect. The executor then dispatches that one step through its usual path
//! (fresh observation, preconditions, atomic claim, gateway, verification) and
//! runs the program again for the next one. Every executor check between two
//! effects (reconciliation, pause/cancel, budgets, questions, goal revisions)
//! therefore runs exactly as with the interpreter.
//!
//! Translation validation: each request is checked against the checked plan
//! (dispatch order, capability, argument values, including replayed steps against
//! the arguments recorded when they were dispatched). Any difference stops the
//! run before anything is dispatched.
//!
//! Least authority: a module may import only `rt.*` and `cap.<id>` for capabilities
//! the policy grants; anything else is refused before it runs.

use crate::codegen::{CAP_MODULE, RT_MODULE};
use crate::store::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Store};

/// Instruction budget for one run of a program (all steps, including replay).
const FUEL: u64 = 5_000_000;
/// Values one run may create.
const MAX_VALUES: usize = 100_000;
const RT_FUNCS: [&str; 4] = ["json", "obj", "set", "out"];

/// How a run's plans are executed; recorded in the run's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecEngine {
    /// Plans run as compiled Wasm programs (native code).
    Compiled,
    /// Plans are walked step by step by the executor (the original engine).
    Interpreted,
}

impl ExecEngine {
    pub fn parse(s: &str) -> Result<ExecEngine, String> {
        match s {
            "compiled" => Ok(ExecEngine::Compiled),
            "interpreted" => Ok(ExecEngine::Interpreted),
            other => Err(format!("unknown engine `{other}` (compiled | interpreted)")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ExecEngine::Compiled => "compiled",
            ExecEngine::Interpreted => "interpreted",
        }
    }
}

/// What the program asked for when it stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum NextStep {
    /// Every step is done.
    Done,
    /// Dispatch this step with these (program-built, validated) arguments.
    Execute { action_id: i64, args: Value },
    /// The step cannot run: an output it needs is not available (as the interpreter
    /// reports it; the step fails without an effect and the plan is replaced).
    Fail { action_id: i64, reason: String },
    /// The program's request does not match the checked plan.
    Diverged(String),
    /// The program stopped abnormally (fuel, memory, a bad value).
    Trapped(String),
    /// The module was refused before running (imports, exports).
    Refused(String),
}

struct HostState {
    actions: Vec<ActionRow>,
    values: Vec<Value>,
    outcome: Option<NextStep>,
}

fn is_done(a: &ActionRow) -> bool {
    a.status == A_SUCCEEDED || a.status == A_SATISFIED
}

fn is_pending(a: &ActionRow) -> bool {
    a.status == A_PENDING || a.status == A_RETRY_PENDING
}

impl HostState {
    /// Record why the program must stop; the first reason wins, except that a
    /// program which keeps going after being told to stop has diverged.
    fn stop(&mut self, why: NextStep) -> i32 {
        if self.outcome.is_some() {
            self.outcome = Some(NextStep::Diverged(
                "the program continued after it was told to stop".into(),
            ));
        } else {
            self.outcome = Some(why);
        }
        1
    }

    fn first_undone(&self) -> Option<usize> {
        self.actions.iter().position(|a| !is_done(a))
    }

    fn row(&self, i: i32) -> Option<&ActionRow> {
        usize::try_from(i).ok().and_then(|i| self.actions.get(i))
    }

    fn push(&mut self, v: Value) -> wasmtime::Result<i32> {
        if self.values.len() >= MAX_VALUES {
            return Err(wasmtime::Error::msg("value limit reached"));
        }
        self.values.push(v);
        Ok(self.values.len() as i32 - 1)
    }

    fn value(&self, h: i32) -> wasmtime::Result<&Value> {
        usize::try_from(h)
            .ok()
            .and_then(|h| self.values.get(h))
            .ok_or_else(|| wasmtime::Error::msg(format!("bad value handle {h}")))
    }
}

fn read_str(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> wasmtime::Result<String> {
    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
        return Err(wasmtime::Error::msg("program exports no memory"));
    };
    let (p, l) = (ptr as u32 as usize, len as u32 as usize);
    let bytes = mem
        .data(&caller)
        .get(p..p.saturating_add(l))
        .ok_or_else(|| wasmtime::Error::msg("string outside program memory"))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| wasmtime::Error::msg("string is not UTF-8"))
}

/// `rt.out(step, src, key)`: an output of an earlier step, as the interpreter resolves it.
fn out(st: &mut HostState, cur: i32, src: i32, key: &str) -> i32 {
    if st.outcome.is_some() {
        st.stop(NextStep::Done); // marks divergence
        return -1;
    }
    let (Some(c), Some(s)) = (st.row(cur).cloned(), st.row(src).cloned()) else {
        st.stop(NextStep::Diverged(format!(
            "output read for unknown step index {cur}/{src}"
        )));
        return -1;
    };
    if src >= cur {
        st.stop(NextStep::Diverged(format!(
            "{} reads an output of {}, which does not run before it",
            c.step_id, s.step_id
        )));
        return -1;
    }
    if is_pending(&c) && st.first_undone() != Some(cur as usize) {
        st.stop(NextStep::Diverged(format!(
            "the program prepared {} before earlier steps were done",
            c.step_id
        )));
        return -1;
    }
    match crate::runtime::step_output(&s, key) {
        Some(v) => st.push(v).unwrap_or(-1),
        None if is_pending(&c) => {
            st.stop(NextStep::Fail {
                action_id: c.action_id,
                reason: format!(
                    "output {}.{key} is not available (step status {})",
                    s.step_id, s.status
                ),
            });
            -1
        }
        None => {
            st.stop(NextStep::Diverged(format!(
                "replaying {} could not read {}.{key} (status {})",
                c.step_id, s.step_id, s.status
            )));
            -1
        }
    }
}

/// `cap.<id>(step, args)`: replay a done step, or stop and request the next one.
fn cap_call(st: &mut HostState, cap: &str, step: i32, args: i32) -> wasmtime::Result<i32> {
    if st.outcome.is_some() {
        return Ok(st.stop(NextStep::Done));
    }
    let Some(row) = st.row(step).cloned() else {
        return Ok(st.stop(NextStep::Diverged(format!(
            "the program called {cap} for unknown step index {step}"
        ))));
    };
    if row.capability != cap {
        return Ok(st.stop(NextStep::Diverged(format!(
            "the program called {cap} for {}, which uses {}",
            row.step_id, row.capability
        ))));
    }
    let built = st.value(args)?.clone();
    if is_done(&row) {
        // Replay: the step's effect is recorded; nothing is dispatched.
        if row.status == A_SUCCEEDED
            && let Some(rec) = &row.resolved_arguments
            && *rec != built
        {
            return Ok(st.stop(NextStep::Diverged(format!(
                "replaying {} built arguments {} but {} were dispatched",
                row.step_id, built, rec
            ))));
        }
        return Ok(0);
    }
    if !is_pending(&row) {
        return Ok(st.stop(NextStep::Diverged(format!(
            "the program reached {} with status {}",
            row.step_id, row.status
        ))));
    }
    if let Some(first) = st.first_undone()
        && first != step as usize
    {
        return Ok(st.stop(NextStep::Diverged(format!(
            "the program reached {} before {} was done",
            row.step_id, st.actions[first].step_id
        ))));
    }
    let why = match crate::runtime::resolve_args(&row, &st.actions) {
        Ok(expected) if expected == built => NextStep::Execute {
            action_id: row.action_id,
            args: built,
        },
        Ok(expected) => NextStep::Diverged(format!(
            "{}: the program built {} but the checked plan resolves to {}",
            row.step_id, built, expected
        )),
        Err(e) => NextStep::Diverged(format!(
            "{}: the program built arguments the plan cannot resolve ({e})",
            row.step_id
        )),
    };
    Ok(st.stop(why))
}

/// Loads, links and runs plan programs for one executor.
pub struct ProgramHost {
    engine: Engine,
    linker: Linker<HostState>,
    grants: Vec<String>,
    modules: HashMap<String, Module>,
}

pub(crate) fn engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config.consume_fuel(true);
    Engine::new(&config).map_err(|e| format!("wasm engine: {e}"))
}

impl ProgramHost {
    pub fn new(grants: &BTreeMap<String, String>) -> Result<ProgramHost, String> {
        let engine = engine()?;
        let mut linker: Linker<HostState> = Linker::new(&engine);
        let e = |e: wasmtime::Error| format!("linking the runtime ABI: {e}");
        linker
            .func_wrap(
                RT_MODULE,
                "json",
                |mut c: Caller<'_, HostState>, p: i32, l: i32| {
                    let s = read_str(&mut c, p, l)?;
                    let v: Value = serde_json::from_str(&s)
                        .map_err(|e| wasmtime::Error::msg(format!("bad literal: {e}")))?;
                    c.data_mut().push(v)
                },
            )
            .map_err(e)?;
        linker
            .func_wrap(RT_MODULE, "obj", |mut c: Caller<'_, HostState>| {
                c.data_mut().push(Value::Object(Default::default()))
            })
            .map_err(e)?;
        linker
            .func_wrap(
                RT_MODULE,
                "set",
                |mut c: Caller<'_, HostState>, obj: i32, kp: i32, kl: i32, val: i32| {
                    let key = read_str(&mut c, kp, kl)?;
                    let st = c.data_mut();
                    let v = st.value(val)?.clone();
                    let target = usize::try_from(obj)
                        .ok()
                        .and_then(|o| st.values.get_mut(o))
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| wasmtime::Error::msg("set on a non-object"))?;
                    target.insert(key, v);
                    Ok(())
                },
            )
            .map_err(e)?;
        linker
            .func_wrap(
                RT_MODULE,
                "out",
                |mut c: Caller<'_, HostState>, cur: i32, src: i32, kp: i32, kl: i32| {
                    let key = read_str(&mut c, kp, kl)?;
                    Ok(out(c.data_mut(), cur, src, &key))
                },
            )
            .map_err(e)?;
        for cap in grants.keys() {
            let name = cap.clone();
            linker
                .func_wrap(
                    CAP_MODULE,
                    cap,
                    move |mut c: Caller<'_, HostState>, step: i32, args: i32| {
                        cap_call(c.data_mut(), &name, step, args)
                    },
                )
                .map_err(e)?;
        }
        Ok(ProgramHost {
            engine,
            linker,
            grants: grants.keys().cloned().collect(),
            modules: HashMap::new(),
        })
    }

    /// Compile (to native code) and check a module once; cached by content hash.
    fn module(&mut self, wasm: &[u8]) -> Result<Module, String> {
        let key = crate::util::sha256_hex(wasm);
        if let Some(m) = self.modules.get(&key) {
            return Ok(m.clone());
        }
        let m = Module::new(&self.engine, wasm).map_err(|e| format!("invalid module: {e}"))?;
        for imp in m.imports() {
            match imp.module() {
                RT_MODULE if RT_FUNCS.contains(&imp.name()) => {}
                CAP_MODULE if self.grants.iter().any(|g| g == imp.name()) => {}
                CAP_MODULE => {
                    return Err(format!(
                        "the program imports capability {}, which is not granted",
                        imp.name()
                    ));
                }
                other => {
                    return Err(format!(
                        "the program imports {other}.{}, which the runtime does not provide",
                        imp.name()
                    ));
                }
            }
        }
        self.modules.insert(key, m.clone());
        Ok(m)
    }

    /// Run the program from its first step until it finishes or asks for a step.
    pub fn next(&mut self, wasm: &[u8], actions: &[ActionRow]) -> NextStep {
        let module = match self.module(wasm) {
            Ok(m) => m,
            Err(e) => return NextStep::Refused(e),
        };
        let mut store = Store::new(
            &self.engine,
            HostState {
                actions: actions.to_vec(),
                values: vec![],
                outcome: None,
            },
        );
        if let Err(e) = store.set_fuel(FUEL) {
            return NextStep::Trapped(format!("fuel: {e}"));
        }
        let ran = self
            .linker
            .instantiate(&mut store, &module)
            .and_then(|i| i.get_typed_func::<(), i32>(&mut store, "run"))
            .and_then(|run| run.call(&mut store, ()));
        let mut st = store.into_data();
        match ran {
            Err(e) => NextStep::Trapped(format!("{e:#}")),
            Ok(code) => match (code, st.outcome.take()) {
                (1, Some(o)) => o,
                (_, Some(NextStep::Diverged(r))) => NextStep::Diverged(r),
                (_, Some(_)) => {
                    NextStep::Diverged("the program finished after it was told to stop".into())
                }
                (0, None) => match st.first_undone() {
                    None => NextStep::Done,
                    Some(i) => NextStep::Diverged(format!(
                        "the program finished but {} is not done",
                        st.actions[i].step_id
                    )),
                },
                (c, None) => {
                    NextStep::Diverged(format!("the program stopped with code {c} but no request"))
                }
            },
        }
    }
}

/// Ahead-of-time compile a module to native code for this machine (a `.cwasm`
/// artifact loadable by the same wasmtime version and configuration).
pub fn precompile(wasm: &[u8]) -> Result<Vec<u8>, String> {
    engine()?
        .precompile_module(wasm)
        .map_err(|e| format!("native compilation: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: i64, step: &str, cap: &str, args: Value, status: &str) -> ActionRow {
        ActionRow {
            action_id: id,
            plan_seq: Some(1),
            origin: "plan".into(),
            step_id: step.into(),
            seq: id - 1,
            goal_revision: 1,
            capability: cap.into(),
            capability_version: "1".into(),
            arguments: args.clone(),
            step_spec: Value::Null,
            resolved_arguments: None,
            descriptor: None,
            op_key: None,
            occurrence: None,
            status: status.into(),
            attempts: 0,
            result: None,
            error: None,
            claimed_at_ms: None,
            finished_at_ms: None,
        }
    }

    fn grants() -> BTreeMap<String, String> {
        crate::policy::Policy::default().grants
    }

    fn two_steps() -> Vec<ActionRow> {
        vec![
            row(
                1,
                "s1",
                "workspace.pick",
                json!({"object_id": "obj-1", "source_id": "bin-red"}),
                A_PENDING,
            ),
            row(
                2,
                "s2",
                "workspace.place",
                json!({"object_id": {"$ref": "s1.object_id"}, "destination_id": "tray-1"}),
                A_PENDING,
            ),
        ]
    }

    fn program(rows: &[ActionRow]) -> Vec<u8> {
        let steps: Vec<_> = rows.iter().map(crate::codegen::StepSrc::from_row).collect();
        crate::codegen::generate(&steps).unwrap().wasm
    }

    #[test]
    fn program_asks_for_one_step_at_a_time_and_replays_done_ones() {
        let mut rows = two_steps();
        let wasm = program(&rows);
        let mut host = ProgramHost::new(&grants()).unwrap();
        assert_eq!(
            host.next(&wasm, &rows),
            NextStep::Execute {
                action_id: 1,
                args: json!({"object_id": "obj-1", "source_id": "bin-red"})
            }
        );
        rows[0].status = A_SUCCEEDED.into();
        rows[0].resolved_arguments = Some(json!({"object_id": "obj-1", "source_id": "bin-red"}));
        rows[0].result = Some(json!({"object_id": "obj-1"}));
        // Data flows from s1's recorded output into s2's arguments inside the program.
        assert_eq!(
            host.next(&wasm, &rows),
            NextStep::Execute {
                action_id: 2,
                args: json!({"object_id": "obj-1", "destination_id": "tray-1"})
            }
        );
        rows[1].status = A_SATISFIED.into();
        assert_eq!(host.next(&wasm, &rows), NextStep::Done);
    }

    #[test]
    fn a_program_that_differs_from_the_plan_is_stopped_before_dispatch() {
        let rows = two_steps();
        // Compiled from a different plan: wrong source bin for s1.
        let mut other = rows.clone();
        other[0].arguments = json!({"object_id": "obj-1", "source_id": "bin-blue"});
        let wasm = program(&other);
        let mut host = ProgramHost::new(&grants()).unwrap();
        match host.next(&wasm, &rows) {
            NextStep::Diverged(r) => assert!(r.contains("checked plan resolves to"), "{r}"),
            x => panic!("expected divergence, got {x:?}"),
        }
    }

    #[test]
    fn modules_importing_ungranted_or_unknown_functions_are_refused() {
        let mut limited = grants();
        limited.remove("workspace.place");
        let rows = two_steps();
        let wasm = program(&rows);
        let mut host = ProgramHost::new(&limited).unwrap();
        match host.next(&wasm, &rows) {
            NextStep::Refused(r) => assert!(
                r.contains("workspace.place") && r.contains("not granted"),
                "{r}"
            ),
            x => panic!("expected refusal, got {x:?}"),
        }
        let sneaky = wat::parse_str(
            r#"(module (import "env" "system" (func (param i32) (result i32)))
                       (memory (export "memory") 1)
                       (func (export "run") (result i32) i32.const 0))"#,
        )
        .unwrap();
        match ProgramHost::new(&grants()).unwrap().next(&sneaky, &rows) {
            NextStep::Refused(r) => assert!(r.contains("env.system"), "{r}"),
            x => panic!("expected refusal, got {x:?}"),
        }
    }

    #[test]
    fn a_runaway_program_is_stopped_by_its_fuel_limit() {
        let spin = wat::parse_str(
            r#"(module (memory (export "memory") 1)
                       (func (export "run") (result i32) (loop $l (br $l)) i32.const 0))"#,
        )
        .unwrap();
        let t = std::time::Instant::now();
        match ProgramHost::new(&grants())
            .unwrap()
            .next(&spin, &two_steps())
        {
            NextStep::Trapped(r) => assert!(r.to_lowercase().contains("fuel"), "{r}"),
            x => panic!("expected a trap, got {x:?}"),
        }
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn a_program_that_ignores_stop_is_treated_as_diverged() {
        // Calls the capability for step 0 twice, ignoring the stop signal.
        let rogue = wat::parse_str(
            r#"(module
                 (import "rt" "obj" (func $obj (result i32)))
                 (import "cap" "workspace.pick" (func $pick (param i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "run") (result i32)
                   (drop (call $pick (i32.const 0) (call $obj)))
                   (drop (call $pick (i32.const 0) (call $obj)))
                   i32.const 1))"#,
        )
        .unwrap();
        match ProgramHost::new(&grants())
            .unwrap()
            .next(&rogue, &two_steps())
        {
            NextStep::Diverged(r) => {
                assert!(r.contains("continued after it was told to stop"), "{r}")
            }
            x => panic!("expected divergence, got {x:?}"),
        }
    }
}
