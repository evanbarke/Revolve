//! Revolve: an explicit-intent execution runtime prototype.
//!
//! Human goal -> planner proposal (Action IR) -> compiler/validator + policy ->
//! recoverable sequential executor -> adapters (simulated workspace, inventory)
//! -> verifier. See docs/architecture.md.

pub mod baseline;
pub mod bookkeeping;
pub mod capability;
pub mod compiler;
pub mod eval;
pub mod gateway;
pub mod harness;
pub mod inventory;
pub mod ir;
pub mod model;
pub mod planner;
pub mod policy;
pub mod runtime;
pub mod scenario;
pub mod server;
pub mod sim;
pub mod store;
pub mod util;
pub mod verifier;
