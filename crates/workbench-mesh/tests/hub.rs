//! Host hub + Worker runtime + the core mesh executor, end to end over loopback.
//! Tools are fakes registered under real task names; no model, no Ollama.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::net::TcpListener;
use workbench_core::engine::OllamaEngine;
use workbench_core::events::VecSink;
use workbench_core::executor::ToolRegistry;
use workbench_core::mesh::execute_plan_mesh;
use workbench_core::schemas::{FileKind, InputFile, Plan, TaskStep, ToolResult};
use workbench_core::tools::{Tool, ToolContext};
use workbench_core::{CancelFlag, CoreError, PipelineConfig, Result, StepEvent};
use workbench_mesh::frame::Hello;
use workbench_mesh::hub::{Hub, HubEvent};
use workbench_mesh::identity::{DeviceIdentity, TrustStore};
use workbench_mesh::link::pair;
use workbench_mesh::scheduler::Capabilities;
use workbench_mesh::worker::{serve, WorkerRuntime};

fn cfg(dir: &std::path::Path) -> PipelineConfig {
    PipelineConfig {
        data_dir: dir.to_path_buf(),
        kb_dir: dir.join("kb"),
        ollama_url: "http://127.0.0.1:11434".into(),
        llm_model: "m".into(),
        vision_model: "v".into(),
        embed_model: "e".into(),
        whisper_model_path: dir.join("w.bin"),
        ocr_detection_model: dir.join("d.rten"),
        ocr_recognition_model: dir.join("r.rten"),
    }
}

fn engine() -> OllamaEngine {
    OllamaEngine::new("http://127.0.0.1:11434", "m", "v", "e")
}

/// Reads its attachment and reports where it ran and what it saw.
struct FakePdf {
    ran_on: &'static str,
    /// Polls the cancel flag this long before finishing.
    work: Duration,
    saw_cancel: Arc<AtomicBool>,
    saw_path: Arc<Mutex<String>>,
}

#[async_trait::async_trait]
impl Tool for FakePdf {
    fn name(&self) -> &'static str {
        "parse_pdf"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let file = ctx.file_for(step, FileKind::Pdf).expect("the attachment came with the job");
        *self.saw_path.lock().unwrap() = file.path.clone();
        let bytes = std::fs::read(&file.path).expect("the file is on this device");
        ctx.sink.emit(StepEvent::Streaming { tool: "parse_pdf".into(), text: "reading…".into() });
        // A Worker must not be able to put this in the Host's UI.
        ctx.sink.emit(StepEvent::Done { report_json: "{}".into() });

        let until = std::time::Instant::now() + self.work;
        while std::time::Instant::now() < until {
            if ctx.engine.cancel_check().is_err() {
                self.saw_cancel.store(true, Ordering::SeqCst);
                return Err(CoreError::Cancelled);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(ToolResult {
            step_id: step.id.clone(),
            task: "parse_pdf".into(),
            ok: true,
            data: json!({ "ran_on": self.ran_on, "bytes": bytes.len(), "prompt": ctx.prompt }),
            error: None,
            warning: None,
            elapsed_ms: 0,
        })
    }
}

struct Probe {
    saw_cancel: Arc<AtomicBool>,
    saw_path: Arc<Mutex<String>>,
}

fn registry(ran_on: &'static str, work: Duration, probe: &Probe) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(FakePdf {
        ran_on,
        work,
        saw_cancel: probe.saw_cancel.clone(),
        saw_path: probe.saw_path.clone(),
    }));
    r
}

struct Mesh {
    hub: Hub,
    worker_task: tokio::task::JoinHandle<String>,
    worker_scratch: std::path::PathBuf,
    probe: Probe,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// A GPU-less Host and one paired Worker. `approve` = the Host user consented.
async fn mesh(approve: bool, worker_work: Duration) -> Mesh {
    let host_caps = Capabilities { physical_cores: 2, ..Default::default() };
    let hub = Hub::new(DeviceIdentity::generate("host").unwrap(), host_caps, TrustStore::in_memory());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = hub.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    let code = hub.open_pairing();
    let mut events = hub.subscribe();

    let me = DeviceIdentity::generate("gpu-box").unwrap();
    let caps = Capabilities { physical_cores: 16, gpu: true, ..Default::default() };
    let hello = Hello { name: me.name.clone(), capabilities: serde_json::to_value(caps).unwrap() };
    let link = pair(addr, &code, &me, &hello).await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    let Ok(Ok(HubEvent::ConsentNeeded { device_id, .. })) = first else {
        panic!("a new device needs consent first, got {first:?}")
    };
    if approve {
        hub.approve(&device_id).unwrap();
    }

    let worker_dir = tempfile::tempdir().unwrap();
    let probe = Probe { saw_cancel: Default::default(), saw_path: Default::default() };
    let scratch = worker_dir.path().join("scratch");
    let rt = Arc::new(WorkerRuntime {
        registry: registry("worker", worker_work, &probe),
        config: cfg(worker_dir.path()),
        scratch: scratch.clone(),
        make_engine: Arc::new(|job, flag| {
            engine().with_mode(job.mode).with_persona(job.persona.clone()).with_cancel(flag)
        }),
    });
    let worker_task = tokio::spawn(serve(link, rt, None));
    // Let the join-time probe settle.
    tokio::time::sleep(Duration::from_millis(100)).await;

    Mesh { hub, worker_task, worker_scratch: scratch, probe, _dirs: (worker_dir, tempfile::tempdir().unwrap()) }
}

fn pdf_turn(dir: &std::path::Path) -> (Plan, Vec<InputFile>) {
    let path = dir.join("inspection.pdf");
    std::fs::write(&path, vec![7u8; 4096]).unwrap();
    let uploads = vec![InputFile {
        path: path.to_string_lossy().into_owned(),
        kind: FileKind::Pdf,
        original_name: "inspection.pdf".into(),
    }];
    let plan = Plan {
        steps: vec![TaskStep { id: "p1".into(), task: "parse_pdf".into(), args: json!(null), depends_on: vec![] }],
    };
    (plan, uploads)
}

#[tokio::test]
async fn an_approved_worker_runs_the_step_with_only_its_file() {
    let m = mesh(true, Duration::ZERO).await;
    let host_dir = tempfile::tempdir().unwrap();
    let (plan, uploads) = pdf_turn(host_dir.path());
    let host_probe = Probe { saw_cancel: Default::default(), saw_path: Default::default() };
    let host_registry = registry("host", Duration::ZERO, &host_probe);
    let sink = VecSink::new();

    let results = execute_plan_mesh(
        &plan, &host_registry, &uploads, &cfg(host_dir.path()), &engine(), "is the pump ok?", "", &sink,
        &m.hub.for_turn(&uploads),
    )
    .await
    .unwrap();

    assert_eq!(results[0].data["ran_on"], "worker", "events: {:?}", sink.events());
    assert_eq!(results[0].data["bytes"], 4096, "the file's bytes travelled");
    assert_eq!(results[0].data["prompt"], "is the pump ok?");
    let worker_path = m.probe.saw_path.lock().unwrap().clone();
    assert!(worker_path.starts_with(m.worker_scratch.to_str().unwrap()), "{worker_path}");
    assert!(!worker_path.contains(host_dir.path().to_str().unwrap()), "the Host's path never leaves it");

    // Only the Worker ran a tool here (asserted above), so any `Done` would be its forgery.
    let events = sink.events();
    assert!(events.iter().any(|e| matches!(e, StepEvent::Streaming { text, .. } if text == "reading…")));
    assert!(!events.iter().any(|e| matches!(e, StepEvent::Done { .. })), "a Worker can't fake a stage");
    assert!(!events.iter().any(|e| matches!(e, StepEvent::Warning { .. })), "no fallback happened");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(std::fs::read_dir(&m.worker_scratch).unwrap().count(), 0, "the Worker kept nothing");
    let w = &m.hub.workers()[0];
    assert!(w.link_mbps.is_some_and(|mbps| mbps > 0.0), "link speed measured at join");
}

#[tokio::test]
async fn a_worker_waiting_for_consent_gets_no_work() {
    let m = mesh(false, Duration::ZERO).await;
    let host_dir = tempfile::tempdir().unwrap();
    let (plan, uploads) = pdf_turn(host_dir.path());
    let host_probe = Probe { saw_cancel: Default::default(), saw_path: Default::default() };

    let results = execute_plan_mesh(
        &plan, &registry("host", Duration::ZERO, &host_probe), &uploads, &cfg(host_dir.path()), &engine(),
        "q", "", &VecSink::new(), &m.hub.for_turn(&uploads),
    )
    .await
    .unwrap();

    assert_eq!(results[0].data["ran_on"], "host");
    assert!(m.probe.saw_path.lock().unwrap().is_empty(), "nothing reached the Worker");
}

#[tokio::test]
async fn a_worker_that_vanishes_mid_job_leaves_the_step_to_the_host() {
    let m = mesh(true, Duration::from_secs(30)).await;
    let host_dir = tempfile::tempdir().unwrap();
    let (plan, uploads) = pdf_turn(host_dir.path());
    let host_probe = Probe { saw_cancel: Default::default(), saw_path: Default::default() };
    let host_registry = registry("host", Duration::ZERO, &host_probe);
    let dispatcher = m.hub.for_turn(&uploads);
    let config = cfg(host_dir.path());
    let engine = engine();
    let sink = VecSink::new();

    let run = execute_plan_mesh(&plan, &host_registry, &uploads, &config, &engine, "q", "", &sink, &dispatcher);
    let pull_the_cable = async {
        while m.probe.saw_path.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        m.worker_task.abort();
    };
    let (results, ()) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(run, pull_the_cable) })
        .await
        .expect("the turn did not hang on a dead Worker");

    assert_eq!(results.unwrap()[0].data["ran_on"], "host");
    assert!(sink.events().iter().any(|e| matches!(e, StepEvent::Warning { .. })), "the user is told");
}

#[tokio::test]
async fn stop_reaches_the_job_running_on_the_worker() {
    let m = mesh(true, Duration::from_secs(30)).await;
    let host_dir = tempfile::tempdir().unwrap();
    let (plan, uploads) = pdf_turn(host_dir.path());
    let host_probe = Probe { saw_cancel: Default::default(), saw_path: Default::default() };
    let host_registry = registry("host", Duration::ZERO, &host_probe);
    let dispatcher = m.hub.for_turn(&uploads);
    let config = cfg(host_dir.path());
    let flag = CancelFlag::new();
    let engine = engine().with_cancel(flag.clone());
    let sink = VecSink::new();

    let run = execute_plan_mesh(&plan, &host_registry, &uploads, &config, &engine, "q", "", &sink, &dispatcher);
    let press_stop = async {
        while m.probe.saw_path.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        flag.cancel();
    };
    let (r, ()) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, press_stop) })
        .await
        .expect("Stop ended the turn promptly");
    assert!(matches!(r, Err(CoreError::Cancelled)));

    let mut waited = 0;
    while !m.probe.saw_cancel.load(Ordering::SeqCst) && waited < 100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    assert!(m.probe.saw_cancel.load(Ordering::SeqCst), "the Worker's tool saw the cancel");
}
