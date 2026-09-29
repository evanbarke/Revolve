//! Code generation: a checked plan becomes a WebAssembly program.
//!
//! The program is the plan's control flow and data flow as machine code: it walks
//! the steps in dispatch order, builds each step's arguments (literals from its
//! data segment, outputs of earlier steps through `rt.out`), and calls the
//! capability. A Wasm runtime compiles it to native code (Cranelift).
//!
//! Authority is structural: the module imports only the capabilities its plan
//! uses, one import per capability (`cap.<capability id>`), plus four pure value
//! helpers (`rt.*`). It has no other way to reach anything. The host decides what
//! a capability call does (see `program.rs`); the program performs no effect
//! itself.
//!
//! ABI (all i32):
//!   rt.json(ptr, len) -> value          literal JSON from the data segment
//!   rt.obj() -> value                   new empty object
//!   rt.set(obj, key_ptr, key_len, value)
//!   rt.out(step, src_step, key_ptr, key_len) -> value | -1 (not available)
//!   cap.<id>(step, args) -> 0 continue | 1 stop
//!   export run() -> 0 all steps done | 1 stopped
//!   export memory
//!
//! Codegen is deterministic: the same steps always give the same bytes.

use crate::ir::ArgValue;
use serde_json::Value;
use std::collections::BTreeMap;
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, CustomSection, DataSection, EntityType, ExportKind,
    ExportSection, Function, FunctionSection, ImportSection, MemorySection, MemoryType, Module,
    TypeSection, ValType,
};

/// Identifies the ABI and lowering; recorded with every program.
pub const CODEGEN: &str = "revolve-wasm/1";
pub const RT_MODULE: &str = "rt";
pub const CAP_MODULE: &str = "cap";

/// One step as the code generator sees it (from a compiled plan or its stored rows).
#[derive(Debug, Clone, PartialEq)]
pub struct StepSrc {
    pub step_id: String,
    pub capability: String,
    pub arguments: BTreeMap<String, Value>,
}

impl StepSrc {
    pub fn from_step(s: &crate::ir::Step) -> StepSrc {
        StepSrc {
            step_id: s.step_id.clone(),
            capability: s.capability.clone(),
            arguments: s.arguments.clone(),
        }
    }

    pub fn from_row(a: &crate::store::ActionRow) -> StepSrc {
        StepSrc {
            step_id: a.step_id.clone(),
            capability: a.capability.clone(),
            arguments: a
                .arguments
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Program {
    pub wasm: Vec<u8>,
    pub sha256: String,
    /// Capabilities the program imports, in import order.
    pub imports: Vec<String>,
}

/// Interned strings laid out in the data segment.
#[derive(Default)]
struct Data {
    bytes: Vec<u8>,
    at: BTreeMap<String, (i32, i32)>,
}

impl Data {
    fn intern(&mut self, s: &str) -> (i32, i32) {
        if let Some(&p) = self.at.get(s) {
            return p;
        }
        let p = (self.bytes.len() as i32, s.len() as i32);
        self.bytes.extend_from_slice(s.as_bytes());
        self.at.insert(s.to_string(), p);
        p
    }
}

const T_JSON: u32 = 0; // (i32, i32) -> i32   rt.json, cap.*
const T_OBJ: u32 = 1; // () -> i32            rt.obj, run
const T_SET: u32 = 2; // (i32 x4) -> ()       rt.set
const T_OUT: u32 = 3; // (i32 x4) -> i32      rt.out
const F_JSON: u32 = 0;
const F_OBJ: u32 = 1;
const F_SET: u32 = 2;
const F_OUT: u32 = 3;
const RT_IMPORTS: u32 = 4;

/// Lower steps (in dispatch order) to a Wasm module.
pub fn generate(steps: &[StepSrc]) -> Result<Program, String> {
    let index: BTreeMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.step_id.as_str(), i))
        .collect();
    if index.len() != steps.len() {
        return Err("duplicate step ids".into());
    }
    // One import per capability used, in first-use order.
    let mut caps: Vec<String> = vec![];
    for s in steps {
        if !caps.contains(&s.capability) {
            caps.push(s.capability.clone());
        }
    }
    let cap_fn = |c: &str| RT_IMPORTS + caps.iter().position(|x| x == c).unwrap() as u32;
    let run_fn = RT_IMPORTS + caps.len() as u32;

    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function([], [ValType::I32]);
    types.ty().function([ValType::I32; 4], []);
    types.ty().function([ValType::I32; 4], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import(RT_MODULE, "json", EntityType::Function(T_JSON));
    imports.import(RT_MODULE, "obj", EntityType::Function(T_OBJ));
    imports.import(RT_MODULE, "set", EntityType::Function(T_SET));
    imports.import(RT_MODULE, "out", EntityType::Function(T_OUT));
    for c in &caps {
        imports.import(CAP_MODULE, c, EntityType::Function(T_JSON));
    }

    let mut functions = FunctionSection::new();
    functions.function(T_OBJ);

    // run(): locals 0 = args object, 1 = a value read from an earlier step.
    let mut data = Data::default();
    let mut f = Function::new([(2, ValType::I32)]);
    let (args_l, val_l) = (0u32, 1u32);
    for (i, s) in steps.iter().enumerate() {
        let i = i as i32;
        f.instructions().call(F_OBJ).local_set(args_l);
        for (k, v) in &s.arguments {
            let (kp, kl) = data.intern(k);
            match ArgValue::parse(v).map_err(|e| format!("{}.{k}: {e}", s.step_id))? {
                ArgValue::Literal(lit) => {
                    let (vp, vl) =
                        data.intern(&serde_json::to_string(&lit).map_err(|e| e.to_string())?);
                    f.instructions()
                        .local_get(args_l)
                        .i32_const(kp)
                        .i32_const(kl)
                        .i32_const(vp)
                        .i32_const(vl)
                        .call(F_JSON)
                        .call(F_SET);
                }
                ArgValue::Ref { step_id, output } => {
                    let src = *index.get(step_id.as_str()).ok_or(format!(
                        "{}.{k} refers to unknown step {step_id}",
                        s.step_id
                    ))? as i32;
                    if src >= i {
                        return Err(format!(
                            "{}.{k} refers to {step_id}, which does not run before it",
                            s.step_id
                        ));
                    }
                    let (op, ol) = data.intern(&output);
                    f.instructions()
                        .i32_const(i)
                        .i32_const(src)
                        .i32_const(op)
                        .i32_const(ol)
                        .call(F_OUT)
                        .local_tee(val_l)
                        .i32_const(0)
                        .i32_lt_s()
                        .if_(BlockType::Empty)
                        .i32_const(1)
                        .return_()
                        .end()
                        .local_get(args_l)
                        .i32_const(kp)
                        .i32_const(kl)
                        .local_get(val_l)
                        .call(F_SET);
                }
            }
        }
        f.instructions()
            .i32_const(i)
            .local_get(args_l)
            .call(cap_fn(&s.capability))
            .if_(BlockType::Empty)
            .i32_const(1)
            .return_()
            .end();
    }
    f.instructions().i32_const(0).end();

    let pages = (data.bytes.len() as u64).div_ceil(65536).max(1);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, run_fn);

    let mut code = CodeSection::new();
    code.function(&f);

    let mut datas = DataSection::new();
    datas.active(0, &ConstExpr::i32_const(0), data.bytes.iter().copied());

    // Step ids by index, for people and tools reading the module.
    let meta = serde_json::json!({
        "codegen": CODEGEN,
        "steps": steps.iter().map(|s| &s.step_id).collect::<Vec<_>>(),
    });
    let meta = meta.to_string();

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&datas)
        .section(&CustomSection {
            name: "revolve.plan".into(),
            data: meta.as_bytes().into(),
        });
    let wasm = module.finish();
    Ok(Program {
        sha256: crate::util::sha256_hex(&wasm),
        wasm,
        imports: caps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, cap: &str, args: Value) -> StepSrc {
        StepSrc {
            step_id: id.into(),
            capability: cap.into(),
            arguments: serde_json::from_value(args).unwrap(),
        }
    }

    #[test]
    fn lowering_is_deterministic_and_imports_only_used_capabilities() {
        let steps = vec![
            step(
                "s1",
                "workspace.pick",
                json!({"object_id": "obj-1", "source_id": "bin-red"}),
            ),
            step(
                "s2",
                "workspace.place",
                json!({"object_id": {"$ref": "s1.object_id"}, "destination_id": "tray-1"}),
            ),
        ];
        let a = generate(&steps).unwrap();
        let b = generate(&steps).unwrap();
        assert_eq!(a.wasm, b.wasm);
        assert_eq!(a.imports, vec!["workspace.pick", "workspace.place"]);
        assert!(a.wasm.starts_with(b"\0asm"));
    }

    #[test]
    fn references_must_point_backwards_to_known_steps() {
        let fwd = vec![
            step(
                "s1",
                "workspace.place",
                json!({"object_id": {"$ref": "s2.object_id"}}),
            ),
            step("s2", "workspace.pick", json!({"object_id": "obj-1"})),
        ];
        assert!(
            generate(&fwd)
                .unwrap_err()
                .contains("does not run before it")
        );
        let unknown = vec![step(
            "s1",
            "workspace.place",
            json!({"object_id": {"$ref": "sx.object_id"}}),
        )];
        assert!(generate(&unknown).unwrap_err().contains("unknown step"));
    }
}
