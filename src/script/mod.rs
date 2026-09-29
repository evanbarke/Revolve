//! Revolve Script: the language AI models write robot programs in.
//!
//! Python-shaped (models already write Python fluently), restricted so programs
//! can be checked before anything moves and always finish:
//! - `parse` → `check` (names, types, literal places and kinds against the observed
//!   workspace, policy, no recursion, stated success criteria);
//! - `lower` → a WebAssembly module (native code via Cranelift);
//! - `host` runs it under the durable executor, one outside-world call at a time,
//!   with deterministic replay after any interruption.
//!
//! See `docs/language.md` (the same guide is given to the AI).

pub mod check;
pub mod codegen;
pub mod host;
pub mod syntax;

use serde::Serialize;
use std::collections::BTreeSet;

pub use check::CheckEnv;

/// `range()` is limited so every loop is small and finite.
pub const MAX_RANGE: i64 = 100;
/// Longest program accepted (bytes).
pub const MAX_SOURCE: usize = 20_000;
pub const LANGUAGE: &str = "revolve-script/1";

/// A problem found before anything ran, with its position in the source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Diag {
    pub line: u32,
    pub col: u32,
    pub message: String,
}

impl std::fmt::Display for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// What the compiler established about a program, shown to the person.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Facts {
    /// Kinds of outside-world calls used: look, pick, place, move, ask, say.
    pub effects: BTreeSet<String>,
    /// Destinations written literally in the program.
    pub literal_destinations: BTreeSet<String>,
    /// Some destinations are computed while running (each is checked before the move).
    pub dynamic_destinations: bool,
    /// Questions the program may ask (literal text).
    pub questions: Vec<String>,
    /// How success is checked (descriptions of the `expect` statements).
    pub expectations: Vec<String>,
    pub loops: u32,
    pub functions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Compiled {
    #[serde(skip)]
    pub wasm: Vec<u8>,
    pub wasm_bytes: usize,
    pub sha256: String,
    /// External functions the module imports (least authority, visible in the binary).
    pub imports: Vec<String>,
    pub facts: Facts,
    pub lines: usize,
}

/// Parse, check and compile a program.
pub fn compile(source: &str, env: &CheckEnv) -> Result<Compiled, Vec<Diag>> {
    if source.len() > MAX_SOURCE {
        return Err(vec![Diag {
            line: 1,
            col: 1,
            message: format!("the program is too long (limit {MAX_SOURCE} characters)"),
        }]);
    }
    let prog = syntax::parse(source).map_err(|d| vec![d])?;
    let facts = check::check(&prog, env)?;
    let lowered = codegen::lower(&prog, source);
    Ok(Compiled {
        sha256: crate::util::sha256_hex(&lowered.wasm),
        wasm_bytes: lowered.wasm.len(),
        wasm: lowered.wasm,
        imports: lowered.imports,
        facts,
        lines: source.lines().filter(|l| !l.trim().is_empty()).count(),
    })
}

/// The language guide given to the AI (and published as docs/language.md).
pub const GUIDE: &str = include_str!("../../docs/language.md");

#[cfg(test)]
mod tests;
