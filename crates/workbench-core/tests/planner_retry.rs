//! Phase 1 gate: the planner retries a rejected plan at most twice. If it still
//! has no valid plan it builds a deterministic fallback (Stage 3) rather than
//! dead-ending — and only parks for the user if even that fallback is empty
//! (nothing to extract, no knowledge to fetch). A valid plan on the last allowed
//! attempt is still accepted.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use workbench_core::engine::schemas::{FileKind, InputFile, IntentResult, Plan, TaskStep};
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

fn intent(needs_knowledge: bool) -> IntentResult {
    IntentResult {
        intents: vec!["inspect".into()],
        file_kinds: vec![],
        needs_knowledge,
    }
}

/// Never produces a valid plan.
struct AlwaysInvalid {
    build_calls: AtomicU32,
    needs_knowledge: bool,
}

#[async_trait]
impl PlanSource for AlwaysInvalid {
    async fn parse_intent(&self, _p: &str, _h: &str, _u: &[InputFile]) -> Result<IntentResult> {
        Ok(intent(self.needs_knowledge))
    }
    async fn build_plan(
        &self,
        _p: &str,
        _h: &str,
        _i: &IntentResult,
        _u: &[InputFile],
        _e: &[String],
    ) -> Result<Plan> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        Ok(bogus_plan())
    }
}

/// With something to work on (an attachment, or knowledge to fetch), an
/// exhausted planner produces a deterministic fallback plan, not a dead end.
#[tokio::test]
async fn exhausted_planner_falls_back_instead_of_parking() {
    let src = AlwaysInvalid {
        build_calls: AtomicU32::new(0),
        needs_knowledge: true,
    };
    let sink = VecSink::new();

    let (_intent, outcome) = plan_turn(&src, "do the thing", &[], "", false, &sink).await.unwrap();

    match outcome {
        PlanOutcome::Ready(plan) => {
            assert!(!plan.steps.is_empty(), "fallback plan must have steps");
            assert!(
                plan.steps.iter().any(|s| s.task == "search_knowledge"),
                "needs_knowledge fallback should search the KB: {plan:?}"
            );
        }
        other => panic!("expected a fallback Ready plan, got {other:?}"),
    }

    // Still exactly 1 initial call + 2 retries before the fallback.
    assert_eq!(src.build_calls.load(Ordering::SeqCst), MAX_PLAN_ATTEMPTS);
    assert!(
        sink.stages().contains(&"warning".to_string()),
        "the user must be told a fallback plan was used; stages: {:?}",
        sink.stages()
    );
}

/// The genuine dead end: nothing attached and no knowledge needed, so even the
/// deterministic fallback is empty. Only then does it park.
#[tokio::test]
async fn truly_empty_fallback_still_parks() {
    let src = AlwaysInvalid {
        build_calls: AtomicU32::new(0),
        needs_knowledge: false,
    };
    let sink = VecSink::new();

    let (_intent, outcome) = plan_turn(&src, "chat with no inputs", &[], "", false, &sink)
        .await
        .unwrap();

    match outcome {
        PlanOutcome::AwaitingUser { errors, .. } => {
            assert!(!errors.is_empty(), "HITL hand-off must carry the errors")
        }
        other => panic!("expected AwaitingUser, got {other:?}"),
    }
    assert_eq!(src.build_calls.load(Ordering::SeqCst), MAX_PLAN_ATTEMPTS);
    assert_eq!(
        sink.stages().last().map(String::as_str),
        Some("awaiting_user"),
        "stepper must end on awaiting_user; got {:?}",
        sink.stages()
    );
}

/// A fallback built from real attachments should be executable as-is.
#[tokio::test]
async fn fallback_from_attachments_is_valid() {
    use workbench_core::planner::validate_plan;

    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("north.jpg");
    std::fs::write(&img, b"x").unwrap();
    let uploads = vec![InputFile {
        path: img.to_string_lossy().into_owned(),
        kind: FileKind::Image,
        original_name: "north.jpg".into(),
    }];

    let src = AlwaysInvalid {
        build_calls: AtomicU32::new(0),
        needs_knowledge: false,
    };
    let sink = VecSink::new();
    let (_intent, outcome) = plan_turn(&src, "inspect", &uploads, "", false, &sink).await.unwrap();

    let PlanOutcome::Ready(plan) = outcome else {
        panic!("expected a fallback Ready plan");
    };
    assert!(validate_plan(&plan, &uploads).is_empty());
    assert!(plan.steps.iter().any(|s| s.task == "analyze_image"));
}

/// Invalid for the first two build calls, valid on the third (last allowed) one.
struct ValidOnLastAttempt {
    build_calls: AtomicU32,
}

#[async_trait]
impl PlanSource for ValidOnLastAttempt {
    async fn parse_intent(&self, _p: &str, _h: &str, _u: &[InputFile]) -> Result<IntentResult> {
        Ok(intent(true))
    }
    async fn build_plan(
        &self,
        _p: &str,
        _h: &str,
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

    let (_intent, outcome) = plan_turn(&src, "p", &[], "", false, &sink).await.unwrap();

    assert!(
        matches!(outcome, PlanOutcome::Ready(_)),
        "a valid plan on attempt 3 must be accepted"
    );
    assert_eq!(src.build_calls.load(Ordering::SeqCst), 3);
}
