//! Sequential execution, rule-based quality gate, and report synthesis.

pub mod compiler;
pub mod executor;
pub mod quality;

pub use compiler::compile;
pub use executor::{execute_plan, ToolRegistry};
pub use quality::assert_sane;
