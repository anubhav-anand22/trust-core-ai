//! Role C wrapper: turn tool results + memory into a [`FinalReport`].
//!
//! Thin on purpose — the prompt and schema live in [`OllamaEngine`]. This adds the
//! one bit of policy the engine shouldn't own: if the model's JSON is unusable even
//! after a retry, fall back to a plain-text report rather than failing the turn.

use crate::engine::schemas::{FinalReport, ToolResult};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};

/// Compile the report, retrying once on a schema error, then degrading to a
/// best-effort text summary built from the tool outputs.
///
/// **Inputs:** engine, the user prompt, every tool result, the running session
/// summary, and long-term facility memory.
/// **Output:** always a [`FinalReport`] — never an error — so a flaky compiler
/// can't sink an otherwise good run.
pub async fn compile(
    engine: &OllamaEngine,
    prompt: &str,
    results: &[ToolResult],
    session_summary: &str,
    facility_memory: &str,
    sink: &dyn ProgressSink,
) -> FinalReport {
    sink.emit(StepEvent::Synthesizing);
    let any_failure = results.iter().any(|r| !r.ok);

    for attempt in 1..=2 {
        match engine
            .compile_report(prompt, results, session_summary, facility_memory)
            .await
        {
            Ok(mut report) => {
                report.degraded = report.degraded || any_failure;
                return report;
            }
            Err(e) => tracing::warn!(attempt, error = %e, "compiler returned bad JSON"),
        }
    }

    tracing::warn!("compiler fell back to plain-text report");
    fallback_report(results, any_failure)
}

/// Deterministic report assembled straight from tool output — used only when the
/// LLM compiler can't produce valid JSON twice in a row.
fn fallback_report(results: &[ToolResult], any_failure: bool) -> FinalReport {
    let mut findings = Vec::new();
    for r in results {
        if r.ok {
            if let Some(text) = r.data.get("text").and_then(|t| t.as_str()) {
                let snippet: String = text.chars().take(280).collect();
                findings.push(format!("[{}] {}", r.task, snippet));
            } else if let Some(obs) = r.data.get("observation").and_then(|t| t.as_str()) {
                findings.push(format!("[{}] {}", r.task, obs));
            }
        } else if let Some(err) = &r.error {
            findings.push(format!("[{}] failed: {}", r.task, err));
        }
    }
    let _ = any_failure; // the fallback is always degraded regardless
    FinalReport {
        summary: "Automated synthesis was unavailable; raw tool findings are listed below."
            .into(),
        findings,
        citations: Vec::new(),
        safety_notes: Vec::new(),
        degraded: true,
    }
}
