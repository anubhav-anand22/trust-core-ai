//! `summarize` and `compare_to_sop` — the analysis-stage steps.
//!
//! These consume the evidence produced by earlier extraction/retrieval steps and
//! run it through the *resident* model (no extra weights). They exist as explicit
//! plan steps so the DAG is meaningful and the audit trail shows the reasoning
//! step, rather than folding all analysis silently into the final compiler.

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::Result;

/// How many retrieved passages to put in front of the model.
const EVIDENCE_TOP_K: usize = 8;

/// Characters of evidence to allow, derived from the context window.
///
/// Roughly 4 characters per token; spend about half the window on evidence and
/// leave the rest for the system prompt, the question and the answer.
fn evidence_budget_chars(num_ctx: u64) -> usize {
    ((num_ctx as usize) * 4) / 2
}

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

        // An attachment is routinely longer than the context window. Sending it
        // whole means the model silently sees only the head of it — the reason a
        // rate buried on a late page reads back as "no relevant information" — and
        // makes prefill crawl. Keep the passages that bear on the user's question.
        let evidence = crate::tools::rag::select_relevant(
            ctx.engine,
            ctx.prompt,
            &evidence,
            EVIDENCE_TOP_K,
            evidence_budget_chars(ctx.engine.limits().num_ctx),
        )
        .await;

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
