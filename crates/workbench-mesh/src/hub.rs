//! The Host side at runtime: accept Workers, keep them alive, and hand them jobs.
//!
//! One [`Hub`] per Host. For each turn the host builds a [`TurnDispatcher`] (the
//! hub plus that turn's attachments) and passes it to
//! `workbench_core::run_turn_with_mesh`. The executor asks it where each step
//! should go and calls it to run one on a Worker.
//!
//! Per connected Worker there are three tasks: a reader (frames off the link
//! into a channel, because a frame read is not safe to interrupt halfway), a
//! writer (one place that sends, so frames never interleave), and a supervisor
//! (routes frames to waiting jobs, pings, and declares the Worker gone when it
//! stops answering).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use workbench_core::mesh::{Dispatcher, Node, PlanJob, PlanReply, StepJob};
use workbench_core::schemas::{InputFile, TaskStep, ToolResult};
use workbench_core::{CoreError, ProgressSink, StepEvent};

use crate::code::{PairingCode, PairingWindow, DEFAULT_MAX_FAILURES, DEFAULT_TTL};
use crate::frame::{FileBlob, Frame, Hello};
use crate::identity::{DeviceIdentity, TrustStore};
use crate::link::{accept, Link, Peer};
use crate::scheduler::{self, Capabilities, WorkerView};

/// How often the Host pings each Worker, and how long silence may last before
/// the Worker counts as gone and its jobs move elsewhere (MESH-7).
pub const PING_EVERY: Duration = Duration::from_secs(5);
pub const SILENCE_LIMIT: Duration = Duration::from_secs(20);

/// Handshakes the Host runs at once (see [`Hub::serve`]). A real room has a
/// handful of devices joining, not sixteen in the same 15 seconds.
pub const MAX_HANDSHAKES: usize = 16;

/// Size of the one-off bandwidth probe sent at join.
const PROBE_BYTES: usize = 1024 * 1024;
const PROBE_ID: u64 = u64::MAX;

/// What the UI hears about.
#[derive(Clone, Debug, PartialEq)]
pub enum HubEvent {
    /// Paired with the code just now: show the consent screen (MESH-4).
    ConsentNeeded { device_id: String, name: String },
    Joined { device_id: String, name: String },
    Left { device_id: String, reason: String },
}

/// A Worker as the UI lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerStatus {
    pub device_id: String,
    pub name: String,
    pub caps: Capabilities,
    pub link_mbps: Option<f64>,
    pub consented: bool,
    pub busy: bool,
}

/// What a Worker answers: a step's result, or a planner call's.
enum Reply {
    Tool(ToolResult),
    Plan(PlanReply),
}

type JobOutcome = std::result::Result<Reply, String>;

struct Waiter {
    task: String,
    events: mpsc::UnboundedSender<StepEvent>,
    done: Option<oneshot::Sender<JobOutcome>>,
}

struct Slot {
    peer: Peer,
    caps: Capabilities,
    link_mbps: Option<f64>,
    consented: bool,
    tx: mpsc::UnboundedSender<Frame>,
    jobs: HashMap<String, Waiter>,
}

struct Inner {
    me: DeviceIdentity,
    host_caps: Mutex<Capabilities>,
    window: Mutex<Option<PairingWindow>>,
    trust: Mutex<TrustStore>,
    slots: Mutex<HashMap<String, Slot>>,
    pinned: Mutex<Option<String>>,
    next_job: AtomicU64,
    events: broadcast::Sender<HubEvent>,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new(me: DeviceIdentity, host_caps: Capabilities, trust: TrustStore) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Inner {
                me,
                host_caps: Mutex::new(host_caps),
                window: Mutex::new(None),
                trust: Mutex::new(trust),
                slots: Mutex::new(HashMap::new()),
                pinned: Mutex::new(None),
                next_job: AtomicU64::new(1),
                events,
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.inner.events.subscribe()
    }

    /// Open a fresh pairing code (replacing any open one).
    pub fn open_pairing(&self) -> PairingCode {
        self.open_pairing_with(DEFAULT_TTL, DEFAULT_MAX_FAILURES)
    }

    pub fn open_pairing_with(&self, ttl: Duration, max_failures: u8) -> PairingCode {
        let code = PairingCode::generate();
        *self.inner.window.lock().expect("window lock") =
            Some(PairingWindow::open(code.clone(), ttl, max_failures));
        code
    }

    pub fn close_pairing(&self) {
        *self.inner.window.lock().expect("window lock") = None;
    }

    /// Time left on the open code, or `None` when pairing is closed.
    pub fn pairing_remaining(&self) -> Option<Duration> {
        let guard = self.inner.window.lock().expect("window lock");
        guard.as_ref().filter(|w| w.check().is_ok()).map(|w| w.remaining())
    }

    pub fn set_host_caps(&self, caps: Capabilities) {
        *self.inner.host_caps.lock().expect("caps lock") = caps;
    }

    pub fn host_caps(&self) -> Capabilities {
        self.inner.host_caps.lock().expect("caps lock").clone()
    }

    /// Accept Workers until the listener fails. Each handshake runs on its own
    /// task, so a slow or hostile client never blocks the next one.
    pub async fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        // Handshakes run before the peer has proved anything. Bound how many at
        // once, so a burst of silent connections (each may hold for the 15 s
        // handshake timeout) can't pile up. Over the bound, a connection is
        // closed at once.
        let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
        loop {
            let (tcp, from) = listener.accept().await?;
            let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                tracing::warn!(%from, "mesh: too many devices joining at once; dropped one");
                drop(tcp);
                continue;
            };
            let hub = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let hello = Hello {
                    name: hub.inner.me.name.clone(),
                    capabilities: serde_json::to_value(hub.host_caps()).unwrap_or_default(),
                };
                let i = &hub.inner;
                match accept(tcp, &i.me, &hello, &i.window, &i.trust).await {
                    Ok(a) => hub.register(a.link, a.newly_paired),
                    Err(e) => tracing::warn!(%from, error = %e, "mesh: a device failed to join"),
                }
            });
        }
    }

    /// The Host user approved a newly paired device: remember it and give it work.
    pub fn approve(&self, device_id: &str) -> crate::Result<()> {
        let mut slots = self.inner.slots.lock().expect("slots lock");
        let slot = slots
            .get_mut(device_id)
            .ok_or_else(|| crate::MeshError::Rejected("that device is no longer connected".into()))?;
        self.inner.trust.lock().expect("trust lock").add(&slot.peer.name, &slot.peer.public_key)?;
        slot.consented = true;
        let _ = self.inner.events.send(HubEvent::Joined {
            device_id: device_id.to_string(),
            name: slot.peer.name.clone(),
        });
        Ok(())
    }

    /// The Host user rejected a newly paired device, or removed a paired one:
    /// forget it and disconnect it.
    pub fn remove(&self, device_id: &str, reason: &str) -> crate::Result<()> {
        self.inner.trust.lock().expect("trust lock").remove(device_id)?;
        if let Some(slot) = self.inner.slots.lock().expect("slots lock").get(device_id) {
            let _ = slot.tx.send(Frame::Bye { reason: reason.to_string() });
        }
        Ok(())
    }

    pub fn pin_delegator(&self, device_id: Option<String>) {
        *self.inner.pinned.lock().expect("pin lock") = device_id;
    }

    pub fn pinned(&self) -> Option<String> {
        self.inner.pinned.lock().expect("pin lock").clone()
    }

    pub fn device_id(&self) -> String {
        self.inner.me.device_id()
    }

    /// Stop hosting: close pairing and say goodbye to every Worker. Each
    /// supervisor then ends on its own and the Workers return to idle.
    pub fn shutdown(&self, reason: &str) {
        self.close_pairing();
        for slot in self.inner.slots.lock().expect("slots lock").values() {
            let _ = slot.tx.send(Frame::Bye { reason: reason.to_string() });
        }
    }

    pub fn workers(&self) -> Vec<WorkerStatus> {
        let slots = self.inner.slots.lock().expect("slots lock");
        let mut list: Vec<WorkerStatus> = slots
            .iter()
            .map(|(id, s)| WorkerStatus {
                device_id: id.clone(),
                name: s.peer.name.clone(),
                caps: s.caps.clone(),
                link_mbps: s.link_mbps,
                consented: s.consented,
                busy: !s.jobs.is_empty(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Consented Workers, for the scheduler.
    fn views(&self) -> Vec<WorkerView> {
        let slots = self.inner.slots.lock().expect("slots lock");
        let mut views: Vec<WorkerView> = slots
            .iter()
            .filter(|(_, s)| s.consented)
            .map(|(id, s)| WorkerView { device_id: id.clone(), caps: s.caps.clone(), link_mbps: s.link_mbps })
            .collect();
        views.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        views
    }

    /// Who should plan this turn (`None` = the Host). Used by delegated planning.
    pub fn delegator(&self) -> Option<String> {
        let pinned = self.inner.pinned.lock().expect("pin lock").clone();
        scheduler::delegator(&self.host_caps(), &self.views(), pinned.as_deref())
    }

    /// A dispatcher for one turn's attachments.
    pub fn for_turn(&self, uploads: &[InputFile]) -> TurnDispatcher {
        TurnDispatcher { hub: self.clone(), uploads: uploads.to_vec() }
    }

    fn register(&self, link: Link, newly_paired: bool) {
        let (mut writer, mut reader, peer) = link.split();
        let device_id = peer.device_id.clone();
        let caps: Capabilities = serde_json::from_value(peer.capabilities.clone()).unwrap_or_default();
        let (tx, mut outbox) = mpsc::unbounded_channel::<Frame>();
        let (frames_tx, mut frames) = mpsc::unbounded_channel::<std::result::Result<Frame, String>>();

        // A device that reconnects replaces its old slot; the old supervisor
        // notices its channel is gone and exits.
        let old = self.inner.slots.lock().expect("slots lock").insert(
            device_id.clone(),
            Slot { peer: peer.clone(), caps, link_mbps: None, consented: !newly_paired, tx: tx.clone(), jobs: HashMap::new() },
        );
        if let Some(old) = old {
            fail_all(old.jobs, "the device reconnected");
        }
        let _ = self.inner.events.send(if newly_paired {
            HubEvent::ConsentNeeded { device_id: device_id.clone(), name: peer.name.clone() }
        } else {
            HubEvent::Joined { device_id: device_id.clone(), name: peer.name.clone() }
        });

        let writer_task = tokio::spawn(async move {
            while let Some(frame) = outbox.recv().await {
                let bye = matches!(frame, Frame::Bye { .. });
                if writer.send(&frame).await.is_err() || bye {
                    break;
                }
            }
            writer.close("host closed the link").await;
        });
        // The error is passed on, not swallowed: "the connection closed" and
        // "a frame failed to decode" need different fixes.
        let reader_task = tokio::spawn(async move {
            loop {
                let next = reader.recv().await.map_err(|e| e.to_string());
                let failed = next.is_err();
                if frames_tx.send(next).is_err() || failed {
                    break;
                }
            }
        });

        let hub = self.clone();
        tokio::spawn(async move {
            // Rough link speed: 1 MiB out, a tiny Pong back. Includes the Worker
            // decoding it, so it errs low, the safe side for the transfer budget.
            let probe_sent = Instant::now();
            let _ = tx.send(Frame::Probe { n: PROBE_ID, padding: vec![0u8; PROBE_BYTES] });
            let mut last_heard = Instant::now();
            let mut tick = tokio::time::interval(PING_EVERY);
            let mut n = 0u64;
            let reason = loop {
                tokio::select! {
                    frame = frames.recv() => {
                        let frame = match frame {
                            Some(Ok(frame)) => frame,
                            Some(Err(e)) => break e,
                            None => break "the connection closed".to_string(),
                        };
                        last_heard = Instant::now();
                        if let Some(reason) = hub.on_frame(&device_id, frame, probe_sent) {
                            break reason;
                        }
                    }
                    _ = tick.tick() => {
                        if last_heard.elapsed() > SILENCE_LIMIT {
                            break "stopped answering".to_string();
                        }
                        n += 1;
                        if tx.send(Frame::Ping { n }).is_err() {
                            break "the connection closed".to_string();
                        }
                    }
                }
            };
            reader_task.abort();
            writer_task.abort();
            hub.unregister(&device_id, &tx, &reason);
        });
    }

    /// Route one frame from a Worker. `Some(reason)` ends the connection.
    fn on_frame(&self, device_id: &str, frame: Frame, probe_sent: Instant) -> Option<String> {
        let mut slots = self.inner.slots.lock().expect("slots lock");
        let slot = slots.get_mut(device_id)?;
        match frame {
            Frame::Done { job_id, result, timings } => {
                // Same fields as the engine's own `model call timing` line, plus
                // the device, so one log shows a whole mesh turn.
                for t in &timings {
                    tracing::info!(
                        device = %slot.peer.name,
                        role = %t.role,
                        model = %t.model,
                        wall_ms = t.wall_ms,
                        load_ms = ?t.load_ms,
                        prompt_tokens = ?t.prompt_tokens,
                        prefill_ms = ?t.prefill_ms,
                        gen_tokens = ?t.gen_tokens,
                        decode_ms = ?t.decode_ms,
                        decode_tok_per_s = ?t.decode_tok_per_s().map(|v| (v * 10.0).round() / 10.0),
                        "mesh model call timing"
                    );
                }
                if let Some(mut w) = slot.jobs.remove(&job_id) {
                    if let Some(done) = w.done.take() {
                        let _ = done.send(Ok(Reply::Tool(result)));
                    }
                }
            }
            Frame::Planned { job_id, reply } => {
                if let Some(mut w) = slot.jobs.remove(&job_id) {
                    if let Some(done) = w.done.take() {
                        let _ = done.send(Ok(Reply::Plan(reply)));
                    }
                }
            }
            Frame::Failed { job_id, message } => {
                if let Some(mut w) = slot.jobs.remove(&job_id) {
                    if let Some(done) = w.done.take() {
                        let _ = done.send(Err(message));
                    }
                }
            }
            Frame::Event { job_id, event } => {
                if let Some(w) = slot.jobs.get(&job_id) {
                    if allowed(&w.task, &event) {
                        let _ = w.events.send(event);
                    }
                }
            }
            Frame::Pong { n } if n == PROBE_ID => {
                let secs = probe_sent.elapsed().as_secs_f64().max(1e-4);
                slot.link_mbps = Some((PROBE_BYTES as f64 * 8.0) / secs / 1e6);
            }
            Frame::Ping { n } => {
                let _ = slot.tx.send(Frame::Pong { n });
            }
            Frame::Bye { reason } => return Some(reason),
            Frame::Pong { .. } => {}
            other => {
                tracing::warn!(device = device_id, frame = frame_name(&other), "mesh: unexpected frame from a worker");
            }
        }
        None
    }

    fn unregister(&self, device_id: &str, tx: &mpsc::UnboundedSender<Frame>, reason: &str) {
        let mut slots = self.inner.slots.lock().expect("slots lock");
        // Only remove the slot this supervisor owns, not a newer reconnection.
        if slots.get(device_id).is_some_and(|s| s.tx.same_channel(tx)) {
            if let Some(slot) = slots.remove(device_id) {
                fail_all(slot.jobs, reason);
            }
            tracing::info!(device = device_id, reason, "mesh: device left");
            let _ = self
                .inner
                .events
                .send(HubEvent::Left { device_id: device_id.to_string(), reason: reason.to_string() });
        }
    }

    /// Send one frame that expects one reply, and wait for it, forwarding the
    /// job's allowed events to `sink` meanwhile.
    async fn request(
        &self,
        worker: &str,
        job_id: String,
        task: String,
        frame: Frame,
        sink: Option<&dyn ProgressSink>,
    ) -> JobOutcome {
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (done_tx, mut done) = oneshot::channel();
        {
            let mut slots = self.inner.slots.lock().expect("slots lock");
            let slot = slots
                .get_mut(worker)
                .filter(|s| s.consented)
                .ok_or_else(|| "that device is no longer connected".to_string())?;
            slot.jobs.insert(job_id.clone(), Waiter { task, events: events_tx, done: Some(done_tx) });
            slot.tx.send(frame).map_err(|_| "the connection closed".to_string())?;
        }
        tracing::info!(worker, job = %job_id, "mesh: job sent");
        let _forget = ForgetOnDrop { hub: self, worker, job_id: &job_id };

        let emit = |event| {
            if let Some(sink) = sink {
                sink.emit(event);
            }
        };
        loop {
            tokio::select! {
                event = events.recv() => {
                    if let Some(event) = event { emit(event); }
                }
                outcome = &mut done => {
                    // Flush what arrived before the result.
                    while let Ok(event) = events.try_recv() { emit(event); }
                    return outcome.unwrap_or_else(|_| Err("the device left mid-job".into()));
                }
            }
        }
    }

    async fn run_plan(&self, worker: &str, job: PlanJob) -> std::result::Result<PlanReply, String> {
        let job_id = format!("plan#{}", self.inner.next_job.fetch_add(1, Ordering::Relaxed));
        let frame = Frame::PlanJob { job_id: job_id.clone(), job };
        match self.request(worker, job_id, "plan".into(), frame, None).await? {
            Reply::Plan(reply) => Ok(reply),
            Reply::Tool(_) => Err("answered a planner call with a step result".into()),
        }
    }

    async fn run_job(&self, worker: &str, mut job: StepJob, sink: &dyn ProgressSink) -> std::result::Result<ToolResult, String> {
        // The Worker gets the bytes and the file's name, never the Host's path
        // (it would leak the user's home folder and username).
        let mut files = Vec::with_capacity(job.files.len());
        for f in &mut job.files {
            let bytes = tokio::fs::read(&f.path).await.map_err(|e| format!("reading {}: {e}", f.original_name))?;
            files.push(FileBlob { original_name: f.original_name.clone(), kind: f.kind, bytes });
            f.path = f.original_name.clone();
        }

        let job_id = format!("{}#{}", job.step.id, self.inner.next_job.fetch_add(1, Ordering::Relaxed));
        let task = job.step.task.clone();
        let frame = Frame::Job { job_id: job_id.clone(), job, files };
        match self.request(worker, job_id, task, frame, Some(sink)).await? {
            Reply::Tool(result) => Ok(result),
            Reply::Plan(_) => Err("answered a step with a planner reply".into()),
        }
    }

    fn cancel_everything(&self) {
        for slot in self.inner.slots.lock().expect("slots lock").values() {
            let _ = slot.tx.send(Frame::Cancel { job_id: None });
        }
    }
}

/// When the executor gives up on a job (Stop, or the turn ended), stop routing
/// its events and tell the Worker.
struct ForgetOnDrop<'a> {
    hub: &'a Hub,
    worker: &'a str,
    job_id: &'a str,
}

impl Drop for ForgetOnDrop<'_> {
    fn drop(&mut self) {
        let mut slots = self.hub.inner.slots.lock().expect("slots lock");
        if let Some(slot) = slots.get_mut(self.worker) {
            if slot.jobs.remove(self.job_id).is_some() {
                let _ = slot.tx.send(Frame::Cancel { job_id: Some(self.job_id.to_string()) });
            }
        }
    }
}

fn fail_all(jobs: HashMap<String, Waiter>, reason: &str) {
    for (_, mut w) in jobs {
        if let Some(done) = w.done.take() {
            let _ = done.send(Err(format!("device disconnected: {reason}")));
        }
    }
}

/// A Worker may show live text, warnings and its own deep-read progress, never
/// a stage change: it can't fake "Done", "Awaiting you" or another step.
fn allowed(task: &str, event: &StepEvent) -> bool {
    match event {
        StepEvent::Streaming { .. } | StepEvent::Warning { .. } => true,
        StepEvent::ExecutingTool { tool, .. } => tool.starts_with(&format!("{task} · ")),
        _ => false,
    }
}

fn frame_name(f: &Frame) -> &'static str {
    match f {
        Frame::Hello(_) => "hello",
        Frame::Job { .. } => "job",
        Frame::Event { .. } => "event",
        Frame::Done { .. } => "done",
        Frame::Failed { .. } => "failed",
        Frame::PlanJob { .. } => "plan_job",
        Frame::Planned { .. } => "planned",
        Frame::Cancel { .. } => "cancel",
        Frame::Ping { .. } => "ping",
        Frame::Pong { .. } => "pong",
        Frame::Probe { .. } => "probe",
        Frame::Bye { .. } => "bye",
    }
}

/// The [`Hub`] plus one turn's attachments: what `run_turn_with_mesh` takes.
pub struct TurnDispatcher {
    hub: Hub,
    uploads: Vec<InputFile>,
}

impl TurnDispatcher {
    fn file_bytes(&self, step: &TaskStep) -> u64 {
        workbench_core::planner::lookup(&step.task)
            .and_then(|spec| spec.requires_file)
            .and_then(|kind| workbench_core::tools::resolve_file(&self.uploads, step, kind))
            .and_then(|f| std::fs::metadata(&f.path).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl Dispatcher for TurnDispatcher {
    fn candidates(&self, step: &TaskStep) -> Vec<Node> {
        scheduler::rank(&step.task, self.file_bytes(step), &self.hub.host_caps(), &self.hub.views())
    }

    async fn run_on_worker(
        &self,
        worker: &str,
        job: StepJob,
        sink: &dyn ProgressSink,
    ) -> workbench_core::Result<ToolResult> {
        self.hub
            .run_job(worker, job, sink)
            .await
            .map_err(|message| CoreError::Tool { tool: format!("mesh:{worker}"), message })
    }

    async fn cancel_all(&self) {
        self.hub.cancel_everything();
    }

    fn delegator(&self) -> Option<String> {
        self.hub.delegator()
    }

    fn device_name(&self, worker: &str) -> Option<String> {
        self.hub.inner.slots.lock().expect("slots lock").get(worker).map(|s| s.peer.name.clone())
    }

    async fn plan_on_worker(&self, worker: &str, job: PlanJob) -> workbench_core::Result<PlanReply> {
        self.hub
            .run_plan(worker, job)
            .await
            .map_err(|message| CoreError::Tool { tool: format!("mesh:{worker}"), message })
    }
}
