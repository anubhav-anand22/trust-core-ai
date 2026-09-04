//! `run_turn` — the full turn orchestration the Tauri host invokes.
//!
//! Sequence (mirrors the blueprint's numbered architecture):
//! ```text
//! Idle → parse intent (A) → build+validate plan (B, ≤3 tries) → [HITL if unresolved]
//!      → sequential tool execution → rule-based quality check → synthesise (C)
//!      → append session memory → Done
//! ```

use crate::engine::schemas::{FinalReport, InputFile, Plan};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::executor::{assert_sane, compiler, execute_plan, ToolRegistry};
use crate::memory::{PersistentMemory, SessionContext, TurnSummary};
use crate::planner::{plan_turn, validate_plan, PlanOutcome};
use crate::{PipelineConfig, Result};

/// Terminal state of one turn.
pub enum TurnOutcome {
    /// Pipeline ran to completion.
    Completed(FinalReport),
    /// Planner could not produce a valid plan (or a resumed plan still failed
    /// validation). The host surfaces the HITL modal; the user edits the plan and
    /// calls [`resume_turn`] with it.
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

    run_from_plan(engine, registry, config, session_id, prompt, uploads, plan, sink).await
}

/// Resume a turn the planner parked: execute a user-approved (optionally edited)
/// plan instead of asking the model to produce one.
///
/// **Inputs:** the same context as [`run_turn`], plus `plan_json` (the plan the
/// HITL modal returned) and `force`. With `force = false` the deterministic
/// validator runs again and a still-invalid plan simply re-parks. With
/// `force = true` — the modal's "run anyway" — validation is skipped and the plan
/// executes exactly as given.
///
/// The caller re-supplies `prompt` and `uploads` (the front-end still holds them);
/// nothing is stashed server-side between the park and the resume.
pub async fn resume_turn(
    engine: &OllamaEngine,
    registry: &ToolRegistry,
    config: &PipelineConfig,
    session_id: &str,
    prompt: &str,
    uploads: &[InputFile],
    plan_json: &str,
    force: bool,
    sink: &dyn ProgressSink,
) -> Result<TurnOutcome> {
    sink.emit(StepEvent::Idle);

    let plan: Plan = serde_json::from_str(plan_json)?;

    if !force {
        sink.emit(StepEvent::ValidatingPlan { attempt: 1 });
        let errors = validate_plan(&plan, uploads);
        if !errors.is_empty() {
            let plan_json = serde_json::to_string(&plan)?;
            sink.emit(StepEvent::AwaitingUser {
                errors: errors.clone(),
                plan_json: plan_json.clone(),
            });
            return Ok(TurnOutcome::AwaitingUser { errors, plan_json });
        }
    }

    run_from_plan(engine, registry, config, session_id, prompt, uploads, plan, sink).await
}

/// The shared tail of [`run_turn`] / [`resume_turn`]: run a validated plan through
/// execution → quality gate → synthesis → memory, streaming progress on `sink`.
#[allow(clippy::too_many_arguments)]
async fn run_from_plan(
    engine: &OllamaEngine,
    registry: &ToolRegistry,
    config: &PipelineConfig,
    session_id: &str,
    prompt: &str,
    uploads: &[InputFile],
    plan: Plan,
    sink: &dyn ProgressSink,
) -> Result<TurnOutcome> {
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

    // Fold the session digest into encrypted long-term memory every turn, so a
    // crash between turns never loses history. `end_session` does the same on an
    // orderly shutdown.
    persist_long_term(config, &session);

    sink.emit(StepEvent::Done {
        report_json: serde_json::to_string(&report)?,
    });
    Ok(TurnOutcome::Completed(report))
}

/// Update `persistent_memory.json` (AES-256-GCM) from the current session's
/// rolling context.
///
/// **Inputs:** pipeline config (for the file path) and the session to summarise.
/// Best-effort: a write failure is logged, never propagated — losing long-term
/// memory must not fail a turn or a shutdown.
pub fn persist_long_term(config: &PipelineConfig, session: &SessionContext) {
    let path = config.persistent_path();
    let mut memory = PersistentMemory::load(&path);

    let digest = if !session.rolling_summary.is_empty() {
        session.rolling_summary.clone()
    } else {
        session
            .turns
            .iter()
            .map(|t| t.report_summary.as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    if !digest.is_empty() {
        memory.history_digest = digest;
    }

    if let Err(e) = memory.persist(&path) {
        tracing::warn!(error = %e, "failed to persist long-term memory");
    }
}
