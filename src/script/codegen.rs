//! Lower a checked Revolve Script program to a WebAssembly module.
//!
//! Control flow (if/elif/else, for loops with break/continue, and/or, calls
//! between functions) is real Wasm control flow, compiled to native code by
//! Cranelift. Values are handles into a runtime-managed value table (`rt.*`).
//! Handles 0, 1 and 2 are None, True and False.
//!
//! Anything that reaches outside the program (looking at the table, moving
//! parts, asking the person, checking an expectation) is an import. A negative
//! result means "stop now": it is propagated out of every function, and `run`
//! returns 1. Each call carries its source line so the person can see which
//! line of the program is running.
//!
//! ABI `revolve-script/1`:
//!   rt.int(n) rt.str(ptr,len) rt.list_new() rt.list_push(l,v) rt.kw_new()
//!   rt.kw_set(o,kp,kl,v) rt.binop(op,a,b,line) rt.unop(op,a,line) rt.truthy(a)
//!   rt.attr(a,np,nl,line) rt.index(a,i,line) rt.snapshot(a,line) rt.len(a)
//!   rt.item(a,i) rt.builtin(id,args,kw,line) rt.method(r,np,nl,args,line)
//!   rt.expect(cond,dp,dl,line) -> 0 | -1
//!   cap.observe(query,args,kw,line)  cap.pick(args,kw,line)  cap.place(args,kw,line)
//!   human.ask(args,kw,line)  human.say(args,kw,line)            -> value | -1
//!   export run() -> 0 finished | 1 stopped; export memory

use super::check::builtin;
use super::syntax::*;
use std::collections::{BTreeMap, BTreeSet};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, CustomSection, DataSection, EntityType, ExportKind,
    ExportSection, Function, FunctionSection, GlobalSection, GlobalType, ImportSection,
    Instruction as I, MemorySection, MemoryType, Module, TypeSection, ValType,
};

pub const ABI: &str = "revolve-script/1";

pub const NONE: i32 = 0;
pub const TRUE: i32 = 1;
pub const FALSE: i32 = 2;

/// Pure builtins handled by `rt.builtin` (index = id).
pub const PURE_BUILTINS: [&str; 9] = [
    "len", "range", "str", "int", "min", "max", "sum", "abs", "sorted",
];
/// World queries handled by `cap.observe` (index = query id).
pub const QUERIES: [&str; 4] = ["parts", "places", "count", "holding"];

pub const BINOPS: [&str; 13] = [
    "+", "-", "*", "//", "%", "==", "!=", "<", "<=", ">", ">=", "in", "not in",
];

fn binop_id(op: BinOp) -> i32 {
    match op {
        BinOp::Add => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::FloorDiv => 3,
        BinOp::Mod => 4,
    }
}
fn cmpop_id(op: CmpOp) -> i32 {
    match op {
        CmpOp::Eq => 5,
        CmpOp::Ne => 6,
        CmpOp::Lt => 7,
        CmpOp::Le => 8,
        CmpOp::Gt => 9,
        CmpOp::Ge => 10,
        CmpOp::In => 11,
        CmpOp::NotIn => 12,
    }
}

// Types
const T_I_I: u32 = 0;
const T_II_I: u32 = 1;
const T_III_I: u32 = 2;
const T_IIII_I: u32 = 3;
const T_V_I: u32 = 4;
const T_II_V: u32 = 5;
const T_IIII_V: u32 = 6;
const T_IIIII_I: u32 = 7;

// rt imports (function index = position)
const RT: [(&str, u32); 17] = [
    ("int", T_I_I),
    ("str", T_II_I),
    ("list_new", T_V_I),
    ("list_push", T_II_V),
    ("kw_new", T_V_I),
    ("kw_set", T_IIII_V),
    ("binop", T_IIII_I),
    ("unop", T_III_I),
    ("truthy", T_I_I),
    ("attr", T_IIII_I),
    ("index", T_III_I),
    ("snapshot", T_II_I),
    ("len", T_I_I),
    ("item", T_II_I),
    ("builtin", T_IIII_I),
    ("method", T_IIIII_I),
    ("expect", T_IIII_I),
];
/// Names of the pure runtime imports (for load-time checks).
pub const RT_NAMES: [&str; 17] = [
    "int",
    "str",
    "list_new",
    "list_push",
    "kw_new",
    "kw_set",
    "binop",
    "unop",
    "truthy",
    "attr",
    "index",
    "snapshot",
    "len",
    "item",
    "builtin",
    "method",
    "expect",
];

fn rt(name: &str) -> u32 {
    RT.iter().position(|(n, _)| *n == name).unwrap() as u32
}

/// External imports: (module, name, type), imported only when used.
const EXT: [(&str, &str, u32); 5] = [
    ("cap", "observe", T_IIII_I),
    ("cap", "pick", T_III_I),
    ("cap", "place", T_III_I),
    ("human", "ask", T_III_I),
    ("human", "say", T_III_I),
];

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

#[derive(Clone, Copy, PartialEq)]
enum Label {
    Break,
    Continue,
    Other,
}

struct Module_<'a> {
    data: Data,
    globals: BTreeMap<String, u32>,
    funcs: BTreeMap<String, u32>,
    ext: BTreeMap<&'static str, u32>, // "cap.pick" -> function index
    prog: &'a Program,
}

struct Fn_<'m, 'a> {
    m: &'m mut Module_<'a>,
    locals: BTreeMap<String, u32>,
    nlocals: u32,
    code: Vec<I<'static>>,
    labels: Vec<Label>,
    tmp: u32,
}

fn assigned_names(body: &[Stmt], out: &mut BTreeSet<String>) {
    for s in body {
        match &s.kind {
            StmtKind::Assign(n, _) | StmtKind::AugAssign(n, _, _) => {
                out.insert(n.clone());
            }
            StmtKind::For(v, _, b) => {
                out.insert(v.clone());
                assigned_names(b, out);
            }
            StmtKind::If(branches, orelse) => {
                for (_, b) in branches {
                    assigned_names(b, out);
                }
                assigned_names(orelse, out);
            }
            _ => {}
        }
    }
}

impl Fn_<'_, '_> {
    fn new_local(&mut self) -> u32 {
        let l = self.nlocals;
        self.nlocals += 1;
        l
    }
    fn emit(&mut self, i: I<'static>) {
        self.code.push(i);
    }
    fn str_const(&mut self, s: &str) {
        let (p, l) = self.m.data.intern(s);
        self.emit(I::I32Const(p));
        self.emit(I::I32Const(l));
    }
    /// After a call that may ask to stop: propagate a negative result.
    fn check_stop(&mut self) {
        self.emit(I::LocalTee(self.tmp));
        self.emit(I::I32Const(0));
        self.emit(I::I32LtS);
        self.emit(I::If(BlockType::Empty));
        self.emit(I::I32Const(-1));
        self.emit(I::Return);
        self.emit(I::End);
        self.emit(I::LocalGet(self.tmp));
    }
    fn get_var(&mut self, n: &str) {
        if let Some(&l) = self.locals.get(n) {
            self.emit(I::LocalGet(l));
        } else if let Some(&g) = self.m.globals.get(n) {
            self.emit(I::GlobalGet(g));
        } else {
            self.emit(I::I32Const(NONE));
        }
    }
    fn set_var(&mut self, n: &str) {
        if let Some(&l) = self.locals.get(n) {
            self.emit(I::LocalSet(l));
        } else if let Some(&g) = self.m.globals.get(n) {
            self.emit(I::GlobalSet(g));
        } else {
            self.emit(I::Drop);
        }
    }
    fn br_to(&mut self, want: Label) {
        let depth = self
            .labels
            .iter()
            .rev()
            .position(|l| *l == want)
            .expect("checked: inside a loop");
        self.emit(I::Br(depth as u32));
    }

    fn block(&mut self, body: &[Stmt]) {
        for s in body {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        let line = s.line as i32;
        match &s.kind {
            StmtKind::Assign(n, e) => {
                self.expr(e);
                self.set_var(n);
            }
            StmtKind::AugAssign(n, op, e) => {
                self.emit(I::I32Const(binop_id(*op)));
                self.get_var(n);
                self.expr(e);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("binop")));
                self.set_var(n);
            }
            StmtKind::If(branches, orelse) => self.if_chain(branches, orelse),
            StmtKind::For(var, iter, body) => {
                let (snap, n, i) = (self.new_local(), self.new_local(), self.new_local());
                self.expr(iter);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("snapshot")));
                self.emit(I::LocalTee(snap));
                self.emit(I::Call(rt("len")));
                self.emit(I::LocalSet(n));
                self.emit(I::I32Const(0));
                self.emit(I::LocalSet(i));
                self.emit(I::Block(BlockType::Empty));
                self.labels.push(Label::Break);
                self.emit(I::Loop(BlockType::Empty));
                self.labels.push(Label::Other);
                self.emit(I::LocalGet(i));
                self.emit(I::LocalGet(n));
                self.emit(I::I32GeS);
                self.emit(I::BrIf(1));
                self.emit(I::LocalGet(snap));
                self.emit(I::LocalGet(i));
                self.emit(I::Call(rt("item")));
                self.set_var(var);
                self.emit(I::LocalGet(i));
                self.emit(I::I32Const(1));
                self.emit(I::I32Add);
                self.emit(I::LocalSet(i));
                self.emit(I::Block(BlockType::Empty));
                self.labels.push(Label::Continue);
                self.block(body);
                self.labels.pop();
                self.emit(I::End);
                self.emit(I::Br(0));
                self.labels.pop();
                self.emit(I::End);
                self.labels.pop();
                self.emit(I::End);
            }
            StmtKind::Break => self.br_to(Label::Break),
            StmtKind::Continue => self.br_to(Label::Continue),
            StmtKind::Pass => {}
            StmtKind::Return(e) => {
                match e {
                    Some(e) => self.expr(e),
                    None => self.emit(I::I32Const(NONE)),
                }
                self.emit(I::Return);
            }
            StmtKind::Expect(cond, desc) => {
                self.expr(cond);
                self.str_const(desc);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("expect")));
                self.check_stop();
                self.emit(I::Drop);
            }
            StmtKind::Expr(e) => {
                self.expr(e);
                self.emit(I::Drop);
            }
        }
    }

    fn if_chain(&mut self, branches: &[(Expr, Vec<Stmt>)], orelse: &[Stmt]) {
        let Some(((cond, body), rest)) = branches.split_first() else {
            self.block(orelse);
            return;
        };
        self.expr(cond);
        self.emit(I::Call(rt("truthy")));
        self.emit(I::If(BlockType::Empty));
        self.labels.push(Label::Other);
        self.block(body);
        if !rest.is_empty() || !orelse.is_empty() {
            self.emit(I::Else);
            self.if_chain(rest, orelse);
        }
        self.labels.pop();
        self.emit(I::End);
    }

    /// Build a list of positional args and an object of keyword args; leaves
    /// (args, kw) on the stack.
    fn args(&mut self, args: &[Expr], kwargs: &[(String, Expr)]) {
        let l = self.new_local();
        self.emit(I::Call(rt("list_new")));
        self.emit(I::LocalSet(l));
        for a in args {
            self.emit(I::LocalGet(l));
            self.expr(a);
            self.emit(I::Call(rt("list_push")));
        }
        let k = self.new_local();
        self.emit(I::Call(rt("kw_new")));
        self.emit(I::LocalSet(k));
        for (name, v) in kwargs {
            self.emit(I::LocalGet(k));
            self.str_const(name);
            self.expr(v);
            self.emit(I::Call(rt("kw_set")));
        }
        self.emit(I::LocalGet(l));
        self.emit(I::LocalGet(k));
    }

    fn ext(&self, key: &'static str) -> u32 {
        self.m.ext[key]
    }

    fn expr(&mut self, e: &Expr) {
        let line = e.line as i32;
        match &e.kind {
            ExprKind::Int(n) => {
                self.emit(I::I32Const(*n as i32));
                self.emit(I::Call(rt("int")));
            }
            ExprKind::Str(s) => {
                self.str_const(s);
                self.emit(I::Call(rt("str")));
            }
            ExprKind::Bool(b) => self.emit(I::I32Const(if *b { TRUE } else { FALSE })),
            ExprKind::None => self.emit(I::I32Const(NONE)),
            ExprKind::Name(n) => self.get_var(n),
            ExprKind::List(items) => {
                let l = self.new_local();
                self.emit(I::Call(rt("list_new")));
                self.emit(I::LocalSet(l));
                for it in items {
                    self.emit(I::LocalGet(l));
                    self.expr(it);
                    self.emit(I::Call(rt("list_push")));
                }
                self.emit(I::LocalGet(l));
            }
            ExprKind::ListComp {
                elt,
                var,
                iter,
                cond,
            } => {
                // A comprehension variable is local to the comprehension.
                let (out, snap, n, i, v) = (
                    self.new_local(),
                    self.new_local(),
                    self.new_local(),
                    self.new_local(),
                    self.new_local(),
                );
                let shadow = self.locals.insert(var.clone(), v);
                self.emit(I::Call(rt("list_new")));
                self.emit(I::LocalSet(out));
                self.expr(iter);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("snapshot")));
                self.emit(I::LocalTee(snap));
                self.emit(I::Call(rt("len")));
                self.emit(I::LocalSet(n));
                self.emit(I::I32Const(0));
                self.emit(I::LocalSet(i));
                self.emit(I::Block(BlockType::Empty));
                self.labels.push(Label::Other);
                self.emit(I::Loop(BlockType::Empty));
                self.labels.push(Label::Other);
                self.emit(I::LocalGet(i));
                self.emit(I::LocalGet(n));
                self.emit(I::I32GeS);
                self.emit(I::BrIf(1));
                self.emit(I::LocalGet(snap));
                self.emit(I::LocalGet(i));
                self.emit(I::Call(rt("item")));
                self.emit(I::LocalSet(v));
                self.emit(I::LocalGet(i));
                self.emit(I::I32Const(1));
                self.emit(I::I32Add);
                self.emit(I::LocalSet(i));
                match cond {
                    Some(c) => {
                        self.expr(c);
                        self.emit(I::Call(rt("truthy")));
                        self.emit(I::If(BlockType::Empty));
                        self.labels.push(Label::Other);
                        self.emit(I::LocalGet(out));
                        self.expr(elt);
                        self.emit(I::Call(rt("list_push")));
                        self.labels.pop();
                        self.emit(I::End);
                    }
                    None => {
                        self.emit(I::LocalGet(out));
                        self.expr(elt);
                        self.emit(I::Call(rt("list_push")));
                    }
                }
                self.emit(I::Br(0));
                self.labels.pop();
                self.emit(I::End);
                self.labels.pop();
                self.emit(I::End);
                match shadow {
                    Some(prev) => {
                        self.locals.insert(var.clone(), prev);
                    }
                    None => {
                        self.locals.remove(var);
                    }
                }
                self.emit(I::LocalGet(out));
            }
            ExprKind::Attr(obj, name) => {
                self.expr(obj);
                self.str_const(name);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("attr")));
            }
            ExprKind::Index(obj, idx) => {
                self.expr(obj);
                self.expr(idx);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("index")));
            }
            ExprKind::Call { func, args, kwargs } => self.call(e, func, args, kwargs),
            ExprKind::Bin(op, a, b) => {
                self.emit(I::I32Const(binop_id(*op)));
                self.expr(a);
                self.expr(b);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("binop")));
            }
            ExprKind::Cmp(op, a, b) => {
                self.emit(I::I32Const(cmpop_id(*op)));
                self.expr(a);
                self.expr(b);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("binop")));
            }
            ExprKind::And(a, b) | ExprKind::Or(a, b) => {
                let t = self.new_local();
                self.expr(a);
                self.emit(I::LocalTee(t));
                self.emit(I::Call(rt("truthy")));
                if matches!(e.kind, ExprKind::Or(..)) {
                    self.emit(I::I32Eqz);
                }
                self.emit(I::If(BlockType::Result(ValType::I32)));
                self.labels.push(Label::Other);
                self.expr(b);
                self.emit(I::Else);
                self.emit(I::LocalGet(t));
                self.labels.pop();
                self.emit(I::End);
            }
            ExprKind::Not(a) => {
                self.emit(I::I32Const(1));
                self.expr(a);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("unop")));
            }
            ExprKind::Neg(a) => {
                self.emit(I::I32Const(0));
                self.expr(a);
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("unop")));
            }
            ExprKind::IfExp(a, c, b) => {
                self.expr(c);
                self.emit(I::Call(rt("truthy")));
                self.emit(I::If(BlockType::Result(ValType::I32)));
                self.labels.push(Label::Other);
                self.expr(a);
                self.emit(I::Else);
                self.expr(b);
                self.labels.pop();
                self.emit(I::End);
            }
        }
    }

    fn call(&mut self, e: &Expr, func: &Expr, args: &[Expr], kwargs: &[(String, Expr)]) {
        let line = e.line as i32;
        match &func.kind {
            ExprKind::Attr(recv, method) => {
                self.expr(recv);
                self.str_const(method);
                let l = self.new_local();
                self.emit(I::Call(rt("list_new")));
                self.emit(I::LocalSet(l));
                for a in args {
                    self.emit(I::LocalGet(l));
                    self.expr(a);
                    self.emit(I::Call(rt("list_push")));
                }
                self.emit(I::LocalGet(l));
                self.emit(I::I32Const(line));
                self.emit(I::Call(rt("method")));
            }
            ExprKind::Name(name) => {
                if let Some(&f) = self.m.funcs.get(name) {
                    for a in args {
                        self.expr(a);
                    }
                    self.emit(I::Call(f));
                    self.check_stop();
                    return;
                }
                let name = name.as_str();
                if let Some(q) = QUERIES.iter().position(|x| *x == name) {
                    self.emit(I::I32Const(q as i32));
                    self.args(args, kwargs);
                    self.emit(I::I32Const(line));
                    self.emit(I::Call(self.ext("cap.observe")));
                    self.check_stop();
                } else if let Some(b) = PURE_BUILTINS.iter().position(|x| *x == name) {
                    self.emit(I::I32Const(b as i32));
                    self.args(args, kwargs);
                    self.emit(I::I32Const(line));
                    self.emit(I::Call(rt("builtin")));
                } else if name == "move" {
                    // move(part, to) = pick(part), then place(part, to) if the pick worked.
                    // The pick also carries `to`, so a destination that cannot take the
                    // part is refused before the arm picks anything up.
                    let (part, to, r) = (self.new_local(), self.new_local(), self.new_local());
                    self.expr(&args[0]);
                    self.emit(I::LocalSet(part));
                    let dest = args
                        .get(1)
                        .or_else(|| kwargs.iter().find(|(k, _)| k == "to").map(|(_, v)| v));
                    match dest {
                        Some(d) => self.expr(d),
                        None => self.emit(I::I32Const(NONE)),
                    }
                    self.emit(I::LocalSet(to));
                    self.two_arg_list(part, to);
                    self.emit(I::Call(rt("kw_new")));
                    self.emit(I::I32Const(line));
                    self.emit(I::Call(self.ext("cap.pick")));
                    self.check_stop();
                    self.emit(I::LocalTee(r));
                    self.emit(I::Call(rt("truthy")));
                    self.emit(I::If(BlockType::Result(ValType::I32)));
                    self.labels.push(Label::Other);
                    self.two_arg_list(part, to);
                    self.emit(I::Call(rt("kw_new")));
                    self.emit(I::I32Const(line));
                    self.emit(I::Call(self.ext("cap.place")));
                    self.check_stop();
                    self.emit(I::Else);
                    self.emit(I::LocalGet(r));
                    self.labels.pop();
                    self.emit(I::End);
                } else {
                    let key = match name {
                        "pick" => "cap.pick",
                        "place" => "cap.place",
                        "ask" => "human.ask",
                        _ => "human.say", // say, print
                    };
                    self.args(args, kwargs);
                    self.emit(I::I32Const(line));
                    self.emit(I::Call(self.ext(key)));
                    self.check_stop();
                }
            }
            _ => self.emit(I::I32Const(NONE)),
        }
    }

    fn two_arg_list(&mut self, a: u32, b: u32) {
        let l = self.new_local();
        self.emit(I::Call(rt("list_new")));
        self.emit(I::LocalTee(l));
        self.emit(I::LocalGet(a));
        self.emit(I::Call(rt("list_push")));
        self.emit(I::LocalGet(l));
        self.emit(I::LocalGet(b));
        self.emit(I::Call(rt("list_push")));
        self.emit(I::LocalGet(l));
    }
}

fn uses(e: &Expr, out: &mut BTreeSet<&'static str>) {
    let mut visit = |x: &Expr| uses(x, out);
    match &e.kind {
        ExprKind::Call { func, args, kwargs } => {
            if let ExprKind::Name(n) = &func.kind {
                match n.as_str() {
                    "parts" | "places" | "count" | "holding" => {
                        out.insert("cap.observe");
                    }
                    "pick" => {
                        out.insert("cap.pick");
                    }
                    "place" => {
                        out.insert("cap.place");
                    }
                    "move" => {
                        out.insert("cap.pick");
                        out.insert("cap.place");
                    }
                    "ask" => {
                        out.insert("human.ask");
                    }
                    "say" | "print" => {
                        out.insert("human.say");
                    }
                    _ => {}
                }
            } else {
                uses(func, out);
            }
            for a in args {
                uses(a, out);
            }
            for (_, v) in kwargs {
                uses(v, out);
            }
        }
        ExprKind::List(items) => items.iter().for_each(&mut visit),
        ExprKind::ListComp {
            elt, iter, cond, ..
        } => {
            uses(elt, out);
            uses(iter, out);
            if let Some(c) = cond {
                uses(c, out);
            }
        }
        ExprKind::Attr(a, _) | ExprKind::Not(a) | ExprKind::Neg(a) => uses(a, out),
        ExprKind::Index(a, b)
        | ExprKind::Bin(_, a, b)
        | ExprKind::Cmp(_, a, b)
        | ExprKind::And(a, b)
        | ExprKind::Or(a, b) => {
            uses(a, out);
            uses(b, out);
        }
        ExprKind::IfExp(a, c, b) => {
            uses(a, out);
            uses(c, out);
            uses(b, out);
        }
        _ => {}
    }
}

fn stmt_uses(body: &[Stmt], out: &mut BTreeSet<&'static str>) {
    for s in body {
        match &s.kind {
            StmtKind::Assign(_, e)
            | StmtKind::AugAssign(_, _, e)
            | StmtKind::Expr(e)
            | StmtKind::Expect(e, _) => uses(e, out),
            StmtKind::Return(Some(e)) => uses(e, out),
            StmtKind::If(branches, orelse) => {
                for (c, b) in branches {
                    uses(c, out);
                    stmt_uses(b, out);
                }
                stmt_uses(orelse, out);
            }
            StmtKind::For(_, it, b) => {
                uses(it, out);
                stmt_uses(b, out);
            }
            _ => {}
        }
    }
}

/// The lowered module and the external functions it imports.
pub struct Lowered {
    pub wasm: Vec<u8>,
    pub imports: Vec<String>,
}

pub fn lower(p: &Program, source: &str) -> Lowered {
    // External imports actually used, in a fixed order.
    let mut used = BTreeSet::new();
    stmt_uses(&p.body, &mut used);
    for f in &p.funcs {
        stmt_uses(&f.body, &mut used);
    }
    let ext_used: Vec<(&str, &str, u32)> = EXT
        .iter()
        .copied()
        .filter(|(m, n, _)| used.contains(format!("{m}.{n}").as_str()))
        .collect();
    let nimports = RT.len() as u32 + ext_used.len() as u32;

    let mut top = BTreeSet::new();
    assigned_names(&p.body, &mut top);
    let mut m = Module_ {
        data: Data::default(),
        globals: top
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i as u32))
            .collect(),
        funcs: p
            .funcs
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.clone(), nimports + 1 + i as u32))
            .collect(),
        ext: BTreeMap::new(),
        prog: p,
    };
    for (i, (md, n, _)) in ext_used.iter().enumerate() {
        let key: &'static str = EXT
            .iter()
            .find(|(a, b, _)| a == md && b == n)
            .map(|(a, b, _)| match (*a, *b) {
                ("cap", "observe") => "cap.observe",
                ("cap", "pick") => "cap.pick",
                ("cap", "place") => "cap.place",
                ("human", "ask") => "human.ask",
                _ => "human.say",
            })
            .unwrap();
        m.ext.insert(key, RT.len() as u32 + i as u32);
    }

    // Function bodies: main, then each def.
    let mut bodies: Vec<(u32, u32, Vec<I<'static>>)> = vec![]; // (params, locals, code)
    {
        let mut f = Fn_ {
            m: &mut m,
            locals: BTreeMap::new(),
            nlocals: 1,
            code: vec![],
            labels: vec![],
            tmp: 0,
        };
        let body = f.m.prog.body.clone();
        f.block(&body);
        f.emit(I::I32Const(NONE));
        f.emit(I::End);
        bodies.push((0, f.nlocals, f.code));
    }
    for fd in &p.funcs {
        let mut names = BTreeSet::new();
        assigned_names(&fd.body, &mut names);
        let mut locals: BTreeMap<String, u32> = fd
            .params
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i as u32))
            .collect();
        let mut next = fd.params.len() as u32;
        for n in names {
            if let std::collections::btree_map::Entry::Vacant(e) = locals.entry(n) {
                e.insert(next);
                next += 1;
            }
        }
        let tmp = next;
        let mut f = Fn_ {
            m: &mut m,
            locals,
            nlocals: next + 1,
            code: vec![],
            labels: vec![],
            tmp,
        };
        f.block(&fd.body);
        f.emit(I::I32Const(NONE));
        f.emit(I::End);
        bodies.push((fd.params.len() as u32, f.nlocals, f.code));
    }

    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], [ValType::I32]);
    types.ty().function([ValType::I32; 2], [ValType::I32]);
    types.ty().function([ValType::I32; 3], [ValType::I32]);
    types.ty().function([ValType::I32; 4], [ValType::I32]);
    types.ty().function([], [ValType::I32]);
    types.ty().function([ValType::I32; 2], []);
    types.ty().function([ValType::I32; 4], []);
    types.ty().function([ValType::I32; 5], [ValType::I32]);
    // User function types: (i32 x n) -> i32, appended after the fixed ones.
    let mut fn_type: BTreeMap<u32, u32> = BTreeMap::new();
    let mut next_type = 8u32;
    for f in &p.funcs {
        let n = f.params.len() as u32;
        if let std::collections::btree_map::Entry::Vacant(e) = fn_type.entry(n) {
            types
                .ty()
                .function(vec![ValType::I32; n as usize], [ValType::I32]);
            e.insert(next_type);
            next_type += 1;
        }
    }

    let mut imports = ImportSection::new();
    for (n, t) in RT {
        imports.import("rt", n, EntityType::Function(t));
    }
    for (md, n, t) in &ext_used {
        imports.import(md, n, EntityType::Function(*t));
    }

    let mut functions = FunctionSection::new();
    functions.function(T_V_I); // main
    for f in &p.funcs {
        functions.function(fn_type[&(f.params.len() as u32)]);
    }
    functions.function(T_V_I); // run

    let pages = (m.data.bytes.len() as u64).div_ceil(65536).max(1);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut globals = GlobalSection::new();
    for _ in &m.globals {
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(NONE),
        );
    }

    let run_idx = nimports + 1 + p.funcs.len() as u32;
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, run_idx);

    let mut code = CodeSection::new();
    for (params, nlocals, body) in &bodies {
        let mut f = Function::new([(nlocals - params, ValType::I32)]);
        for i in body {
            f.instruction(i);
        }
        code.function(&f);
    }
    {
        let mut f = Function::new([]);
        f.instruction(&I::Call(nimports));
        f.instruction(&I::I32Const(0));
        f.instruction(&I::I32LtS);
        f.instruction(&I::End);
        code.function(&f);
    }

    let mut datas = DataSection::new();
    datas.active(0, &ConstExpr::i32_const(0), m.data.bytes.iter().copied());

    let meta = serde_json::json!({"abi": ABI, "source_sha256": crate::util::sha256_hex(source.as_bytes())}).to_string();
    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&globals)
        .section(&exports)
        .section(&code)
        .section(&datas)
        .section(&CustomSection {
            name: "revolve.script".into(),
            data: meta.as_bytes().into(),
        });
    let _ = builtin; // arity was checked before lowering
    Lowered {
        wasm: module.finish(),
        imports: ext_used
            .iter()
            .map(|(md, n, _)| format!("{md}.{n}"))
            .collect(),
    }
}
