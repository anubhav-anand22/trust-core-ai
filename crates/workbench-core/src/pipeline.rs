//! `run_turn` — the full turn orchestration the Tauri host invokes.
//!
//! Sequence (mirrors the blueprint's numbered architecture):
//! ```text
//! Idle → parse intent (A) → build+validate plan (B, ≤3 tries) → [HITL if unresolved]
//!      → sequential tool execution → rule-based quality check → synthesise (C)
//!      → append session memory → Done
//! ```

use crate::engine::schemas::{FinalReport, InputFile};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::executor::{assert_sane, compiler, execute_plan, ToolRegistry};
use crate::memory::{PersistentMemory, SessionContext, TurnSummary};
use crate::planner::{plan_turn, PlanOutcome};
use crate::{PipelineConfig, Result};

/// Terminal state of one turn.
pub enum TurnOutcome {
    /// Pipeline ran to completion.
    Completed(FinalReport),
    /// Planner could not produce a valid plan; the host must surface the HITL
    /// modal and later call `run_turn` again with the user-approved plan (Phase 2
    /// wires the resume path).
    AwaitingUser { errors: Vec<String>, plan_json: String },
}

/// Run one turn end to end.
///
/// **Inputs:** the engine (resident model), the tool registry (empty until Phase 2
/// registers tools), pipeline config, the session id, the user's prompt, and the
/// files attached to this turn. Progress is streamed on `sink`.
/// **Output:** a [`TurnOutcome`]. Errors are reserved for infrastructure failures
/// (Ollama unreachable); tool-level and compiler-level problems degrade the report
/// instead.
pub async fn run_turn(
    engine: &OllamaEngine,
    registry: &ToolRegistry,
    config: &PipelineConfig,
    session_id: &str,
    prompt: &str,
    uploads: &[InputFile],
    sink: &dyn ProgressSink,
) -> Result<TurnOutcome> {
    sink.emit(StepEvent::Idle);

    // --- plan + deterministic validation (with retry ceiling) ---------------
    let (_intent, outcome) = plan_turn(engine, prompt, uploads, sink).await?;
    let plan = match outcome {
        PlanOutcome::Ready(plan) => plan,
        PlanOutcome::AwaitingUser { errors, last_plan } => {
            return Ok(TurnOutcome::AwaitingUser {
                errors,
                plan_json: serde_json::to_string(&last_plan)?,
            });
        }
    };

    // --- strictly sequential tool execution --------------------------------
    let results = execute_plan(&plan, registry, uploads, config, engine, prompt, sink).await?;

    // --- rule-based quality gate (before synthesis, per the blueprint) -----
    let quality = assert_sane(&plan, &results, None);
    sink.emit(StepEvent::QualityCheck {
        passed: quality.passed,
    });

    // --- load memory, synthesise (role C) --------------------------------
    let mut session = SessionContext::load_or_new(&config.session_path(), session_id);
    let facility = PersistentMemory::load(&config.persistent_path());

    let mut report = compiler::compile(
        engine,
        prompt,
        &results,
        &session.context_blob(),
        &facility.context_blob(),
        sink,
    )
    .await;
    report.degraded = report.degraded || !quality.passed;

    // --- append this turn to session memory (best effort) ----------------
    let turn = TurnSummary {
        user_prompt: prompt.chars().take(200).collect(),
        plan_summary: plan
            .steps
            .iter()
            .map(|s| s.task.clone())
            .collect::<Vec<_>>()
            .join(" → "),
        tool_summary: format!(
            "{} tool(s), {} ok",
            results.len(),
            results.iter().filter(|r| r.ok).count()
        ),
        report_summary: report.summary.chars().take(200).collect(),
    };
    if let Err(e) = session
        .append_turn(turn, engine, &config.session_path())
        .await
    {
        tracing::warn!(error = %e, "failed to append session turn");
    }

    sink.emit(StepEvent::Done {
        report_json: serde_json::to_string(&report)?,
    });
    Ok(TurnOutcome::Completed(report))
}
