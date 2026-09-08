//! Strictly sequential tool execution.
//!
//! The blueprint's resource rule: tools run "one after another in a synchronous
//! queue". This module never spawns concurrent work — a single `for` loop over the
//! validated plan. That is what guarantees only one model is resident at a time,
//! and it makes the stepper's progress honest.

use std::collections::HashMap;

use crate::engine::schemas::{InputFile, Plan, ToolResult};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, PipelineConfig, Result};

/// Name → tool lookup, populated by the host in Phase 2.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool under its [`Tool::name`].
    pub fn register(&mut self, tool: Box<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name(), tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Run every step of a validated plan, in order, one at a time.
///
/// **Inputs:** the validated plan, the registry of available tools, the attached
/// files, config, the original prompt, and a progress sink.
/// **Output:** one [`ToolResult`] per step, in plan order.
///
/// A tool that fails does *not* abort the run — it yields `ok: false` and the loop
/// continues, so the user still gets a partial report. The quality gate and the
/// report's `degraded` flag are what surface the failure.
pub async fn execute_plan(
    plan: &Plan,
    registry: &ToolRegistry,
    uploads: &[InputFile],
    config: &PipelineConfig,
    engine: &OllamaEngine,
    prompt: &str,
    session_blob: &str,
    sink: &dyn ProgressSink,
) -> Result<Vec<ToolResult>> {
    let total = plan.steps.len();
    let mut results: Vec<ToolResult> = Vec::with_capacity(total);
    // Step id → that step's `data`, so later steps can read earlier output.
    let mut outputs: HashMap<String, serde_json::Value> = HashMap::new();

    tracing::info!(
        steps = total,
        plan = ?plan.steps.iter().map(|s| s.task.as_str()).collect::<Vec<_>>(),
        "executing plan"
    );

    for (index, step) in plan.steps.iter().enumerate() {
        // Honour a Stop pressed during the previous step before starting the next.
        // A blocking tool (PDF, OCR, whisper) still finishes its current call —
        // nothing can safely kill `spawn_blocking` mid-flight — but the run ends
        // here rather than grinding through every remaining step.
        if engine.cancel_check().is_err() {
            tracing::info!(next_step = %step.id, "run cancelled by user before this step");
            return Err(CoreError::Cancelled);
        }

        sink.emit(StepEvent::ExecutingTool {
            tool: step.task.clone(),
            index,
            total,
        });

        tracing::info!(
            step = %step.id,
            tool = %step.task,
            position = format!("{}/{}", index + 1, total),
            depends_on = ?step.depends_on,
            args = %step.args,
            "tool: start"
        );

        let started = std::time::Instant::now();

        let result = match registry.get(&step.task) {
            Some(tool) => {
                let ctx = ToolContext {
                    config,
                    uploads,
                    outputs: &outputs,
                    prompt,
                    session_blob,
                    engine,
                    sink,
                };
                match tool.run(step, &ctx).await {
                    Ok(r) => r,
                    // User pressed Stop while this tool was running: abandon the
                    // whole turn, don't synthesise a half-report.
                    Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                    // Timed out: the turn goes on without this step's output, but
                    // the user is told why the report is thin.
                    Err(e @ CoreError::Timeout { .. }) => {
                        crate::events::warn(
                            sink,
                            format!(
                                "Step '{}' ran out of time and was skipped; the report                                  will be missing its findings.",
                                step.task
                            ),
                            Some(e.to_string()),
                        );
                        ToolResult::failure(
                            &step.id,
                            &step.task,
                            e.to_string(),
                            started.elapsed().as_millis(),
                        )
                    }
                    Err(e) => ToolResult::failure(
                        &step.id,
                        &step.task,
                        e.to_string(),
                        started.elapsed().as_millis(),
                    ),
                }
            }
            // Validation should have caught this; treat it as a degraded step
            // rather than a panic so one bad row can't kill a demo.
            None => ToolResult::failure(
                &step.id,
                &step.task,
                format!("no tool registered for task `{}`", step.task),
                started.elapsed().as_millis(),
            ),
        };

        sink.emit(StepEvent::ToolFinished {
            tool: step.task.clone(),
            ok: result.ok,
            elapsed_ms: result.elapsed_ms,
        });

        if result.ok {
            tracing::info!(
                step = %step.id,
                tool = %step.task,
                elapsed_ms = result.elapsed_ms,
                output_chars = result.data.to_string().len(),
                warning = result.warning.as_deref().unwrap_or(""),
                "tool: ok"
            );
            outputs.insert(step.id.clone(), result.data.clone());
        } else {
            // Not fatal by design — a failed tool degrades the run rather than
            // aborting it — but it is the first thing to look for in a log.
            tracing::error!(
                step = %step.id,
                tool = %step.task,
                elapsed_ms = result.elapsed_ms,
                error = result.error.as_deref().unwrap_or("unknown"),
                "tool: FAILED (run continues, report will be degraded)"
            );
        }
        results.push(result);
    }

    Ok(results)
}
