//! `summarize` and `compare_to_sop` — the analysis-stage steps.
//!
//! These consume the evidence produced by earlier extraction/retrieval steps and
//! run it through the *resident* model (no extra weights). They exist as explicit
//! plan steps so the DAG is meaningful and the audit trail shows the reasoning
//! step, rather than folding all analysis silently into the final compiler.

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::Result;

/// One tool that serves both analysis task names, distinguished at construction.
pub struct AnalysisTool {
    task: &'static str,
    instruction: &'static str,
}

impl AnalysisTool {
    pub fn summarize() -> Self {
        Self {
            task: "summarize",
            instruction: "Summarise the key findings from the evidence below into a short list. \
                          Keep equipment tags, measurements and dates exact.",
        }
    }

    pub fn compare_to_sop() -> Self {
        Self {
            task: "compare_to_sop",
            instruction: "Compare the observed conditions against the SOP / procedure passages in \
                          the evidence. List each point as COMPLIANT or DEVIATION with a one-line \
                          reason and the SOP source name.",
        }
    }
}

#[async_trait::async_trait]
impl Tool for AnalysisTool {
    fn name(&self) -> &'static str {
        self.task
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        // Prefer this step's declared dependencies; fall back to every output so
        // far so a planner that under-specifies `depends_on` still gets context.
        let mut evidence = collect(step.depends_on.iter().filter_map(|id| ctx.outputs.get(id)));
        if evidence.trim().is_empty() {
            evidence = collect(ctx.outputs.values());
        }
        if evidence.trim().is_empty() {
            return Ok(ToolResult::failure(
                &step.id,
                self.name(),
                "no evidence available from earlier steps",
                started.elapsed().as_millis(),
            ));
        }

        match ctx.engine.analyze(self.instruction, &evidence, ctx.prompt).await {
            Ok(text) => Ok(ToolResult {
                step_id: step.id.clone(),
                task: self.name().into(),
                ok: true,
                data: serde_json::json!({ "text": text }),
                error: None,
                warning: None,
                elapsed_ms: started.elapsed().as_millis(),
            }),
            Err(e) => Ok(ToolResult::failure(
                &step.id,
                self.name(),
                e.to_string(),
                started.elapsed().as_millis(),
            )),
        }
    }
}

/// Render a set of prior step outputs into a compact text block for the prompt.
fn collect<'a>(values: impl Iterator<Item = &'a serde_json::Value>) -> String {
    values
        .map(|v| {
            // Pull the human-relevant field if there is one, else dump the JSON.
            if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                t.to_string()
            } else if let Some(o) = v.get("observation").and_then(|o| o.as_str()) {
                o.to_string()
            } else if let Some(chunks) = v.get("chunks").and_then(|c| c.as_array()) {
                chunks
                    .iter()
                    .filter_map(|c| {
                        let text = c.get("text")?.as_str()?;
                        let src = c.get("source").and_then(|s| s.as_str()).unwrap_or("kb");
                        Some(format!("[{src}] {text}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                serde_json::to_string_pretty(v).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
