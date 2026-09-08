//! `ocr_image` — read printed / stamped text off an industrial photo.
//!
//! Pure-Rust OCR via `ocrs` (built on the `rten` inference engine). No C library,
//! no external binary. Needs two `.rten` model files under `data/models/`; Phase 5
//! bundles them. If they're absent the tool degrades gracefully.

use std::path::Path;

use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;

use crate::engine::schemas::{FileKind, TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// `ocr_image` tool.
pub struct OcrTool;

#[async_trait::async_trait]
impl Tool for OcrTool {
    fn name(&self) -> &'static str {
        "ocr_image"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        let file = match ctx.file_for(step, FileKind::Image) {
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

        let det = ctx.config.ocr_detection_model.clone();
        let rec = ctx.config.ocr_recognition_model.clone();
        if !det.is_file() || !rec.is_file() {
            return Ok(ToolResult::failure(
                &step.id,
                self.name(),
                "OCR models not installed (expected text-detection.rten and \
                 text-recognition.rten under data/models/)",
                started.elapsed().as_millis(),
            ));
        }

        let path = file.path.clone();
        // rten inference is CPU-bound and synchronous.
        let outcome = tokio::task::spawn_blocking(move || run_ocr(&det, &rec, &path))
            .await
            .map_err(|e| CoreError::Ocr(e.to_string()))?;

        match outcome {
            Ok(text) => {
                let empty = text.trim().is_empty();
                Ok(ToolResult {
                    step_id: step.id.clone(),
                    task: self.name().into(),
                    ok: true,
                    data: serde_json::json!({ "text": text, "source": file.original_name }),
                    error: None,
                    warning: empty.then(|| "no text detected in image".to_string()),
                    elapsed_ms: started.elapsed().as_millis(),
                })
            }
            Err(e) => Ok(ToolResult::failure(
                &step.id,
                self.name(),
                e.to_string(),
                started.elapsed().as_millis(),
            )),
        }
    }
}

/// Load the two models, run detect → line-group → recognise, and join the lines.
fn run_ocr(det_path: &Path, rec_path: &Path, image_path: &str) -> Result<String> {
    let detection = Model::load_file(det_path)
        .map_err(|e| CoreError::Ocr(format!("load detection model: {e}")))?;
    let recognition = Model::load_file(rec_path)
        .map_err(|e| CoreError::Ocr(format!("load recognition model: {e}")))?;

    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection),
        recognition_model: Some(recognition),
        ..Default::default()
    })
    .map_err(|e| CoreError::Ocr(e.to_string()))?;

    let img = image::open(image_path)
        .map_err(|e| CoreError::Ocr(format!("open image: {e}")))?
        .into_rgb8();
    let source = ImageSource::from_bytes(img.as_raw(), img.dimensions())
        .map_err(|e| CoreError::Ocr(e.to_string()))?;

    let input = engine
        .prepare_input(source)
        .map_err(|e| CoreError::Ocr(e.to_string()))?;
    let word_rects = engine
        .detect_words(&input)
        .map_err(|e| CoreError::Ocr(e.to_string()))?;
    let line_rects = engine.find_text_lines(&input, &word_rects);
    let lines = engine
        .recognize_text(&input, &line_rects)
        .map_err(|e| CoreError::Ocr(e.to_string()))?;

    let text = lines
        .iter()
        .flatten()
        .map(|line| line.to_string())
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(text)
}
