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
use crate::memory::{Exchange, PersistentMemory, SessionContext, TurnSummary};
use crate::planner::{complete_plan, plan_turn, validate_plan, wants_knowledge, Available, PlanOutcome};
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

    // Drop attachments we cannot use, with a visible warning, so one `.mov` in a
    // batch of good files never blocks the turn.
    let uploads = screen_uploads(uploads, sink);
    let uploads = uploads.as_slice();

    // Load the session BEFORE planning: a follow-up question ("and debit cards?")
    // is only answerable if role A and role B see what was asked and answered
    // before. Previously only role C saw any of it.
    let session = SessionContext::load_or_new(&config.session_path_for(session_id), session_id);
    let history = session.context_blob();

    // --- plan + deterministic validation (with retry ceiling) ---------------
    let plan_started = std::time::Instant::now();
    let (intent, outcome) =
        plan_turn(engine, prompt, uploads, &history, session.has_history(), sink).await?;
    tracing::info!(plan_ms = plan_started.elapsed().as_millis(), "turn phase: planning done");
    let plan = match outcome {
        PlanOutcome::Ready(plan) => plan,
        PlanOutcome::AwaitingUser { errors, last_plan } => {
            return Ok(TurnOutcome::AwaitingUser {
                errors,
                plan_json: serde_json::to_string(&last_plan)?,
            });
        }
    };

    // Make the plan cover what was asked: a small planner routinely skips the
    // photo or the SOP search. Here and not in `run_from_plan`, so a plan the
    // user edited in the HITL modal is never "completed" behind their back.
    let available = Available {
        vision: !engine.vision_model().trim().is_empty(),
        audio: config.whisper_model_path.is_file(),
    };
    let (plan, added) =
        complete_plan(plan, uploads, prompt, wants_knowledge(&intent, prompt), available);
    if !added.is_empty() {
        tracing::info!(?added, "plan completed: the planner left these out");
    }

    run_from_plan(engine, registry, config, session_id, prompt, uploads, plan, session, sink).await
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

    let uploads = screen_uploads(uploads, sink);
    let uploads = uploads.as_slice();

    let session = SessionContext::load_or_new(&config.session_path_for(session_id), session_id);

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

    run_from_plan(engine, registry, config, session_id, prompt, uploads, plan, session, sink).await
}

/// Keep only the attachments the pipeline can actually read; warn about the rest.
///
/// An unsupported type (`.mov`, `.zip`, …) used to become a hard validation error
/// the planner could never fix, so it burned every retry and parked the turn —
/// one bad file blocking every good one. Now it is dropped here with a
/// [`StepEvent::Warning`] and the turn proceeds on what remains. A file missing
/// from disk is left in; `validate_plan` still fails hard on that, because it
/// means something is genuinely broken.
fn screen_uploads(uploads: &[InputFile], sink: &dyn ProgressSink) -> Vec<InputFile> {
    let mut kept = Vec::with_capacity(uploads.len());
    for f in uploads {
        if f.kind == crate::engine::schemas::FileKind::Unknown {
            crate::events::warn(
                sink,
                format!(
                    "Skipped \"{}\" — unsupported file type. Supported: PDF, \
                     PNG/JPG/TIFF/BMP/WebP images, WAV/MP3/M4A/FLAC/OGG audio.",
                    f.original_name
                ),
                None,
            );
        } else {
            kept.push(f.clone());
        }
    }
    if kept.is_empty() && !uploads.is_empty() {
        crate::events::warn(
            sink,
            "None of the attached files are a supported type; answering from the prompt alone.",
            None,
        );
    }
    kept
}

/// Warn about any attachment no plan step reads, so a silently-ignored file is
/// visible rather than a mystery. (`validate_plan` counts nothing here — one
/// `ocr_image` step "covers" fifty images as far as it is concerned.)
fn warn_uncovered_files(plan: &Plan, uploads: &[InputFile], sink: &dyn ProgressSink) {
    // Same definition `complete_plan` uses, so after completion this fires only
    // for a file whose tool is switched off (or on a resumed plan).
    for f in uploads {
        if !crate::planner::is_covered(plan, uploads, f) {
            crate::events::warn(
                sink,
                format!(
                    "Attached file \"{}\" is not read by any step of this plan.",
                    f.original_name
                ),
                None,
            );
        }
    }
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
    mut session: SessionContext,
    sink: &dyn ProgressSink,
) -> Result<TurnOutcome> {
    // One model call instead of two when both analysis steps were planned. Here,
    // not in `plan_turn`, so fresh, resumed and force-resumed plans all get it.
    let plan = crate::planner::fold_analysis(plan);
    warn_uncovered_files(&plan, uploads, sink);

    // --- strictly sequential tool execution --------------------------------
    let session_blob = session.context_blob();
    let execute_started = std::time::Instant::now();
    let results =
        execute_plan(&plan, registry, uploads, config, engine, prompt, &session_blob, sink).await?;
    let execute_ms = execute_started.elapsed().as_millis();

    // A Stop pressed during the final tool: don't spend another minute
    // synthesising a report the user no longer wants.
    engine.cancel_check()?;

    // --- rule-based quality gate (before synthesis, per the blueprint) -----
    let quality = assert_sane(&plan, &results, None);
    if quality.passed {
        tracing::info!(checks = quality.checks.len(), "quality gate passed");
    } else {
        tracing::warn!(
            failed = ?quality.checks.iter().filter(|c| !c.passed).map(|c| &c.name).collect::<Vec<_>>(),
            "quality gate failed; report will be marked degraded"
        );
    }
    sink.emit(StepEvent::QualityCheck {
        passed: quality.passed,
    });

    // --- synthesise (role C) -------------------------------------------
    // `session` was loaded before planning and handed in. Cross-chat memory is
    // read here, and it is safe to share across chats now: it holds only facts
    // the user confirmed, never an automatic digest of some other conversation.
    let facility = PersistentMemory::load(&config.persistent_path());

    let compile_started = std::time::Instant::now();
    let mut report = compiler::compile(
        engine,
        prompt,
        &results,
        &session.context_blob(),
        &facility.context_blob(),
        sink,
    )
    .await;
    let compile_ms = compile_started.elapsed().as_millis();
    report.degraded = report.degraded || !quality.passed;
    // The model only *suggests* cross-chat facts; code decides. Drop anything
    // that isn't a short, grounded facility fact (a small model happily
    // proposes findings and document titles), then anything already confirmed.
    let proposed = report.memory_proposals.len();
    report.memory_proposals = crate::memory::proposals::screen(
        std::mem::take(&mut report.memory_proposals),
        prompt,
        &results,
    );
    report.memory_proposals.retain(|p| !facility.has(p));
    if proposed > 0 {
        tracing::info!(
            proposed,
            kept = report.memory_proposals.len(),
            "memory proposals screened"
        );
    }
    tracing::info!(
        degraded = report.degraded,
        findings = report.findings.len(),
        citations = ?report.citations,
        summary_chars = report.summary.len(),
        "report synthesised"
    );

    // --- record this turn: full transcript entry + bounded model summary ---
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
    let exchange = Exchange {
        prompt: prompt.to_string(),
        attachments: uploads.iter().map(|u| u.original_name.clone()).collect(),
        report: Some(report.clone()),
        ts: 0, // 0 = record_turn stamps the current time
    };
    let record_started = std::time::Instant::now();
    if let Err(e) = session
        .record_turn(exchange, turn, engine, &config.session_path_for(session_id))
        .await
    {
        tracing::warn!(error = %e, "failed to record session turn");
    }

    // One line to read the turn's shape from; per-call detail is in the
    // `model call timing` lines (and `engine.timings()`).
    let calls = engine.timings();
    tracing::info!(
        execute_ms,
        compile_ms,
        record_ms = record_started.elapsed().as_millis(),
        model_calls = calls.len(),
        model_wall_ms = calls.iter().map(|c| c.wall_ms).sum::<u128>(),
        "turn phases"
    );

    sink.emit(StepEvent::Done {
        report_json: serde_json::to_string(&report)?,
    });
    Ok(TurnOutcome::Completed(report))
}
