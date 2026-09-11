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
use crate::planner::{plan_turn, validate_plan, PlanOutcome};
use crate::{PipelineConfig, Result};

/// **Temporarily disabled (2026-09-12).** `PersistentMemory` is ONE file shared by
/// every session (`persistent_memory.json`, keyed by nothing but the machine) —
/// not scoped per session the way `SessionContext` is. `persist_long_term` folded
/// each session's digest into that single file, and every turn in every OTHER
/// session then loaded the same file and fed its `history_digest` /
/// `facility_metadata` into role C's prompt as "long-term facility memory" — one
/// user's (or one chat's) context leaking into an unrelated conversation.
///
/// This flag is the single choke point for that cross-session store: `false`
/// means a turn never reads it (role C gets `facility_memory: ""`, which the
/// prompt already renders as "(none)") and never writes it (`persist_long_term`
/// returns immediately). Per-session memory — `SessionContext`,
/// `sessions/<id>.json`, `turns`, `rolling_summary` — is a **different type**
/// entirely and is NOT touched by this flag; a follow-up question inside one chat
/// still has full context.
///
/// Any `persistent_memory.json` already on disk from before this flag is simply
/// never read while it is `false` — inert, not deleted. Flip back to `true` once
/// `PersistentMemory` carries a session (or user) key instead of being global;
/// see docs/02-execution-log.md, "the cross-session leak", for the real fix.
const GLOBAL_MEMORY_ENABLED: bool = false;

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
    let (_intent, outcome) =
        plan_turn(engine, prompt, uploads, &history, session.has_history(), sink).await?;
    let plan = match outcome {
        PlanOutcome::Ready(plan) => plan,
        PlanOutcome::AwaitingUser { errors, last_plan } => {
            return Ok(TurnOutcome::AwaitingUser {
                errors,
                plan_json: serde_json::to_string(&last_plan)?,
            });
        }
    };

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
    use crate::planner::registry::lookup;

    for f in uploads {
        // Covered if some step names this exact file…
        let named = plan.steps.iter().any(|s| {
            s.args.get("file").and_then(|v| v.as_str()) == Some(f.original_name.as_str())
        });
        // …or a step consumes this kind without naming a file, in which case
        // `ToolContext::file_for` falls back to the first upload of that kind.
        let is_first_of_kind =
            uploads.iter().find(|u| u.kind == f.kind).map(|u| &u.original_name)
                == Some(&f.original_name);
        let covered_by_fallback = is_first_of_kind
            && plan.steps.iter().any(|s| {
                s.args.get("file").is_none()
                    && lookup(&s.task).and_then(|spec| spec.requires_file) == Some(f.kind)
            });

        if !named && !covered_by_fallback {
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
    warn_uncovered_files(&plan, uploads, sink);

    // --- strictly sequential tool execution --------------------------------
    let session_blob = session.context_blob();
    let results =
        execute_plan(&plan, registry, uploads, config, engine, prompt, &session_blob, sink).await?;

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
    // `session` was loaded before planning and handed in; only long-term memory
    // is read here — gated by `GLOBAL_MEMORY_ENABLED` (see its doc comment: this
    // is the cross-session store, disabled because it was leaking between chats).
    let facility_context = if GLOBAL_MEMORY_ENABLED {
        PersistentMemory::load(&config.persistent_path()).context_blob()
    } else {
        String::new()
    };

    let mut report = compiler::compile(
        engine,
        prompt,
        &results,
        &session.context_blob(),
        &facility_context,
        sink,
    )
    .await;
    report.degraded = report.degraded || !quality.passed;
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
        ts: 0, // filled in by record_turn from the clock
    };
    if let Err(e) = session
        .record_turn(exchange, turn, engine, &config.session_path_for(session_id))
        .await
    {
        tracing::warn!(error = %e, "failed to record session turn");
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
    // See `GLOBAL_MEMORY_ENABLED`: this is the write side of the disabled
    // cross-session store. Covers both callers of this function (the mid-turn
    // call below and `end_session` in the Tauri host) from one place, so neither
    // can be re-enabled without the other by accident.
    if !GLOBAL_MEMORY_ENABLED {
        return;
    }

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
