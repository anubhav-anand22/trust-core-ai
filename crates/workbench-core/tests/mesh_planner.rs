//! Delegated planning (MESH-9/10): the planner's model calls run on a Worker,
//! while validation, retries and the fallback still run on the Host.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use workbench_core::engine::schemas::{FileKind, InputFile, IntentResult, Plan, TaskStep, ToolResult};
use workbench_core::engine::OllamaEngine;
use workbench_core::events::VecSink;
use workbench_core::mesh::{run_plan_job, Dispatcher, Node, PlanCall, PlanJob, PlanReply, RemotePlanner, StepJob};
use workbench_core::planner::{plan_turn, PlanOutcome, PlanSource};
use workbench_core::{CancelFlag, CoreError, ProgressSink, Result, StepEvent};

fn step(task: &str) -> TaskStep {
    TaskStep { id: "s1".into(), task: task.into(), args: serde_json::Value::Null, depends_on: vec![] }
}

/// Invalid on the first build call, valid after: proves the Host's
/// validate → retry loop runs around the remote calls.
struct Planner {
    intent_calls: AtomicU32,
    build_calls: AtomicU32,
}

impl Planner {
    fn new() -> Self {
        Self { intent_calls: AtomicU32::new(0), build_calls: AtomicU32::new(0) }
    }
    fn calls(&self) -> u32 {
        self.intent_calls.load(Ordering::SeqCst) + self.build_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PlanSource for Planner {
    async fn parse_intent(&self, _p: &str, _h: &str, _u: &[InputFile]) -> Result<IntentResult> {
        self.intent_calls.fetch_add(1, Ordering::SeqCst);
        Ok(IntentResult { intents: vec!["inspect".into()], file_kinds: vec![], needs_knowledge: true })
    }
    async fn build_plan(&self, _p: &str, _h: &str, _i: &IntentResult, _u: &[InputFile], _e: &[String]) -> Result<Plan> {
        let prior = self.build_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Plan { steps: vec![step(if prior == 0 { "not_a_real_task" } else { "search_knowledge" })] })
    }
}

enum Behaviour {
    Answer,
    Fail,
    Hang,
}

/// A mesh whose delegator runs the plan calls on its own `Planner`.
struct Delegating {
    behaviour: Behaviour,
    worker_planner: Planner,
    jobs: Mutex<Vec<PlanJob>>,
    cancelled: AtomicBool,
}

impl Delegating {
    fn new(behaviour: Behaviour) -> Self {
        Self { behaviour, worker_planner: Planner::new(), jobs: Mutex::new(vec![]), cancelled: AtomicBool::new(false) }
    }
}

#[async_trait]
impl Dispatcher for Delegating {
    fn candidates(&self, _step: &TaskStep) -> Vec<Node> {
        vec![]
    }
    async fn run_on_worker(&self, _w: &str, _job: StepJob, _s: &dyn ProgressSink) -> Result<ToolResult> {
        unreachable!("planning only")
    }
    async fn cancel_all(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    fn delegator(&self) -> Option<String> {
        Some("gpu-box".into())
    }
    async fn plan_on_worker(&self, worker: &str, job: PlanJob) -> Result<PlanReply> {
        assert_eq!(worker, "gpu-box");
        self.jobs.lock().unwrap().push(job.clone());
        match self.behaviour {
            Behaviour::Answer => run_plan_job(&job, &self.worker_planner).await,
            Behaviour::Fail => Err(CoreError::Tool { tool: "mesh:gpu-box".into(), message: "dropped".into() }),
            Behaviour::Hang => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                unreachable!()
            }
        }
    }
}

fn attachment(dir: &std::path::Path) -> InputFile {
    let path = dir.join("pump-report.pdf");
    std::fs::write(&path, b"%PDF").unwrap();
    InputFile { path: path.to_string_lossy().into_owned(), kind: FileKind::Pdf, original_name: "pump-report.pdf".into() }
}

#[tokio::test]
async fn the_worker_plans_and_the_host_still_validates_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = [attachment(dir.path())];
    let mesh = Delegating::new(Behaviour::Answer);
    let local = Planner::new();
    let engine = OllamaEngine::new("http://127.0.0.1:11434", "m", "v", "e").with_persona("plant inspector");
    let sink = VecSink::new();
    let remote = RemotePlanner::new(&mesh, "gpu-box".into(), &local, &engine, &sink);

    let (_intent, outcome) = plan_turn(&remote, "check the pump", &uploads, "HISTORY", true, &sink).await.unwrap();

    assert!(matches!(outcome, PlanOutcome::Ready(p) if p.steps[0].task == "search_knowledge"));
    assert_eq!(mesh.worker_planner.calls(), 3, "intent + a rejected plan + the retry, all on the Worker");
    assert_eq!(local.calls(), 0, "the Host's model was never used");

    let jobs = mesh.jobs.lock().unwrap();
    for job in jobs.iter() {
        assert_eq!(job.persona, "plant inspector");
        let (history, uploads) = match &job.call {
            PlanCall::ParseIntent { history, uploads, .. } => (history, uploads),
            PlanCall::BuildPlan { history, uploads, .. } => (history, uploads),
        };
        assert_eq!(history, "HISTORY", "MESH-5: the delegator sees the conversation");
        assert_eq!(uploads[0].path, "pump-report.pdf", "names and kinds only, never the Host's path");
    }
    let PlanCall::BuildPlan { previous_errors, .. } = &jobs[2].call else { panic!("third call is the retry") };
    assert!(!previous_errors.is_empty(), "the Host's validator errors went back to the Worker");
}

#[tokio::test]
async fn a_worker_that_cannot_plan_hands_the_turn_back_to_the_host_once() {
    let mesh = Delegating::new(Behaviour::Fail);
    let local = Planner::new();
    let engine = OllamaEngine::new("http://127.0.0.1:11434", "m", "v", "e");
    let sink = VecSink::new();
    let remote = RemotePlanner::new(&mesh, "gpu-box".into(), &local, &engine, &sink);

    let (_intent, outcome) = plan_turn(&remote, "q", &[], "", false, &sink).await.unwrap();

    assert!(matches!(outcome, PlanOutcome::Ready(_)));
    assert_eq!(mesh.jobs.lock().unwrap().len(), 1, "gave up on the Worker after one failure");
    assert_eq!(local.calls(), 3, "the Host planned the whole turn instead");
    let warnings = sink.events().iter().filter(|e| matches!(e, StepEvent::Warning { .. })).count();
    assert_eq!(warnings, 1);
}

#[tokio::test]
async fn stop_ends_delegated_planning_promptly() {
    let mesh = Delegating::new(Behaviour::Hang);
    let local = Planner::new();
    let flag = CancelFlag::new();
    let engine = OllamaEngine::new("http://127.0.0.1:11434", "m", "v", "e").with_cancel(flag.clone());
    let sink = VecSink::new();
    let remote = RemotePlanner::new(&mesh, "gpu-box".into(), &local, &engine, &sink);

    let stopper = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flag.cancel();
    };
    let (r, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(plan_turn(&remote, "q", &[], "", false, &sink), stopper)
    })
    .await
    .expect("Stop was honoured while waiting on the Worker");

    assert!(matches!(r, Err(CoreError::Cancelled)));
    assert!(mesh.cancelled.load(Ordering::SeqCst), "the Worker was told");
    assert_eq!(local.calls(), 0);
}
