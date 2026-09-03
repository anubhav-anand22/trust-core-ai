//! Turn planning: intent parsing, plan generation, deterministic validation.

pub mod planner;
pub mod registry;

pub use planner::{plan_turn, PlanOutcome, PlanSource, MAX_PLAN_ATTEMPTS};
pub use registry::{lookup, validate_plan, Stage, TaskSpec, TASK_REGISTRY};
