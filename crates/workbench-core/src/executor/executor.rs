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
use crate::{PipelineConfig, Result};

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
    sink: &dyn ProgressSink,
) -> Result<Vec<ToolResult>> {
    let total = plan.steps.len();
    let mut results: Vec<ToolResult> = Vec::with_capacity(total);
    // Step id → that step's `data`, so later steps can read earlier output.
    let mut outputs: HashMap<String, serde_json::Value> = HashMap::new();

    for (index, step) in plan.steps.iter().enumerate() {
        sink.emit(StepEvent::ExecutingTool {
            tool: step.task.clone(),
            index,
            total,
        });

        let started = std::time::Instant::now();

        let result = match registry.get(&step.task) {
            Some(tool) => {
                let ctx = ToolContext {
                    config,
                    uploads,
                    outputs: &outputs,
                    prompt,
                    engine,
                };
                match tool.run(step, &ctx).await {
                    Ok(r) => r,
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
            outputs.insert(step.id.clone(), result.data.clone());
        }
        results.push(result);
    }

    Ok(results)
}
