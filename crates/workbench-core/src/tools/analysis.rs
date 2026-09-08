//! `summarize` and `compare_to_sop` — the analysis-stage steps.
//!
//! These consume the evidence produced by earlier extraction/retrieval steps and
//! run it through the *resident* model (no extra weights). They exist as explicit
//! plan steps so the DAG is meaningful and the audit trail shows the reasoning
//! step, rather than folding all analysis silently into the final compiler.

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::events::StepEvent;
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result, TurnMode};

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

        let budget = evidence_budget_chars(ctx.engine.limits().num_ctx);

        // Deep mode: when the evidence is bigger than one context window, read the
        // *whole* thing in passes rather than retrieving a slice of it. Costs one
        // model call per window, so it is opt-in.
        if ctx.engine.mode() == TurnMode::Deep && evidence.chars().count() > budget {
            return self.deep_read(step, ctx, &evidence, budget, started).await;
        }

        // Fast mode: an attachment is routinely longer than the context window.
        // Sending it whole means the model silently sees only the head — the
        // reason a rate on a late page reads back as "no relevant information" —
        // and makes prefill crawl. Keep the passages that bear on the question.
        let evidence = crate::tools::rag::select_relevant(
            ctx.engine,
            ctx.prompt,
            &evidence,
            EVIDENCE_TOP_K,
            budget,
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

impl AnalysisTool {
    /// Deep read: map the instruction over every window of the document, then
    /// reduce the partial answers into one. Nothing is skipped.
    ///
    /// Each window emits an `ExecutingTool` progress event so a multi-minute pass
    /// on CPU shows movement instead of looking hung, and the cancel flag is
    /// checked between windows so **Stop** still works.
    async fn deep_read(
        &self,
        step: &TaskStep,
        ctx: &ToolContext<'_>,
        evidence: &str,
        budget: usize,
        started: std::time::Instant,
    ) -> Result<ToolResult> {
        // ~80% of the evidence budget per window: room for the window plus the
        // system prompt and the question.
        let window_chars = (budget * 4 / 5).max(2_000);
        let windows = crate::tools::rag::windows(evidence, window_chars);
        let n = windows.len();
        tracing::info!(
            task = self.name(),
            windows = n,
            window_chars,
            evidence_chars = evidence.chars().count(),
            "deep read: mapping instruction over the whole document"
        );

        let mut partials: Vec<String> = Vec::with_capacity(n);
        for (i, w) in windows.iter().enumerate() {
            ctx.engine.cancel_check()?;
            ctx.sink.emit(StepEvent::ExecutingTool {
                tool: format!("{} · deep read {}/{}", self.name(), i + 1, n),
                index: i,
                total: n,
            });
            match ctx.engine.analyze(self.instruction, w, ctx.prompt).await {
                Ok(a) if !a.trim().is_empty() => partials.push(a),
                Ok(_) => {}
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => crate::events::warn(
                    ctx.sink,
                    format!("Deep read: window {}/{} failed, continuing", i + 1, n),
                    Some(e.to_string()),
                ),
            }
        }

        if partials.is_empty() {
            return Ok(ToolResult::failure(
                &step.id,
                self.name(),
                "deep read produced nothing from any window",
                started.elapsed().as_millis(),
            ));
        }

        // Reduce. If there was only one window, its answer *is* the result.
        let text = if partials.len() == 1 {
            partials.pop().unwrap()
        } else {
            let combined = partials.join("\n\n---\n\n");
            let reduce = "You are given several partial analyses of ONE document, in order. \
                          Merge them into a single answer. Keep every specific figure, rate, \
                          percentage, equipment tag, date and source name exactly as written. \
                          Do not introduce anything that is not in a partial.";
            match ctx.engine.analyze(reduce, &combined, ctx.prompt).await {
                Ok(t) => t,
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        &step.id,
                        self.name(),
                        format!("deep read reduce step failed: {e}"),
                        started.elapsed().as_millis(),
                    ))
                }
            }
        };

        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name().into(),
            ok: true,
            data: serde_json::json!({ "text": text, "mode": "deep", "windows": n }),
            error: None,
            warning: (n > 1).then(|| format!("deep read: {n} passes over the document")),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}

/// Render a set of prior step outputs into a compact text block for the prompt.
///
/// Two things this has to get right, both learned the hard way:
///   * **Tables must survive.** `parse_pdf` puts a flat rendering in
///     `tables_text`; this used to return `text` and stop, so every table — and
///     any figure inside one — was silently dropped before the model saw it.
///   * **Keep the source.** Prefixing each block with its file name is what lets
///     the final report attribute a finding to a specific attachment.
fn collect<'a>(values: impl Iterator<Item = &'a serde_json::Value>) -> String {
    /// Per-source cap. With several attachments, one verbose PDF would otherwise
    /// fill the whole evidence blob and the ranking never sees the audio or the
    /// photos. `select_relevant` narrows further; this just keeps the input fair.
    const MAX_PER_SOURCE_CHARS: usize = 12_000;

    let cap = |mut s: String| {
        if s.chars().count() > MAX_PER_SOURCE_CHARS {
            s = s.chars().take(MAX_PER_SOURCE_CHARS).collect::<String>() + " …[truncated]";
        }
        s
    };

    values
        .map(|v| {
            let src = v.get("source").and_then(|s| s.as_str());
            let prefix = |body: String| {
                let body = cap(body);
                match src {
                    Some(name) if !name.is_empty() => format!("[{name}]\n{body}"),
                    _ => body,
                }
            };

            // KB retrieval: a list of chunks, each already carrying its source.
            if let Some(chunks) = v.get("chunks").and_then(|c| c.as_array()) {
                return cap(
                    chunks
                        .iter()
                        .filter_map(|c| {
                            let text = c.get("text")?.as_str()?;
                            let s = c.get("source").and_then(|s| s.as_str()).unwrap_or("kb");
                            Some(format!("[{s}] {text}"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }

            // Document / OCR / transcription: prose, plus a PDF's tables.
            let mut parts: Vec<String> = Vec::new();
            if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    parts.push(t.to_string());
                }
            }
            if let Some(tbl) = v.get("tables_text").and_then(|t| t.as_str()) {
                if !tbl.trim().is_empty() {
                    parts.push(format!("TABLES:\n{tbl}"));
                }
            }
            if !parts.is_empty() {
                return prefix(parts.join("\n\n"));
            }

            if let Some(o) = v.get("observation").and_then(|o| o.as_str()) {
                return prefix(o.to_string());
            }

            cap(serde_json::to_string_pretty(v).unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::collect;
    use serde_json::json;

    /// The core Stage-2 fix: a PDF's tables must reach the evidence blob. `collect`
    /// used to return `data["text"]` and stop, dropping `tables_text` — and any
    /// fee inside it.
    #[test]
    fn collect_includes_pdf_tables_and_the_source() {
        let v = json!({
            "text": "PayU MDR schedule, effective 2026-04-01.",
            "tables_text": "[table 1]\nMode | MDR\nMode | MDR  \u{25b8}  Credit card | 2.00%\n",
            "source": "fee_card.pdf",
        });
        let out = collect([v].iter());
        assert!(out.contains("2.00%"), "table figure missing: {out}");
        assert!(out.contains("[fee_card.pdf]"), "source prefix missing: {out}");
        assert!(out.contains("TABLES:"), "tables section missing: {out}");
    }

    #[test]
    fn collect_still_handles_plain_text_and_observations() {
        let text = collect([json!({"text": "just prose"})].iter());
        assert_eq!(text, "just prose");
        let obs = collect([json!({"observation": "a valve, badly corroded"})].iter());
        assert_eq!(obs, "a valve, badly corroded");
    }

    #[test]
    fn collect_renders_kb_chunks_with_their_own_sources() {
        let v = json!({ "chunks": [
            {"text": "torque the flange to 90 Nm", "source": "SOP-ROT-007"},
        ]});
        let out = collect([v].iter());
        assert!(out.contains("[SOP-ROT-007] torque the flange"), "{out}");
    }
}
