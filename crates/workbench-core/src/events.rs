//! Progress events for the front-end execution stepper.
//!
//! The blueprint asks for enterprise explainability: the user should always see
//! which stage the pipeline is in and how long each tool took. Every checkpoint
//! is a [`StepEvent`], serialised straight to the React stepper by the Tauri host.

use serde::{Deserialize, Serialize};

/// One observable checkpoint in a run. Variants map 1:1 to stepper UI states:
/// `Idle → ParsingContext → ValidatingPlan → ExecutingTool* → QualityCheck →
/// Synthesizing → Done` (with `AwaitingUser` and `Error` as off-ramps).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum StepEvent {
    Idle,

    /// Role A is extracting intent and file kinds.
    ParsingContext { attempt: u32 },

    /// Deterministic validation of the plan role B produced.
    ValidatingPlan { attempt: u32 },

    /// Planner exhausted `MAX_PLAN_ATTEMPTS`. The run is parked; the UI shows the
    /// errors plus the last (invalid) plan so the user can edit or approve it.
    AwaitingUser {
        errors: Vec<String>,
        plan_json: String,
    },

    /// A tool is about to run. `index`/`total` drive the stepper's progress text.
    ExecutingTool {
        tool: String,
        index: usize,
        total: usize,
    },

    /// A tool finished. `elapsed_ms` feeds the audit sidebar.
    ToolFinished {
        tool: String,
        ok: bool,
        elapsed_ms: u128,
    },

    /// Rule-based sanity assertions completed.
    QualityCheck { passed: bool },

    /// Role C is synthesising the final report.
    Synthesizing,

    /// Terminal success. `report_json` is a serialised
    /// [`FinalReport`](crate::engine::schemas::FinalReport).
    Done { report_json: String },

    /// Terminal failure.
    Error { message: String },
}

/// Where the pipeline pushes [`StepEvent`]s.
///
/// The Tauri host implements this over a `tauri::ipc::Channel`; tests use
/// [`VecSink`]. Kept object-safe (`&dyn ProgressSink`) so the pipeline never has
/// to know which transport it is talking to.
pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: StepEvent);
}

/// Discards every event — for code paths and tests that don't observe progress.
pub struct NullSink;

impl ProgressSink for NullSink {
    fn emit(&self, _event: StepEvent) {}
}

/// Collects events in order. Used by tests to assert the stepper sequence.
#[derive(Default)]
pub struct VecSink(std::sync::Mutex<Vec<StepEvent>>);

impl VecSink {
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    /// Snapshot of everything emitted so far.
    pub fn events(&self) -> Vec<StepEvent> {
        self.0.lock().expect("event sink poisoned").clone()
    }

    /// Stage tags in order, e.g. `["parsing_context", "validating_plan", "done"]`.
    /// Convenient for asserting flow without matching on payloads.
    pub fn stages(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter_map(|e| {
                serde_json::to_value(e)
                    .ok()?
                    .get("stage")?
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }
}

impl ProgressSink for VecSink {
    fn emit(&self, event: StepEvent) {
        self.0.lock().expect("event sink poisoned").push(event);
    }
}
