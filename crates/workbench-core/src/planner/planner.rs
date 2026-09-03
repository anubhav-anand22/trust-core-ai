//! The plan / validate / retry loop with a hard ceiling and a human off-ramp.
//!
//! Blueprint rule: **at most 2 automated re-prompts**, then stop and ask the user.
//! That is [`MAX_PLAN_ATTEMPTS`] = 3 (one initial attempt + two retries). There is
//! no unbounded "reflect and try again" loop anywhere in this pipeline.
//!
//! Planning talks to the model through the [`PlanSource`] trait rather than a
//! concrete client, so the retry ceiling and the HITL hand-off can be unit-tested
//! with a mock and no running Ollama.

use crate::engine::schemas::{InputFile, IntentResult, Plan};
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
    /// Role A.
    async fn parse_intent(&self, prompt: &str, uploads: &[InputFile]) -> Result<IntentResult>;

    /// Role B. `previous_errors` carries the validator's messages on a retry.
    async fn build_plan(
        &self,
        prompt: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan>;
}

#[async_trait::async_trait]
impl PlanSource for OllamaEngine {
    async fn parse_intent(&self, prompt: &str, uploads: &[InputFile]) -> Result<IntentResult> {
        OllamaEngine::parse_intent(self, prompt, uploads).await
    }

    async fn build_plan(
        &self,
        prompt: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        OllamaEngine::build_plan(self, prompt, intent, uploads, previous_errors).await
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
    sink: &dyn ProgressSink,
) -> Result<(IntentResult, PlanOutcome)> {
    sink.emit(StepEvent::ParsingContext { attempt: 1 });
    let intent = source.parse_intent(prompt, uploads).await?;

    let mut last_plan = Plan::default();
    let mut errors: Vec<String> = Vec::new();

    for attempt in 1..=MAX_PLAN_ATTEMPTS {
        let plan = source.build_plan(prompt, &intent, uploads, &errors).await?;

        sink.emit(StepEvent::ValidatingPlan { attempt });
        errors = validate_plan(&plan, uploads);

        if errors.is_empty() {
            tracing::info!(attempt, steps = plan.steps.len(), "plan validated");
            return Ok((intent, PlanOutcome::Ready(plan)));
        }

        tracing::warn!(attempt, ?errors, "plan rejected by validator");
        last_plan = plan;
    }

    tracing::warn!(
        attempts = MAX_PLAN_ATTEMPTS,
        "planner exhausted retries; handing over to user"
    );
    sink.emit(StepEvent::AwaitingUser {
        errors: errors.clone(),
        plan_json: serde_json::to_string(&last_plan).unwrap_or_else(|_| "{}".into()),
    });

    Ok((intent, PlanOutcome::AwaitingUser { errors, last_plan }))
}
