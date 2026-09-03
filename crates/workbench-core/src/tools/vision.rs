//! `analyze_image` — visual condition assessment of an inspection photo.
//!
//! Runs the compact `moondream` VLM through Ollama with `keep_alive = 0`
//! ([`OllamaEngine::analyze_image`]). Reserved for *physical* inspection
//! (corrosion, leaks, damage, gauge readings) — text on labels goes through
//! `ocr_image` instead.

use base64::Engine as _;

use crate::engine::schemas::{FileKind, TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// Default question if the planner didn't supply one in `args`.
const INSPECTION_QUESTION: &str = "You are inspecting industrial refinery equipment. \
Describe, factually and briefly, any visible signs of: corrosion or rust, leaks, wet \
staining or product tracks, cracks or mechanical damage, missing or damaged insulation, \
and the state or reading of any visible gauges and labels. Do not mention anything that \
is not clearly visible.";

/// `analyze_image` tool.
pub struct VisionTool;

#[async_trait::async_trait]
impl Tool for VisionTool {
    fn name(&self) -> &'static str {
        "analyze_image"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        let file = match ctx.first_file_of(FileKind::Image) {
            Some(f) => f,
            None => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    "no image file attached",
                    started.elapsed().as_millis(),
                ))
            }
        };

        let bytes = std::fs::read(&file.path).map_err(|e| CoreError::Tool {
            tool: self.name().into(),
            message: format!("read image: {e}"),
        })?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let question = step
            .args
            .get("question")
            .and_then(|q| q.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(INSPECTION_QUESTION)
            .to_string();

        let observation = match ctx.engine.analyze_image(&b64, &question).await {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    e.to_string(),
                    started.elapsed().as_millis(),
                ))
            }
        };

        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name().into(),
            ok: true,
            data: serde_json::json!({
                "observation": observation,
                "source": file.original_name,
            }),
            error: None,
            warning: if observation.trim().is_empty() {
                Some("vision model returned no description".into())
            } else {
                None
            },
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}
