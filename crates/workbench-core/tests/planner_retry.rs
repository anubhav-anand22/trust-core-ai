//! Phase 1 gate: the planner retries a rejected plan at most twice, then parks the
//! run for the user — and it *does* succeed if a valid plan arrives on the last
//! allowed attempt.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use workbench_core::engine::schemas::{InputFile, IntentResult, Plan, TaskStep};
use workbench_core::events::VecSink;
use workbench_core::planner::{plan_turn, PlanOutcome, PlanSource, MAX_PLAN_ATTEMPTS};
use workbench_core::Result;

fn bogus_plan() -> Plan {
    Plan {
        steps: vec![TaskStep {
            id: "s1".into(),
            task: "not_a_real_task".into(),
            args: serde_json::Value::Null,
            depends_on: vec![],
        }],
    }
}

fn valid_plan() -> Plan {
    Plan {
        steps: vec![TaskStep {
            id: "s1".into(),
            task: "search_knowledge".into(),
            args: serde_json::Value::Null,
            depends_on: vec![],
        }],
    }
}

fn empty_intent() -> IntentResult {
    IntentResult {
        intents: vec!["inspect".into()],
        file_kinds: vec![],
        needs_knowledge: true,
    }
}

/// Never produces a valid plan.
struct AlwaysInvalid {
    build_calls: AtomicU32,
}

#[async_trait]
impl PlanSource for AlwaysInvalid {
    async fn parse_intent(&self, _p: &str, _u: &[InputFile]) -> Result<IntentResult> {
        Ok(empty_intent())
    }
    async fn build_plan(
        &self,
        _p: &str,
        _i: &IntentResult,
        _u: &[InputFile],
        _e: &[String],
    ) -> Result<Plan> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        Ok(bogus_plan())
    }
}

#[tokio::test]
async fn stops_after_two_retries_then_awaits_user() {
    let src = AlwaysInvalid {
        build_calls: AtomicU32::new(0),
    };
    let sink = VecSink::new();

    let (_intent, outcome) = plan_turn(&src, "do the thing", &[], &sink).await.unwrap();

    match outcome {
        PlanOutcome::AwaitingUser { errors, .. } => {
            assert!(!errors.is_empty(), "HITL hand-off must carry the errors")
        }
        other => panic!("expected AwaitingUser, got {other:?}"),
    }

    // Exactly 1 initial call + 2 retries.
    assert_eq!(src.build_calls.load(Ordering::SeqCst), MAX_PLAN_ATTEMPTS);
    assert_eq!(
        sink.stages().last().map(String::as_str),
        Some("awaiting_user"),
        "stepper must end on awaiting_user; got {:?}",
        sink.stages()
    );
}

/// Invalid for the first two build calls, valid on the third (last allowed) one.
struct ValidOnLastAttempt {
    build_calls: AtomicU32,
}

#[async_trait]
impl PlanSource for ValidOnLastAttempt {
    async fn parse_intent(&self, _p: &str, _u: &[InputFile]) -> Result<IntentResult> {
        Ok(empty_intent())
    }
    async fn build_plan(
        &self,
        _p: &str,
        _i: &IntentResult,
        _u: &[InputFile],
        _e: &[String],
    ) -> Result<Plan> {
        let prior = self.build_calls.fetch_add(1, Ordering::SeqCst);
        if prior < 2 {
            Ok(bogus_plan())
        } else {
            Ok(valid_plan())
        }
    }
}

#[tokio::test]
async fn recovers_on_last_allowed_attempt() {
    let src = ValidOnLastAttempt {
        build_calls: AtomicU32::new(0),
    };
    let sink = VecSink::new();

    let (_intent, outcome) = plan_turn(&src, "p", &[], &sink).await.unwrap();

    assert!(
        matches!(outcome, PlanOutcome::Ready(_)),
        "a valid plan on attempt 3 must be accepted"
    );
    assert_eq!(src.build_calls.load(Ordering::SeqCst), 3);
}
