//! Tool modules. Each is a small, independently testable unit that turns one
//! step's arguments into a [`ToolResult`].
//!
//! The trait and context below are the contract every tool implements.
//! [`default_registry`] wires the concrete set the executor runs.

pub mod analysis;
pub mod audio;
pub mod document;
pub mod ocr;
pub mod rag;
pub mod vision;

use std::collections::HashMap;

use crate::executor::ToolRegistry;

use crate::engine::schemas::{InputFile, TaskStep, ToolResult};
use crate::engine::OllamaEngine;
use crate::{PipelineConfig, Result};

/// Everything a tool may read while running.
///
/// `outputs` holds the [`ToolResult::data`] of every previously completed step,
/// keyed by step id — this is how a `summarize` step reads what `parse_pdf`
/// produced without the tools knowing about each other.
pub struct ToolContext<'a> {
    pub config: &'a PipelineConfig,
    pub uploads: &'a [InputFile],
    pub outputs: &'a HashMap<String, serde_json::Value>,
    /// The original user prompt, for tools that need the ask verbatim
    /// (e.g. the vision tool's question, the RAG query fallback).
    pub prompt: &'a str,
    /// Shared Ollama engine — the vision tool and the RAG embedder call the
    /// specialist models (`moondream`, `nomic-embed-text`) through it with
    /// `keep_alive = 0`, so no second model ever co-resides with the resident one.
    pub engine: &'a OllamaEngine,
}

impl<'a> ToolContext<'a> {
    /// First attached file of a given kind, or `None`.
    ///
    /// Tools use this rather than trusting a path in `args`, so a planner that
    /// invents a filename cannot make us read an arbitrary location.
    pub fn first_file_of(
        &self,
        kind: crate::engine::schemas::FileKind,
    ) -> Option<&'a InputFile> {
        self.uploads.iter().find(|f| f.kind == kind)
    }

    /// All attached files of a given kind.
    pub fn files_of(
        &self,
        kind: crate::engine::schemas::FileKind,
    ) -> Vec<&'a InputFile> {
        self.uploads.iter().filter(|f| f.kind == kind).collect()
    }
}

/// One executable capability.
///
/// Implementations must be side-effect free apart from their own scratch files,
/// and must return within a bounded time — the executor runs them strictly one at
/// a time so a hung tool stalls the whole turn.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Registry name; must match a [`TaskSpec::name`](crate::planner::TaskSpec).
    fn name(&self) -> &'static str;

    /// Run the step. Returning `Err` is reserved for programmer errors; expected
    /// failures (bad file, empty result) should come back as a
    /// [`ToolResult`] with `ok: false` so the run can continue degraded.
    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult>;
}

/// The concrete tool set the pipeline runs. One entry per non-analysis task in
/// [`TASK_REGISTRY`](crate::planner::registry::TASK_REGISTRY), plus the two
/// analysis tasks served by [`analysis::AnalysisTool`].
pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(audio::AudioTool))
        .register(Box::new(document::DocumentTool))
        .register(Box::new(ocr::OcrTool))
        .register(Box::new(vision::VisionTool))
        .register(Box::new(rag::RagTool))
        .register(Box::new(analysis::AnalysisTool::summarize()))
        .register(Box::new(analysis::AnalysisTool::compare_to_sop()));
    registry
}
