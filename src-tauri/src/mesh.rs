//! The mesh in the desktop app: Tauri commands over `workbench-mesh`.
//!
//! One role at a time: off, Host or Worker. The mesh is **off at every launch**
//! and nothing listens on the network until the user picks a role in the mesh
//! sidebar. Paired devices are remembered, so turning it on again reconnects
//! them without a code. Design and decisions: docs/12-mesh.md.
//!
//! Files, under `<app data>/mesh/`: `identity.json` (this device's key),
//! `workers.json` (Workers this Host trusts), `hosts.json` (Hosts this Worker
//! trusts), `scratch/` (a Worker's per-job temp folders).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Manager, State};
use tokio::net::TcpListener;
use workbench_core::engine::{OllamaEngine, ResourceLimits};
use workbench_core::schemas::InputFile;
use workbench_core::planner::PlanSource;
use workbench_core::tools::default_registry;
use workbench_core::{CancelFlag, TurnMode};
use workbench_mesh::code::PairingCode;
use workbench_mesh::discovery::{find_host_by_id, find_hosts, Advertisement};
use workbench_mesh::frame::Hello;
use workbench_mesh::hub::{Hub, TurnDispatcher};
use workbench_mesh::identity::{DeviceIdentity, TrustStore};
use workbench_mesh::link::{pair, reconnect, Link};
use workbench_mesh::scheduler::Capabilities;
use workbench_mesh::worker::{serve, Activity, WorkerRuntime};

use crate::bootstrap::hardware::detect_hardware;
use crate::bootstrap::models::ModelPlan;
use crate::{pipeline_config, AppState, OLLAMA_URL};

/// How long a Worker looks for the Host on the LAN before suggesting the address.
const FIND_HOST_FOR: Duration = Duration::from_secs(6);

#[derive(Default)]
pub struct MeshState(tokio::sync::Mutex<Role>);

#[derive(Default)]
enum Role {
    #[default]
    Off,
    Host(HostRole),
    Worker(WorkerRole),
}

struct HostRole {
    hub: Hub,
    port: u16,
    code: String,
    /// Held for its Drop: ending the role stops listening.
    _serve: Abort,
    advert: Option<Advertisement>,
}

struct WorkerRole {
    /// Which join this is, so a finished serve loop only clears its own role,
    /// never one the user started afterwards.
    generation: u64,
    host_name: String,
    /// Held for its Drop: ending the role closes the link.
    _task: Abort,
    activity: Arc<Mutex<Activity>>,
    /// Read by the engine factory for every job, so a change applies to the next one.
    threads: Arc<AtomicU32>,
    max_threads: u32,
    /// Whether to let jobs use this device's GPU (MESH-12). Off = CPU only.
    lend_gpu: Arc<AtomicBool>,
    has_gpu: bool,
}

/// Aborts its task when the role ends.
struct Abort(tokio::task::JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- what the UI sees -------------------------------------------------------------

#[derive(Serialize)]
pub struct MeshStatus {
    /// "off", "host" or "worker".
    role: &'static str,
    device_name: String,
    host: Option<HostStatus>,
    worker: Option<WorkerStatus>,
    /// Hosts this device paired with as a Worker, for one-click reconnect.
    known_hosts: Vec<KnownDevice>,
}

#[derive(Serialize)]
struct HostStatus {
    /// `None` once the code has expired: the UI offers a new one.
    code: Option<String>,
    code_seconds_left: u64,
    /// For networks that block discovery: what to type on the Worker.
    address: Option<String>,
    discoverable: bool,
    /// Measured decode speed of this device; `None` while measuring or if Ollama is down.
    tok_s: Option<f64>,
    devices: Vec<DeviceStatus>,
    delegator: Option<String>,
    pinned: Option<String>,
}

#[derive(Serialize)]
struct DeviceStatus {
    device_id: String,
    name: String,
    /// `false` = paired just now and waiting on the consent screen.
    consented: bool,
    busy: bool,
    gpu: bool,
    gpu_name: String,
    cores: usize,
    ram_gb: f32,
    llm_model: Option<String>,
    tok_s: Option<f64>,
    link_mbps: Option<f64>,
}

#[derive(Serialize)]
struct WorkerStatus {
    host_name: String,
    /// "idle", or the job's task type ("parse_pdf", "planning"…), never its content.
    activity: String,
    threads: u32,
    max_threads: u32,
    has_gpu: bool,
    lend_gpu: bool,
}

#[derive(Serialize)]
struct KnownDevice {
    device_id: String,
    name: String,
}

// --- helpers ------------------------------------------------------------------------

fn mesh_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("mesh");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn device_name() -> String {
    sysinfo::System::host_name().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "TrustCore device".into())
}

fn identity(app: &tauri::AppHandle) -> Result<DeviceIdentity, String> {
    DeviceIdentity::load_or_create(&mesh_dir(app)?.join("identity.json"), device_name()).map_err(|e| e.to_string())
}

/// What this device can do, as sent in its `Hello`. `tok_s` is filled by [`measure_tok_s`].
fn capabilities(app: &tauri::AppHandle, plan: &ModelPlan) -> Capabilities {
    let hw = detect_hardware();
    let config = pipeline_config(app, plan).ok();
    let exists = |p: Option<&std::path::Path>| p.is_some_and(|p| p.is_file());
    Capabilities {
        physical_cores: hw.physical_cores,
        ram_gb: hw.total_ram_gb,
        gpu: hw.has_usable_gpu(),
        gpu_name: hw.gpu_name.clone(),
        llm_model: Some(plan.llm.clone()).filter(|m| !m.is_empty()),
        vision_model: Some(plan.vision.clone()).filter(|m| !m.is_empty()),
        embed_model: Some(plan.embed.clone()).filter(|m| !m.is_empty()),
        whisper: exists(config.as_ref().map(|c| c.whisper_model_path.as_path())),
        ocr: exists(config.as_ref().map(|c| c.ocr_detection_model.as_path()))
            && exists(config.as_ref().map(|c| c.ocr_recognition_model.as_path())),
        decode_tok_s: None,
    }
}

/// One short answer from the resident model, to measure decode speed (and load
/// it). A few seconds on a CPU. `None` if Ollama isn't answering.
async fn measure_tok_s(plan: &ModelPlan, threads: u32) -> Option<f64> {
    let engine = OllamaEngine::new(OLLAMA_URL, plan.llm.clone(), plan.vision.clone(), plan.embed.clone())
        .with_limits(ResourceLimits { num_thread: threads, num_ctx: plan.num_ctx, ..Default::default() });
    engine.respond("Reply with the single word: ready.", None).await.ok()?;
    engine.timings().last().and_then(|t| t.decode_tok_per_s())
}

/// This device's LAN address, for the "enter the address" fallback. Connecting a
/// UDP socket sends nothing; it only asks the OS which interface it would use.
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("10.255.255.255:1").ok()?;
    socket.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

fn advertise(hub: &Hub, port: u16, code: &PairingCode) -> Option<Advertisement> {
    match Advertisement::start(&hub.device_id(), port, code.tag()) {
        Ok(ad) => Some(ad),
        Err(e) => {
            tracing::warn!(error = %e, "mesh: LAN discovery unavailable; Workers can still enter the address");
            None
        }
    }
}

fn status_of(role: &Role, app: &tauri::AppHandle) -> MeshStatus {
    let known_hosts = mesh_dir(app)
        .ok()
        .and_then(|d| TrustStore::load(&d.join("hosts.json")).ok())
        .map(|s| s.devices().iter().map(|d| KnownDevice { device_id: d.device_id.clone(), name: d.name.clone() }).collect())
        .unwrap_or_default();
    let mut status = MeshStatus { role: "off", device_name: device_name(), host: None, worker: None, known_hosts };
    match role {
        Role::Off => {}
        Role::Host(h) => {
            let left = h.hub.pairing_remaining();
            status.role = "host";
            status.host = Some(HostStatus {
                code: left.map(|_| h.code.clone()),
                code_seconds_left: left.map(|d| d.as_secs()).unwrap_or(0),
                address: lan_ip().map(|ip| SocketAddr::new(ip, h.port).to_string()),
                discoverable: h.advert.is_some(),
                tok_s: h.hub.host_caps().decode_tok_s,
                devices: h
                    .hub
                    .workers()
                    .into_iter()
                    .map(|w| DeviceStatus {
                        device_id: w.device_id,
                        name: w.name,
                        consented: w.consented,
                        busy: w.busy,
                        gpu: w.caps.gpu,
                        gpu_name: w.caps.gpu_name,
                        cores: w.caps.physical_cores,
                        ram_gb: w.caps.ram_gb,
                        llm_model: w.caps.llm_model,
                        tok_s: w.caps.decode_tok_s,
                        link_mbps: w.link_mbps,
                    })
                    .collect(),
                delegator: h.hub.delegator(),
                pinned: h.hub.pinned(),
            });
        }
        Role::Worker(w) => {
            let activity = match &*w.activity.lock().expect("activity lock") {
                Activity::Idle => "idle".to_string(),
                Activity::Busy { task } => task.clone(),
            };
            status.role = "worker";
            status.worker = Some(WorkerStatus {
                host_name: w.host_name.clone(),
                activity,
                threads: w.threads.load(Ordering::Relaxed),
                max_threads: w.max_threads,
                has_gpu: w.has_gpu,
                lend_gpu: w.lend_gpu.load(Ordering::Relaxed),
            });
        }
    }
    status
}

/// The dispatcher for a turn, when this device hosts at least one approved
/// Worker. `None` = run the turn exactly as without the mesh.
pub async fn turn_dispatcher(app: &tauri::AppHandle, uploads: &[InputFile]) -> Option<TurnDispatcher> {
    let mesh = app.state::<MeshState>();
    let role = mesh.0.lock().await;
    match &*role {
        Role::Host(h) if h.hub.workers().iter().any(|w| w.consented) => Some(h.hub.for_turn(uploads)),
        _ => None,
    }
}

/// A Worker is dedicated (MESH-12): its own turns are refused until it leaves.
pub async fn refuse_if_worker(app: &tauri::AppHandle) -> Result<(), String> {
    let mesh = app.state::<MeshState>();
    if matches!(&*mesh.0.lock().await, Role::Worker(_)) {
        return Err("This device is working for another TrustCore Host. Leave the mesh to run your own questions.".into());
    }
    Ok(())
}

// --- commands -----------------------------------------------------------------------

#[tauri::command]
pub async fn mesh_status(app: tauri::AppHandle, mesh: State<'_, MeshState>) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    Ok(status_of(&role, &app))
}

/// Become a Host: listen on the LAN, open a pairing code, advertise it.
#[tauri::command]
pub async fn mesh_start_host(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    mesh: State<'_, MeshState>,
) -> Result<MeshStatus, String> {
    let mut role = mesh.0.lock().await;
    if !matches!(*role, Role::Off) {
        return Err("Leave the current mesh role first.".into());
    }
    let plan = state.plan();
    let me = identity(&app)?;
    let trust = TrustStore::load(&mesh_dir(&app)?.join("workers.json")).map_err(|e| e.to_string())?;
    let hub = Hub::new(me, capabilities(&app, &plan), trust);

    // Any interface, a port the OS picks: Workers learn it from discovery.
    let listener = TcpListener::bind("0.0.0.0:0").await.map_err(|e| format!("could not open a network port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let serving = hub.clone();
    let serve_task = tokio::spawn(async move {
        if let Err(e) = serving.serve(listener).await {
            tracing::error!(error = %e, "mesh: host stopped listening");
        }
    });

    let code = hub.open_pairing();
    let advert = advertise(&hub, port, &code);
    tracing::info!(port, discoverable = advert.is_some(), "mesh: hosting");

    // Measure in the background: until it lands the Host keeps planning itself,
    // the safe default (scheduler::delegator).
    let measuring = hub.clone();
    let threads = detect_hardware().worker_threads() as u32;
    tokio::spawn(async move {
        let tok_s = measure_tok_s(&plan, threads).await;
        let mut caps = measuring.host_caps();
        caps.decode_tok_s = tok_s;
        measuring.set_host_caps(caps);
        tracing::info!(?tok_s, "mesh: host speed measured");
    });

    *role = Role::Host(HostRole { hub, port, code: code.to_string(), _serve: Abort(serve_task), advert });
    Ok(status_of(&role, &app))
}

/// A fresh code (the old one stops working).
#[tauri::command]
pub async fn mesh_new_code(app: tauri::AppHandle, mesh: State<'_, MeshState>) -> Result<MeshStatus, String> {
    let mut role = mesh.0.lock().await;
    let Role::Host(h) = &mut *role else { return Err("Only a Host has a pairing code.".into()) };
    let code = h.hub.open_pairing();
    h.advert = None; // withdraw the old tag before announcing the new one
    h.advert = advertise(&h.hub, h.port, &code);
    h.code = code.to_string();
    Ok(status_of(&role, &app))
}

/// The consent screen's "Allow": remember the device and give it work (MESH-4).
#[tauri::command]
pub async fn mesh_approve(app: tauri::AppHandle, mesh: State<'_, MeshState>, device_id: String) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    let Role::Host(h) = &*role else { return Err("Not hosting.".into()) };
    h.hub.approve(&device_id).map_err(|e| e.to_string())?;
    tracing::info!(device = %device_id, "mesh: device approved by the user");
    Ok(status_of(&role, &app))
}

/// "Don't allow" on the consent screen, or "Remove" on a paired device.
#[tauri::command]
pub async fn mesh_remove(app: tauri::AppHandle, mesh: State<'_, MeshState>, device_id: String) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    let Role::Host(h) = &*role else { return Err("Not hosting.".into()) };
    h.hub.remove(&device_id, "the Host removed this device").map_err(|e| e.to_string())?;
    tracing::info!(device = %device_id, "mesh: device removed by the user");
    Ok(status_of(&role, &app))
}

/// Make one device the planner, or `None` to let speed decide (MESH-10).
#[tauri::command]
pub async fn mesh_pin_delegator(
    app: tauri::AppHandle,
    mesh: State<'_, MeshState>,
    device_id: Option<String>,
) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    let Role::Host(h) = &*role else { return Err("Not hosting.".into()) };
    h.hub.pin_delegator(device_id);
    Ok(status_of(&role, &app))
}

/// Become a Worker for the Host showing `code`. `address` is the fallback for
/// networks where discovery finds nothing.
#[tauri::command]
pub async fn mesh_join(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    mesh: State<'_, MeshState>,
    code: String,
    address: Option<String>,
) -> Result<MeshStatus, String> {
    ensure_off(&mesh).await?;
    let code = PairingCode::parse(&code)
        .ok_or("That code doesn't look right: it is 6 letters and digits, like K7Q-4M9.")?;
    let addr = match address.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => a.parse::<SocketAddr>().map_err(|_| "The address should look like 192.168.1.20:52345.")?,
        None => *find_hosts(code.tag(), FIND_HOST_FOR)
            .await
            .map_err(|e| e.to_string())?
            .first()
            .ok_or("No Host with that code was found on this network. Check both devices are on the same Wi-Fi, or enter the Host's address.")?,
    };
    let (me, hello, plan) = worker_hello(&app, &state).await?;
    let link = pair(addr, &code, &me, &hello).await.map_err(|e| e.to_string())?;
    let host = link.peer().clone();
    let mut hosts = TrustStore::load(&mesh_dir(&app)?.join("hosts.json")).map_err(|e| e.to_string())?;
    hosts.add(&host.name, &host.public_key).map_err(|e| e.to_string())?;
    start_worker(&app, &mesh, link, plan).await
}

/// Reconnect to a Host this device paired with before. No code needed.
#[tauri::command]
pub async fn mesh_reconnect(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    mesh: State<'_, MeshState>,
    device_id: String,
) -> Result<MeshStatus, String> {
    ensure_off(&mesh).await?;
    let hosts = TrustStore::load(&mesh_dir(&app)?.join("hosts.json")).map_err(|e| e.to_string())?;
    let known = hosts.devices().iter().find(|d| d.device_id == device_id).ok_or("That Host isn't paired any more.")?;
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.decode(&known.public_key).map_err(|e| e.to_string())?;
    let addr = *find_host_by_id(&device_id, FIND_HOST_FOR)
        .await
        .map_err(|e| e.to_string())?
        .first()
        .ok_or("That Host isn't hosting right now, or isn't on this network.")?;
    let (me, hello, plan) = worker_hello(&app, &state).await?;
    let link = reconnect(addr, &me, &key, &hello).await.map_err(|e| e.to_string())?;
    start_worker(&app, &mesh, link, plan).await
}

/// Forget a Host this Worker paired with.
#[tauri::command]
pub async fn mesh_forget_host(app: tauri::AppHandle, mesh: State<'_, MeshState>, device_id: String) -> Result<MeshStatus, String> {
    let mut hosts = TrustStore::load(&mesh_dir(&app)?.join("hosts.json")).map_err(|e| e.to_string())?;
    hosts.remove(&device_id).map_err(|e| e.to_string())?;
    let role = mesh.0.lock().await;
    Ok(status_of(&role, &app))
}

/// The Worker owner's thread cap (MESH-12). Applies from the next job.
#[tauri::command]
pub async fn mesh_set_threads(app: tauri::AppHandle, mesh: State<'_, MeshState>, threads: u32) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    let Role::Worker(w) = &*role else { return Err("Not a Worker.".into()) };
    w.threads.store(threads.clamp(1, w.max_threads), Ordering::Relaxed);
    Ok(status_of(&role, &app))
}

/// The Worker owner's GPU switch (MESH-12). Off = jobs run CPU-only. Applies
/// from the next job. The Host ranked this device by the GPU it reported at
/// join, so with the GPU withheld it may still get model steps, just slower.
#[tauri::command]
pub async fn mesh_set_lend_gpu(app: tauri::AppHandle, mesh: State<'_, MeshState>, lend: bool) -> Result<MeshStatus, String> {
    let role = mesh.0.lock().await;
    let Role::Worker(w) = &*role else { return Err("Not a Worker.".into()) };
    w.lend_gpu.store(lend, Ordering::Relaxed);
    Ok(status_of(&role, &app))
}

/// Stop hosting or working. Paired devices stay remembered.
#[tauri::command]
pub async fn mesh_leave(app: tauri::AppHandle, mesh: State<'_, MeshState>) -> Result<MeshStatus, String> {
    let mut role = mesh.0.lock().await;
    if let Role::Host(h) = &*role {
        h.hub.shutdown("the Host stopped hosting");
    }
    *role = Role::Off; // drops the tasks and the LAN advertisement
    tracing::info!("mesh: left");
    Ok(status_of(&role, &app))
}

// --- Worker setup ---------------------------------------------------------------------

/// Checked before pairing, so a Host doesn't approve a device that then can't
/// become a Worker. `start_worker` checks again under the lock.
async fn ensure_off(mesh: &State<'_, MeshState>) -> Result<(), String> {
    if matches!(&*mesh.0.lock().await, Role::Off) {
        Ok(())
    } else {
        Err("Leave the current mesh role first.".into())
    }
}

/// The Worker's identity, its `Hello` (with a measured speed: a few seconds),
/// and the model plan it will run jobs with.
async fn worker_hello(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
) -> Result<(DeviceIdentity, Hello, ModelPlan), String> {
    let plan = state.plan();
    let me = identity(app)?;
    let mut caps = capabilities(app, &plan);
    caps.decode_tok_s = measure_tok_s(&plan, detect_hardware().worker_threads() as u32).await;
    let hello = Hello { name: me.name.clone(), capabilities: serde_json::to_value(&caps).map_err(|e| e.to_string())? };
    Ok((me, hello, plan))
}

async fn start_worker(
    app: &tauri::AppHandle,
    mesh: &State<'_, MeshState>,
    link: Link,
    plan: ModelPlan,
) -> Result<MeshStatus, String> {
    let mut role = mesh.0.lock().await;
    if !matches!(*role, Role::Off) {
        return Err("Leave the current mesh role first.".into());
    }
    let host_name = link.peer().name.clone();
    // "Leave one core free" by default: the same rule the app uses for itself.
    let hw = detect_hardware();
    let max_threads = hw.worker_threads() as u32;
    let has_gpu = hw.has_usable_gpu();
    let threads = Arc::new(AtomicU32::new(max_threads));
    let lend_gpu = Arc::new(AtomicBool::new(true));

    let engine_plan = plan.clone();
    let engine_threads = threads.clone();
    let engine_gpu = lend_gpu.clone();
    let make = move |mode: TurnMode, persona: &str, flag: CancelFlag| {
        OllamaEngine::new(OLLAMA_URL, engine_plan.llm.clone(), engine_plan.vision.clone(), engine_plan.embed.clone())
            .with_limits(ResourceLimits {
                num_thread: engine_threads.load(Ordering::Relaxed),
                num_ctx: engine_plan.num_ctx,
                // Lent: Ollama decides, as for any turn. Withheld: no layers on the GPU.
                num_gpu: if engine_gpu.load(Ordering::Relaxed) { None } else { Some(0) },
                ..Default::default()
            })
            .with_mode(mode)
            .with_persona(persona.to_string())
            .with_cancel(flag)
    };
    let make = Arc::new(make);
    let for_jobs = make.clone();
    let rt = Arc::new(WorkerRuntime {
        registry: default_registry(),
        config: pipeline_config(app, &plan)?,
        scratch: mesh_dir(app)?.join("scratch"),
        make_engine: Arc::new(move |job, flag| for_jobs(job.mode, &job.persona, flag)),
        make_planner: Arc::new(move |job, flag| {
            Box::new(make(TurnMode::Fast, &job.persona, flag)) as Box<dyn PlanSource>
        }),
    });

    static JOINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = JOINS.fetch_add(1, Ordering::Relaxed);
    let activity = Arc::new(Mutex::new(Activity::Idle));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let shown = activity.clone();
    let watcher = app.clone();
    let task = tokio::spawn(async move {
        let track = tokio::spawn(async move {
            while let Some(a) = rx.recv().await {
                *shown.lock().expect("activity lock") = a;
            }
        });
        let reason = serve(link, rt, Some(tx)).await;
        track.abort();
        tracing::info!(%reason, "mesh: worker stopped");
        // The Host went away: back to Off, so this device's own app works again.
        // Dropping the role aborts this task too, which is harmless: nothing
        // awaits after this point.
        let mesh = watcher.state::<MeshState>();
        let mut role = mesh.0.lock().await;
        if matches!(&*role, Role::Worker(w) if w.generation == generation) {
            *role = Role::Off;
        }
    });

    tracing::info!(host = %host_name, "mesh: working for a Host");
    *role = Role::Worker(WorkerRole {
        generation,
        host_name,
        _task: Abort(task),
        activity,
        threads,
        max_threads,
        lend_gpu,
        has_gpu,
    });
    Ok(status_of(&role, app))
}
