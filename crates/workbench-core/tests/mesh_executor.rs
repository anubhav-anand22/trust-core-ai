//! Mesh executor against an in-process fake Dispatcher. No model and no network:
//! the tools are fakes registered under real task names, so the stage rules and
//! file kinds are the registry's real ones.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use workbench_core::engine::schemas::{FileKind, InputFile, Plan, TaskStep, ToolResult};
use workbench_core::engine::OllamaEngine;
use workbench_core::events::{NullSink, VecSink};
use workbench_core::executor::{execute_plan, ToolRegistry};
use workbench_core::mesh::{execute_plan_mesh, run_job, Dispatcher, Node, StepJob};
use workbench_core::tools::{Tool, ToolContext};
use workbench_core::{CancelFlag, CoreError, PipelineConfig, ProgressSink, Result, StepEvent};

fn cfg() -> PipelineConfig {
    let dir = std::env::temp_dir().join("trustcore-mesh-tests");
    PipelineConfig {
        data_dir: dir.clone(),
        kb_dir: dir.join("kb"),
        ollama_url: "http://127.0.0.1:11434".into(),
        llm_model: "test-llm".into(),
        vision_model: "test-vision".into(),
        embed_model: "test-embed".into(),
        whisper_model_path: dir.join("models/ggml-base.en.bin"),
        ocr_detection_model: dir.join("models/text-detection.rten"),
        ocr_recognition_model: dir.join("models/text-recognition.rten"),
    }
}

fn engine() -> OllamaEngine {
    OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c")
}

fn step(id: &str, task: &str, deps: &[&str]) -> TaskStep {
    TaskStep {
        id: id.into(),
        task: task.into(),
        args: serde_json::Value::Null,
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
    }
}

fn file(name: &str, kind: FileKind) -> InputFile {
    InputFile { path: format!("/host/uploads/{name}"), kind, original_name: name.into() }
}

/// What a tool saw when it ran.
#[derive(Clone, Debug)]
struct Seen {
    step: String,
    outputs: Vec<String>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Stands in for a real tool: sleeps, records its context, returns its step id.
struct Fake {
    name: &'static str,
    delay_ms: u64,
    log: Log,
}

#[async_trait::async_trait]
impl Tool for Fake {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        let mut outputs: Vec<String> = ctx.outputs.keys().cloned().collect();
        outputs.sort();
        self.log.lock().unwrap().push(Seen { step: step.id.clone(), outputs });
        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name.into(),
            ok: true,
            data: json!({ "from": step.id }),
            error: None,
            warning: None,
            elapsed_ms: 0,
        })
    }
}

fn registry(log: &Log) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    for (name, delay_ms) in [
        ("parse_pdf", 120),
        ("ocr_image", 10),
        ("summarize", 5),
        ("answer_followup", 5),
    ] {
        r.register(Box::new(Fake { name, delay_ms, log: log.clone() }));
    }
    r
}

/// A mesh of fake Workers. Each runs jobs with the same fake tools, through the
/// real `run_job`, and records what it was sent and how many ran at once.
struct FakeMesh {
    candidates: HashMap<&'static str, Vec<Node>>,
    broken: HashSet<&'static str>,
    registry: ToolRegistry,
    config: PipelineConfig,
    engine: OllamaEngine,
    jobs: Mutex<Vec<(String, StepJob)>>,
    running: Mutex<HashMap<String, usize>>,
    max_on_one_worker: AtomicUsize,
    now_total: AtomicUsize,
    max_total: AtomicUsize,
    cancelled: AtomicBool,
}

impl FakeMesh {
    fn new(log: &Log, candidates: &[(&'static str, &[&str])], broken: &[&'static str]) -> Self {
        Self {
            candidates: candidates
                .iter()
                .map(|(task, ws)| (*task, ws.iter().map(|w| Node::Worker(w.to_string())).collect()))
                .collect(),
            broken: broken.iter().copied().collect(),
            registry: registry(log),
            config: cfg(),
            engine: engine(),
            jobs: Mutex::new(Vec::new()),
            running: Mutex::new(HashMap::new()),
            max_on_one_worker: AtomicUsize::new(0),
            now_total: AtomicUsize::new(0),
            max_total: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    fn job_for(&self, step_id: &str) -> StepJob {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|(_, j)| j.step.id == step_id)
            .map(|(_, j)| j.clone())
            .expect("job was sent")
    }
}

#[async_trait::async_trait]
impl Dispatcher for FakeMesh {
    fn candidates(&self, step: &TaskStep) -> Vec<Node> {
        self.candidates.get(step.task.as_str()).cloned().unwrap_or_default()
    }

    async fn run_on_worker(
        &self,
        worker: &str,
        job: StepJob,
        sink: &dyn ProgressSink,
    ) -> Result<ToolResult> {
        self.jobs.lock().unwrap().push((worker.to_string(), job.clone()));
        if self.broken.contains(worker) {
            return Err(CoreError::Ollama("worker dropped".into()));
        }
        {
            let mut running = self.running.lock().unwrap();
            let n = running.entry(worker.to_string()).or_default();
            *n += 1;
            self.max_on_one_worker.fetch_max(*n, Ordering::SeqCst);
        }
        let total = self.now_total.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_total.fetch_max(total, Ordering::SeqCst);

        let r = run_job(&job, &self.registry, &self.config, &self.engine, sink).await;

        self.now_total.fetch_sub(1, Ordering::SeqCst);
        *self.running.lock().unwrap().get_mut(worker).unwrap() -= 1;
        r
    }

    async fn cancel_all(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

fn plan(steps: Vec<TaskStep>) -> Plan {
    Plan { steps }
}

/// The comparable part of a result: elapsed time differs run to run.
fn shape(results: &[ToolResult]) -> Vec<(String, String, bool, serde_json::Value)> {
    results.iter().map(|r| (r.step_id.clone(), r.task.clone(), r.ok, r.data.clone())).collect()
}

#[tokio::test]
async fn with_no_workers_the_mesh_matches_the_sequential_executor() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [file("a.pdf", FileKind::Pdf), file("b.png", FileKind::Image)];
    let p = plan(vec![
        step("p1", "parse_pdf", &[]),
        step("o1", "ocr_image", &[]),
        step("s1", "summarize", &["p1", "o1"]),
    ]);
    let (config, engine) = (cfg(), engine());

    let sequential =
        execute_plan(&p, &reg, &uploads, &config, &engine, "q", "", &NullSink).await.unwrap();
    let mesh = FakeMesh::new(&log, &[], &[]);
    let meshed = execute_plan_mesh(&p, &reg, &uploads, &config, &engine, "q", "", &NullSink, &mesh)
        .await
        .unwrap();

    assert_eq!(shape(&meshed), shape(&sequential));
    assert!(mesh.jobs.lock().unwrap().is_empty(), "nothing left the Host");
}

#[tokio::test]
async fn independent_steps_run_at_once_but_never_two_on_one_worker() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [file("a.pdf", FileKind::Pdf), file("b.png", FileKind::Image)];
    let p = plan(vec![
        step("p1", "parse_pdf", &[]),
        step("p2", "parse_pdf", &[]),
        step("o1", "ocr_image", &[]),
        step("s1", "summarize", &["p1", "p2", "o1"]),
    ]);
    let ws: &[&str] = &["w1", "w2"];
    let mesh = FakeMesh::new(&log, &[("parse_pdf", ws), ("ocr_image", ws), ("summarize", ws)], &[]);

    let sink = VecSink::new();
    let results =
        execute_plan_mesh(&p, &reg, &uploads, &cfg(), &engine(), "q", "", &sink, &mesh)
            .await
            .unwrap();

    assert!(results.iter().all(|r| r.ok));
    assert_eq!(
        results.iter().map(|r| r.step_id.as_str()).collect::<Vec<_>>(),
        ["p1", "p2", "o1", "s1"],
        "results stay in plan order"
    );
    assert_eq!(mesh.max_total.load(Ordering::SeqCst), 2, "both workers busy at once");
    let on: Vec<String> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e {
            StepEvent::ToolFinished { device: Some(d), .. } => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(on.len(), 4, "every step names the worker that ran it: {on:?}");
    assert!(on.iter().all(|d| d == "w1" || d == "w2"));
    assert_eq!(mesh.max_on_one_worker.load(Ordering::SeqCst), 1, "one job per worker");
}

#[tokio::test]
async fn analysis_waits_for_every_earlier_step_and_sees_only_those() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [file("a.pdf", FileKind::Pdf), file("b.png", FileKind::Image)];
    // `s1` declares nothing, so the tool falls back to "every output so far".
    // `o2` comes after it in the plan and finishes first on its own worker:
    // the sequential executor would not have shown it to `s1`, so neither may we.
    let p = plan(vec![
        step("p1", "parse_pdf", &[]),
        step("s1", "summarize", &[]),
        step("o2", "ocr_image", &[]),
    ]);
    let mesh = FakeMesh::new(&log, &[("parse_pdf", &["w1"]), ("ocr_image", &["w2"])], &[]);

    execute_plan_mesh(&p, &reg, &uploads, &cfg(), &engine(), "q", "", &NullSink, &mesh)
        .await
        .unwrap();

    let seen = log.lock().unwrap().clone();
    let s1 = seen.iter().find(|s| s.step == "s1").unwrap();
    assert_eq!(s1.outputs, ["p1"]);
    let order: Vec<&str> = seen.iter().map(|s| s.step.as_str()).collect();
    assert!(
        order.iter().position(|s| *s == "o2") < order.iter().position(|s| *s == "p1"),
        "o2 really did finish before p1: {order:?}"
    );
}

#[tokio::test]
async fn a_dropped_worker_is_retried_elsewhere_then_on_the_host() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [file("a.pdf", FileKind::Pdf)];
    let p = plan(vec![step("p1", "parse_pdf", &[])]);
    let mesh = FakeMesh::new(&log, &[("parse_pdf", &["bad1", "bad2"])], &["bad1", "bad2"]);
    let sink = VecSink::new();

    let results = execute_plan_mesh(&p, &reg, &uploads, &cfg(), &engine(), "q", "", &sink, &mesh)
        .await
        .unwrap();

    assert!(results[0].ok, "the Host ran it");
    let finished: Vec<Option<String>> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e {
            StepEvent::ToolFinished { device, .. } => Some(device),
            _ => None,
        })
        .collect();
    assert_eq!(finished, [None], "run here: no device named, the pre-mesh JSON");
    let tried: Vec<String> = mesh.jobs.lock().unwrap().iter().map(|(w, _)| w.clone()).collect();
    assert_eq!(tried, ["bad1", "bad2"]);
    let warnings =
        sink.events().iter().filter(|e| matches!(e, StepEvent::Warning { .. })).count();
    assert_eq!(warnings, 2, "the user is told each time a device failed");
}

#[tokio::test]
async fn a_job_carries_only_its_own_file_and_the_history_only_when_read() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [
        file("a.pdf", FileKind::Pdf),
        file("b.pdf", FileKind::Pdf),
        file("c.png", FileKind::Image),
    ];
    let mut named = step("p2", "parse_pdf", &[]);
    named.args = json!({ "file": "b.pdf" });
    let p = plan(vec![
        named,
        step("o1", "ocr_image", &[]),
        step("f1", "answer_followup", &[]),
        step("s1", "summarize", &["p2", "o1"]),
    ]);
    let ws: &[&str] = &["w1"];
    let mesh = FakeMesh::new(
        &log,
        &[("parse_pdf", ws), ("ocr_image", ws), ("answer_followup", ws), ("summarize", ws)],
        &[],
    );

    execute_plan_mesh(&p, &reg, &uploads, &cfg(), &engine(), "q", "HISTORY", &NullSink, &mesh)
        .await
        .unwrap();

    let names = |j: &StepJob| j.files.iter().map(|f| f.original_name.clone()).collect::<Vec<_>>();
    let p2 = mesh.job_for("p2");
    assert_eq!(names(&p2), ["b.pdf"], "the named attachment, not the first pdf");
    assert_eq!(p2.session_blob, "");
    assert!(p2.dep_outputs.is_empty());
    assert_eq!(names(&mesh.job_for("o1")), ["c.png"]);
    let f1 = mesh.job_for("f1");
    assert_eq!(f1.session_blob, "HISTORY");
    assert!(f1.files.is_empty());
    let s1 = mesh.job_for("s1");
    assert!(s1.files.is_empty() && s1.session_blob.is_empty());
    let mut deps: Vec<&String> = s1.dep_outputs.keys().collect();
    deps.sort();
    assert_eq!(deps, ["f1", "o1", "p2"], "analysis gets every earlier output");
}

#[tokio::test]
async fn stop_ends_the_mesh_run_and_cancels_the_workers() {
    let log: Log = Default::default();
    let reg = registry(&log);
    let uploads = [file("a.pdf", FileKind::Pdf)];
    let p = plan(vec![step("p1", "parse_pdf", &[])]);
    let mesh = FakeMesh::new(&log, &[("parse_pdf", &["w1"])], &[]);
    let flag = CancelFlag::new();
    let engine = engine().with_cancel(flag.clone());
    flag.cancel();

    let r = execute_plan_mesh(&p, &reg, &uploads, &cfg(), &engine, "q", "", &NullSink, &mesh).await;

    assert!(matches!(r, Err(CoreError::Cancelled)));
    assert!(mesh.cancelled.load(Ordering::SeqCst));
    assert!(mesh.jobs.lock().unwrap().is_empty(), "nothing dispatched after Stop");
}
