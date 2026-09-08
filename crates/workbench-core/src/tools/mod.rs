//! Tool modules. Each is a small, independently testable unit that turns one
//! step's arguments into a [`ToolResult`].
//!
//! The trait and context below are the contract every tool implements.
//! [`default_registry`] wires the concrete set the executor runs.

pub mod analysis;
pub mod audio;
pub mod document;
pub mod followup;
pub mod ocr;
pub mod rag;
pub mod vision;

use std::collections::HashMap;

use crate::executor::ToolRegistry;

use crate::engine::schemas::{InputFile, TaskStep, ToolResult};
use crate::engine::OllamaEngine;
use crate::events::ProgressSink;
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
    /// The session's rolling context (`SessionContext::context_blob`). Empty on
    /// the first turn. `answer_followup` answers straight from this.
    pub session_blob: &'a str,
    /// Shared Ollama engine — the vision tool and the RAG embedder call the
    /// specialist models (`moondream`, `nomic-embed-text`) through it with
    /// `keep_alive = 0`, so no second model ever co-resides with the resident one.
    pub engine: &'a OllamaEngine,
    /// Progress sink, so a long-running tool (deep-read analysis over many
    /// document windows) can report per-window progress instead of looking hung.
    pub sink: &'a dyn ProgressSink,
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

    /// The file this step should operate on.
    ///
    /// Resolves `step.args["file"]` (the planner names an attachment by its
    /// `original_name`) against the uploads; falls back to the first file of
    /// `kind` when the plan does not name one — so a single-attachment turn still
    /// works even if the planner leaves `args` empty.
    ///
    /// Matching is by **name, never path**: a planner that hallucinates
    /// `/etc/passwd` gets `None`, not an arbitrary file read.
    pub fn file_for(
        &self,
        step: &TaskStep,
        kind: crate::engine::schemas::FileKind,
    ) -> Option<&'a InputFile> {
        if let Some(want) = step.args.get("file").and_then(|v| v.as_str()) {
            let want = want.trim();
            if !want.is_empty() {
                if let Some(f) = self
                    .uploads
                    .iter()
                    .find(|f| f.original_name == want && f.kind == kind)
                {
                    return Some(f);
                }
                // Named a file, but not one we have of this kind. Do NOT silently
                // fall through to some other file — that is how three `ocr_image`
                // steps all ended up reading image #1.
                tracing::warn!(
                    requested = want,
                    ?kind,
                    "file_for: step names an attachment that is not present as that kind"
                );
                return None;
            }
        }
        self.first_file_of(kind)
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
        .register(Box::new(followup::AnswerFollowupTool))
        .register(Box::new(analysis::AnalysisTool::summarize()))
        .register(Box::new(analysis::AnalysisTool::compare_to_sop()));
    registry
}

#[cfg(test)]
mod file_for_tests {
    use super::*;
    use crate::engine::schemas::{FileKind, InputFile, TaskStep};
    use crate::engine::OllamaEngine;
    use crate::events::NullSink;
    use std::collections::HashMap;

    fn img(name: &str) -> InputFile {
        InputFile {
            path: format!("/uploads/{name}"),
            kind: FileKind::Image,
            original_name: name.into(),
        }
    }

    fn step_with_file(file: Option<&str>) -> TaskStep {
        TaskStep {
            id: "s1".into(),
            task: "analyze_image".into(),
            args: match file {
                Some(f) => serde_json::json!({ "file": f }),
                None => serde_json::Value::Object(Default::default()),
            },
            depends_on: vec![],
        }
    }

    fn ctx<'a>(
        uploads: &'a [InputFile],
        outputs: &'a HashMap<String, serde_json::Value>,
        engine: &'a OllamaEngine,
    ) -> ToolContext<'a> {
        ToolContext {
            config: Box::leak(Box::new(crate::PipelineConfig {
                data_dir: ".".into(),
                kb_dir: "kb".into(),
                ollama_url: "http://127.0.0.1:11434".into(),
                llm_model: "m".into(),
                vision_model: "v".into(),
                embed_model: "e".into(),
                whisper_model_path: "w".into(),
                ocr_detection_model: "d".into(),
                ocr_recognition_model: "r".into(),
            })),
            uploads,
            outputs,
            prompt: "look at the photos",
            session_blob: "",
            engine,
            sink: &NullSink,
        }
    }

    #[test]
    fn named_file_wins_over_first_of_kind() {
        let engine = OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c");
        let outputs = HashMap::new();
        let uploads = vec![img("north.jpg"), img("south.jpg"), img("nameplate.jpg")];
        let c = ctx(&uploads, &outputs, &engine);

        let got = c
            .file_for(&step_with_file(Some("south.jpg")), FileKind::Image)
            .unwrap();
        assert_eq!(got.original_name, "south.jpg", "should honour args.file");
    }

    #[test]
    fn empty_args_falls_back_to_first_of_kind() {
        let engine = OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c");
        let outputs = HashMap::new();
        let uploads = vec![img("north.jpg"), img("south.jpg")];
        let c = ctx(&uploads, &outputs, &engine);

        let got = c.file_for(&step_with_file(None), FileKind::Image).unwrap();
        assert_eq!(got.original_name, "north.jpg");
    }

    #[test]
    fn a_hallucinated_name_resolves_to_nothing_not_an_arbitrary_file() {
        let engine = OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c");
        let outputs = HashMap::new();
        let uploads = vec![img("north.jpg")];
        let c = ctx(&uploads, &outputs, &engine);

        assert!(
            c.file_for(&step_with_file(Some("/etc/passwd")), FileKind::Image)
                .is_none(),
            "a name we do not have must not fall through to some other file"
        );
    }
}
