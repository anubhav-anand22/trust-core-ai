//! The typed contract between the LLM, the validator, the executor and the UI.
//!
//! Every structure the model is asked to produce has a `serde` type here. Ollama
//! is called with `format: json`, and we deserialise into these types — the Rust
//! equivalent of the blueprint's "enforce typed JSON responses using Pydantic".
//! A malformed response is a [`CoreError::Schema`](crate::CoreError::Schema),
//! which is exactly what triggers a planner retry.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// File kinds the pipeline understands.
///
/// Detected from the upload's extension by the host and echoed by role A; matched
/// against [`TaskSpec::requires_file`](crate::planner::registry::TaskSpec) during
/// validation so a plan can never schedule `ocr_image` with no image attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Audio,
    Pdf,
    Image,
    Unknown,
}

impl FileKind {
    /// Classify by file extension. Anything unrecognised is [`FileKind::Unknown`]
    /// and will be rejected by validation rather than silently guessed at.
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "wav" | "mp3" | "m4a" | "flac" | "ogg" | "aac" | "wma" => Self::Audio,
            "pdf" => Self::Pdf,
            "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp" => Self::Image,
            _ => Self::Unknown,
        }
    }

    pub fn from_path(path: &std::path::Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map(Self::from_extension)
            .unwrap_or(Self::Unknown)
    }
}

/// One attached file, already written to `data/uploads/<session>/`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputFile {
    /// Absolute path on disk. Validation asserts this actually exists.
    pub path: String,
    pub kind: FileKind,
    /// Name as the user supplied it, for display and citations.
    pub original_name: String,
}

/// Output of **role A — Intent Parser**.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct IntentResult {
    /// Distinct user goals, one per ask.
    pub intents: Vec<String>,
    /// File kinds the model believes are in play.
    #[serde(default)]
    pub file_kinds: Vec<FileKind>,
    /// Whether the SOP / manual knowledge base should be consulted.
    #[serde(default)]
    pub needs_knowledge: bool,
}

/// One node of the plan. `depends_on` names other [`TaskStep::id`]s, forming a DAG
/// that [`validate_plan`](crate::planner::registry::validate_plan) checks.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskStep {
    pub id: String,
    /// Must be a name present in
    /// [`TASK_REGISTRY`](crate::planner::registry::TASK_REGISTRY).
    pub task: String,
    /// Free-form per-task arguments (e.g. `{"query": "...", "file": "..."}`).
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Output of **role B — Task Planner**: an ordered, executable sequence.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub steps: Vec<TaskStep>,
}

/// Result of running one tool. Always produced, even on failure, so the audit
/// sidebar and the quality check see the complete picture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    /// Step id this result belongs to.
    pub step_id: String,
    pub task: String,
    pub ok: bool,
    /// Tool-specific payload: extracted text, table rows, observations, chunks…
    pub data: serde_json::Value,
    /// Populated when `ok == false`.
    pub error: Option<String>,
    /// Non-fatal note, e.g. "scanned PDF: no extractable text".
    pub warning: Option<String>,
    pub elapsed_ms: u128,
}

impl ToolResult {
    pub fn failure(step_id: &str, task: &str, message: impl Into<String>, elapsed_ms: u128) -> Self {
        Self {
            step_id: step_id.to_string(),
            task: task.to_string(),
            ok: false,
            data: serde_json::Value::Null,
            error: Some(message.into()),
            warning: None,
            elapsed_ms,
        }
    }
}

/// One rule-based assertion from the quality gate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Verdict over all tool results and the compiled report. Rule-based only — the
/// blueprint explicitly forbids an LLM self-evaluation loop here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityReport {
    pub passed: bool,
    pub checks: Vec<QualityCheck>,
}

/// Output of **role C — Output Compiler**: what the user actually reads.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinalReport {
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
    /// Source names pulled from the knowledge base, so claims are traceable.
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub safety_notes: Vec<String>,
    /// Set when a tool failed or a quality check tripped.
    #[serde(default)]
    pub degraded: bool,
}
