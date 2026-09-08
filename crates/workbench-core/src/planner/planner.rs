//! The plan / validate / retry loop with a hard ceiling and a human off-ramp.
//!
//! Blueprint rule: **at most 2 automated re-prompts**, then stop and ask the user.
//! That is [`MAX_PLAN_ATTEMPTS`] = 3 (one initial attempt + two retries). There is
//! no unbounded "reflect and try again" loop anywhere in this pipeline.
//!
//! Planning talks to the model through the [`PlanSource`] trait rather than a
//! concrete client, so the retry ceiling and the HITL hand-off can be unit-tested
//! with a mock and no running Ollama.

use crate::engine::schemas::{FileKind, InputFile, IntentResult, Plan, TaskStep};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::planner::registry::validate_plan;
use crate::Result;

/// 1 initial planner call + 2 retries. Exceeding this parks the run for the user.
pub const MAX_PLAN_ATTEMPTS: u32 = 3;

/// The two model calls planning needs. Implemented by [`OllamaEngine`] for real
/// runs and by mocks in tests.
#[async_trait::async_trait]
pub trait PlanSource: Send + Sync {
    /// Role A. `history` is the session's `context_blob` — empty on the first turn.
    async fn parse_intent(
        &self,
        prompt: &str,
        history: &str,
        uploads: &[InputFile],
    ) -> Result<IntentResult>;

    /// Role B. `previous_errors` carries the validator's messages on a retry.
    async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan>;
}

#[async_trait::async_trait]
impl PlanSource for OllamaEngine {
    async fn parse_intent(
        &self,
        prompt: &str,
        history: &str,
        uploads: &[InputFile],
    ) -> Result<IntentResult> {
        OllamaEngine::parse_intent(self, prompt, history, uploads).await
    }

    async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        OllamaEngine::build_plan(self, prompt, history, intent, uploads, previous_errors).await
    }
}

/// What planning produced.
#[derive(Debug)]
pub enum PlanOutcome {
    /// Passed every deterministic check; safe to execute.
    Ready(Plan),
    /// Retries exhausted. `errors` and `last_plan` go to the HITL modal so the
    /// user can correct the plan or approve it as-is.
    AwaitingUser { errors: Vec<String>, last_plan: Plan },
}

/// Parse intent, then plan-and-validate until the plan is clean or attempts run
/// out.
///
/// **Inputs:** a plan source, the raw prompt, the files attached this turn, and a
/// progress sink. **Output:** the parsed intent plus a [`PlanOutcome`].
///
/// On each retry the *validator's own error strings* are fed back to
/// [`PlanSource::build_plan`], so the model repairs the specific defect instead of
/// re-rolling blindly.
pub async fn plan_turn(
    source: &dyn PlanSource,
    prompt: &str,
    uploads: &[InputFile],
    history: &str,
    has_history: bool,
    sink: &dyn ProgressSink,
) -> Result<(IntentResult, PlanOutcome)> {
    sink.emit(StepEvent::ParsingContext { attempt: 1 });
    let intent = source.parse_intent(prompt, history, uploads).await?;

    let mut last_plan = Plan::default();
    let mut errors: Vec<String> = Vec::new();

    for attempt in 1..=MAX_PLAN_ATTEMPTS {
        let plan = source
            .build_plan(prompt, history, &intent, uploads, &errors)
            .await?;

        sink.emit(StepEvent::ValidatingPlan { attempt });
        errors = validate_plan(&plan, uploads);

        if errors.is_empty() {
            tracing::info!(attempt, steps = plan.steps.len(), "plan validated");
            return Ok((intent, PlanOutcome::Ready(plan)));
        }

        tracing::warn!(attempt, ?errors, "plan rejected by validator");
        last_plan = plan;
    }

    // Retries exhausted. Rather than dead-ending in the HITL modal, fall back to
    // a deterministic plan built straight from the attachments — one extraction
    // step per file, then analysis. The stepper still showed the model planning;
    // it just can never leave the user with nothing. Only if *that* somehow fails
    // validation (it should not) do we actually park.
    let fb = fallback_plan(&intent, uploads, has_history);
    let fb_errors = validate_plan(&fb, uploads);
    if fb_errors.is_empty() {
        tracing::warn!(
            attempts = MAX_PLAN_ATTEMPTS,
            steps = fb.steps.len(),
            "planner exhausted retries; using a deterministic fallback plan"
        );
        crate::events::warn(
            sink,
            "The model could not produce a valid plan, so a standard \
             one-step-per-file plan was used.",
            Some(format!("planner errors: {}", errors.join("; "))),
        );
        return Ok((intent, PlanOutcome::Ready(fb)));
    }

    tracing::warn!(
        attempts = MAX_PLAN_ATTEMPTS,
        ?fb_errors,
        "planner exhausted retries and the fallback plan also failed; handing over to user"
    );
    sink.emit(StepEvent::AwaitingUser {
        errors: errors.clone(),
        plan_json: serde_json::to_string(&last_plan).unwrap_or_else(|_| "{}".into()),
    });

    Ok((intent, PlanOutcome::AwaitingUser { errors, last_plan }))
}

/// A plan built with no model call: extract every attached file, then analyse.
///
/// Deterministic and always valid for the given uploads — the safety net when the
/// resident model cannot write a usable plan (common on a 0.5–1.5B CPU model).
pub(crate) fn fallback_plan(
    intent: &IntentResult,
    uploads: &[InputFile],
    has_history: bool,
) -> Plan {
    let step = |id: &str, task: &str, args: serde_json::Value, deps: Vec<String>| TaskStep {
        id: id.to_string(),
        task: task.to_string(),
        args,
        depends_on: deps,
    };

    // No new files and nothing to retrieve, but there IS a conversation: this is
    // a follow-up question. Answer it from the transcript.
    if uploads.is_empty() && !intent.needs_knowledge && has_history {
        return Plan {
            steps: vec![step(
                "fu",
                "answer_followup",
                serde_json::Value::Object(Default::default()),
                vec![],
            )],
        };
    }

    let mut steps: Vec<TaskStep> = Vec::new();
    let mut extract_ids: Vec<String> = Vec::new();

    for (i, f) in uploads.iter().enumerate() {
        let task = match f.kind {
            FileKind::Pdf => "parse_pdf",
            FileKind::Audio => "transcribe_audio",
            FileKind::Image => "analyze_image",
            FileKind::Unknown => continue, // screened out upstream; belt and braces
        };
        let id = format!("x{}", i + 1);
        steps.push(step(
            &id,
            task,
            serde_json::json!({ "file": f.original_name }),
            vec![],
        ));
        extract_ids.push(id);
    }

    if intent.needs_knowledge {
        steps.push(step(
            "kb",
            "search_knowledge",
            serde_json::json!({ "query": intent.intents.join("; ") }),
            vec![],
        ));
    }

    // Analysis depends on everything above it.
    let mut analysis_deps = extract_ids.clone();
    if intent.needs_knowledge {
        analysis_deps.push("kb".to_string());
    }

    if !analysis_deps.is_empty() {
        steps.push(step(
            "sum",
            "summarize",
            serde_json::Value::Object(Default::default()),
            analysis_deps.clone(),
        ));
        if intent.needs_knowledge {
            steps.push(step(
                "sop",
                "compare_to_sop",
                serde_json::Value::Object(Default::default()),
                analysis_deps,
            ));
        }
    }

    Plan { steps }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::engine::schemas::FileKind;
    use crate::planner::registry::validate_plan;

    fn file(name: &str, kind: FileKind) -> InputFile {
        InputFile {
            path: format!("/uploads/{name}"),
            kind,
            original_name: name.into(),
        }
    }

    fn intent(needs_knowledge: bool) -> IntentResult {
        IntentResult {
            intents: vec!["assess risk".into()],
            file_kinds: vec![],
            needs_knowledge,
        }
    }

    #[test]
    fn fallback_makes_one_step_per_file_and_names_it() {
        let uploads = vec![
            file("north.jpg", FileKind::Image),
            file("south.jpg", FileKind::Image),
            file("note.m4a", FileKind::Audio),
            file("sheet.pdf", FileKind::Pdf),
        ];
        let plan = fallback_plan(&intent(false), &uploads, false);

        let extract: Vec<_> = plan
            .steps
            .iter()
            .filter(|s| s.task != "summarize" && s.task != "compare_to_sop")
            .collect();
        assert_eq!(extract.len(), 4, "one extraction step per file");
        for s in &extract {
            let named = s.args.get("file").and_then(|v| v.as_str());
            assert!(
                named.map(|n| uploads.iter().any(|u| u.original_name == n)).unwrap_or(false),
                "step {} must name a real attachment, got {:?}",
                s.id,
                s.args
            );
        }
        // Two images → two analyze_image steps, not one.
        assert_eq!(
            plan.steps.iter().filter(|s| s.task == "analyze_image").count(),
            2
        );
    }

    #[test]
    fn fallback_plan_always_validates() {
        // validate_plan asserts every upload is really on disk, so use temp files.
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, kind: FileKind| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            InputFile {
                path: p.to_string_lossy().into_owned(),
                kind,
                original_name: name.into(),
            }
        };
        for nk in [false, true] {
            let uploads = vec![mk("a.pdf", FileKind::Pdf), mk("b.png", FileKind::Image)];
            let plan = fallback_plan(&intent(nk), &uploads, false);
            let errs = validate_plan(&plan, &uploads);
            assert!(errs.is_empty(), "needs_knowledge={nk}: {errs:?}");
        }
    }

    #[test]
    fn fallback_adds_sop_comparison_only_when_knowledge_is_needed() {
        let uploads = vec![file("a.pdf", FileKind::Pdf)];
        assert!(!fallback_plan(&intent(false), &uploads, false)
            .steps
            .iter()
            .any(|s| s.task == "compare_to_sop"));
        assert!(fallback_plan(&intent(true), &uploads, false)
            .steps
            .iter()
            .any(|s| s.task == "compare_to_sop"));
    }
}
