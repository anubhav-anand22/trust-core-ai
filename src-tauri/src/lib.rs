//! Tauri host for the Sovereign On-Premise AI Workbench.
//!
//! What stays on the Rust side of the app:
//!   * Ollama bootstrap — install + tier-aware model pulls ([`bootstrap`])
//!   * Host capability probe + model-tier pick ([`bootstrap::hardware`], [`bootstrap::models`])
//!   * Driving [`workbench_core`] once per turn and streaming
//!     [`StepEvent`](workbench_core::StepEvent)s to the React stepper ([`submit_turn`])
//!
//! The pipeline (planning, validation, tools, memory) lives in the
//! `workbench-core` crate and knows nothing about Tauri.

mod bootstrap;
mod events;
mod logging;

use std::sync::Mutex;

use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::generation::parameters::KeepAlive;
use ollama_rs::Ollama;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{Manager, State};

use bootstrap::hardware::detect_hardware;
use bootstrap::models::{assess_llm, ensure_models, probe_system, LlmAssessment, ModelPlan};
use bootstrap::ollama::{ensure_ollama_installed, is_ollama_installed, lower_ollama_priority};
use base64::Engine as _;
use events::ChannelSink;
use workbench_core::engine::OllamaEngine;
use workbench_core::memory::{SessionContext, SessionMeta};
use workbench_core::schemas::{FileKind, InputFile};
use workbench_core::tools::default_registry;
use workbench_core::{
    persist_long_term, CancelFlag, PipelineConfig, ProgressSink, StepEvent, TurnMode,
};

/// Where the local Ollama server listens. Fixed — this is an offline desktop app.
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Tauri-managed shared state.
pub struct AppState {
    /// Client used by the bootstrap commands (list / pull models).
    pub ollama: Ollama,
    /// The tier the user confirmed in the bootstrap screen. `None` until then.
    pub model_plan: Mutex<Option<ModelPlan>>,
    /// The cancel flag of the turn currently running, if any. `cancel_turn` sets
    /// it; `submit_turn` / `resume_turn` install and clear it. Only one turn runs
    /// at a time, so a single slot is enough.
    pub cancel: Mutex<Option<CancelFlag>>,
}

impl AppState {
    /// The active plan, or the conservative fallback if nothing is set yet.
    fn plan(&self) -> ModelPlan {
        self.model_plan
            .lock()
            .expect("model_plan mutex poisoned")
            .clone()
            .unwrap_or_else(ModelPlan::fallback)
    }
}

/// Debug helper kept from the scaffold: one-shot generation. Not on the pipeline
/// path — handy for smoke-testing that Ollama answers at all.
#[tauri::command]
async fn generate_response(prompt: String, state: State<'_, AppState>) -> Result<String, String> {
    let request = GenerationRequest::new(state.plan().llm, prompt).keep_alive(KeepAlive::Indefinitely);
    state
        .ollama
        .generate(request)
        .await
        .map(|r| r.response)
        .map_err(|e| e.to_string())
}

/// Persist the model plan the user confirmed in the bootstrap screen.
#[tauri::command]
fn set_model_plan(plan: ModelPlan, state: State<'_, AppState>) {
    *state.model_plan.lock().expect("model_plan mutex poisoned") = Some(plan);
}

/// Judge a resident-model choice against this host, for the bootstrap override
/// warning. Re-probes the hardware so the RAM figure reflects the machine's state
/// right now, not whatever it was at first launch.
#[tauri::command]
fn assess_model(model: String) -> LlmAssessment {
    let hw = detect_hardware();
    let assessment = assess_llm(&model, &hw);
    tracing::info!(
        model = %model,
        severity = %assessment.severity,
        approx_ram_gb = assessment.approx_ram_gb,
        warnings = assessment.warnings.len(),
        "model assessment for override"
    );
    assessment
}

/// Finalise a session: fold its rolling context into encrypted long-term memory.
/// Safe to call at any time (e.g. window close, "new session" button).
#[tauri::command]
fn end_session(app: tauri::AppHandle, state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let config = pipeline_config(&app, &state.plan())?;
    let session = SessionContext::load_or_new(&config.session_path_for(&session_id), &session_id);
    persist_long_term(&config, &session);
    Ok(())
}

/// Move a pre-Stage-4 single `session_context.json` into `sessions/<id>.json`
/// the first time we see it, so an existing user's last chat is not orphaned.
fn migrate_legacy_session(config: &PipelineConfig) {
    let legacy = config.legacy_session_path();
    if !legacy.is_file() {
        return;
    }
    if let Some(s) = SessionContext::load(&legacy) {
        if !s.session_id.is_empty() {
            let dest = config.session_path_for(&s.session_id);
            if !dest.exists() {
                if let Err(e) = s.save(&dest) {
                    tracing::warn!(error = %e, "legacy session migration failed");
                    return;
                }
            }
        }
    }
    let _ = std::fs::rename(&legacy, legacy.with_extension("json.migrated"));
    tracing::info!("migrated legacy session_context.json into sessions/");
}

/// List past chat sessions, newest first, for the sidebar.
#[tauri::command]
fn list_sessions(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<Vec<SessionMeta>, String> {
    let config = pipeline_config(&app, &state.plan())?;
    migrate_legacy_session(&config);
    Ok(SessionContext::list(&config.sessions_dir()))
}

/// Load one session's full transcript so the UI can redraw it.
#[tauri::command]
fn load_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<SessionContext, String> {
    let config = pipeline_config(&app, &state.plan())?;
    SessionContext::load(&config.session_path_for(&session_id))
        .ok_or_else(|| format!("no session `{session_id}`"))
}

/// Delete a session file. `end_session`'s long-term digest is left intact.
#[tauri::command]
fn delete_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let config = pipeline_config(&app, &state.plan())?;
    let path = config.session_path_for(&session_id);
    if path.is_file() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        tracing::info!(%session_id, "session deleted");
    }
    Ok(())
}

/// Rename a session (sidebar title). No-op if the session does not exist yet.
#[tauri::command]
fn rename_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> Result<(), String> {
    let config = pipeline_config(&app, &state.plan())?;
    let path = config.session_path_for(&session_id);
    if let Some(mut s) = SessionContext::load(&path) {
        s.title = title.chars().take(120).collect();
        s.save(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Warm the resident model so the first real turn is not cold. Idempotent.
#[tauri::command]
async fn warm_model(state: State<'_, AppState>) -> Result<(), String> {
    let plan = state.plan();

    // Go through the engine, not the bare client: the first model load is the one
    // most likely to thrash a small machine, so it must carry the same thread
    // ceiling as every other call. Deriving `num_thread` from the host also means
    // the warm-up respects the physical-core budget from here on.
    let hw = detect_hardware();
    let limits = workbench_core::engine::ResourceLimits {
        num_thread: hw.worker_threads() as u32,
        ..Default::default()
    };
    let engine = OllamaEngine::new(
        OLLAMA_URL,
        plan.llm.clone(),
        plan.vision.clone(),
        plan.embed.clone(),
    )
    .with_limits(limits);

    let warmed = engine.warm().await.map_err(|e| e.to_string());
    // The model runner subprocess exists now; pull it below normal too.
    lower_ollama_priority();
    warmed
}

/// Absolute paths the audit sidebar displays.
#[derive(Serialize)]
struct AuditPaths {
    data_dir: String,
    uploads: String,
    lancedb: String,
    session_context: String,
    persistent_memory: String,
    logs: String,
}

#[tauri::command]
fn audit_paths(app: tauri::AppHandle) -> Result<AuditPaths, String> {
    let cfg = pipeline_config(&app, &ModelPlan::fallback())?;
    Ok(AuditPaths {
        data_dir: cfg.data_dir.display().to_string(),
        uploads: cfg.uploads_dir().display().to_string(),
        lancedb: cfg.lancedb_dir().display().to_string(),
        session_context: cfg.sessions_dir().display().to_string(),
        persistent_memory: cfg.persistent_path().display().to_string(),
        logs: logging::log_dir(&cfg.data_dir).display().to_string(),
    })
}

/// Assemble a [`PipelineConfig`] rooted at the OS app-data directory.
fn pipeline_config(app: &tauri::AppHandle, plan: &ModelPlan) -> Result<PipelineConfig, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let models_dir = data_dir.join("models");
    Ok(PipelineConfig {
        kb_dir: data_dir.join("kb"),
        data_dir,
        ollama_url: OLLAMA_URL.to_string(),
        llm_model: plan.llm.clone(),
        vision_model: plan.vision.clone(),
        embed_model: plan.embed.clone(),
        // Phase 2 wires downloads for these; the paths are fixed now.
        whisper_model_path: models_dir.join("ggml-base.en.bin"),
        ocr_detection_model: models_dir.join("text-detection.rten"),
        ocr_recognition_model: models_dir.join("text-recognition.rten"),
    })
}

/// One attachment as sent from the front-end: original name + base64 bytes.
///
/// base64 (rather than a `Vec<u8>` that JSON would balloon) keeps the IPC payload
/// compact and needs no extra Tauri plugin.
#[derive(serde::Deserialize)]
struct UploadedFile {
    name: String,
    content_base64: String,
}

/// Decode the uploads, write them under `uploads/<session>/`, and classify each by
/// extension into a [`InputFile`] the pipeline can consume.
fn persist_uploads(
    config: &PipelineConfig,
    session_id: &str,
    files: &[UploadedFile],
) -> Result<Vec<InputFile>, String> {
    let dir = config.uploads_dir().join(session_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(files.len());
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content_base64.trim())
            .map_err(|e| format!("invalid base64 for `{}`: {e}", file.name))?;
        let safe_name: String = file
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
            .collect();
        let base = if safe_name.is_empty() { "upload".to_string() } else { safe_name };

        // Two attachments that sanitise to the same on-disk name (`photo.jpg`
        // from two folders, or `IMG 1.jpg` and `IMG_1.jpg`) would otherwise
        // overwrite each other, leaving two `InputFile`s pointing at one file.
        let disk_name = {
            let mut candidate = base.clone();
            let (stem, ext) = match base.rsplit_once('.') {
                Some((s, e)) => (s.to_string(), format!(".{e}")),
                None => (base.clone(), String::new()),
            };
            let mut n = 1;
            while !used.insert(candidate.clone()) {
                n += 1;
                candidate = format!("{stem}_{n}{ext}");
            }
            candidate
        };

        let path = dir.join(&disk_name);
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        out.push(InputFile {
            kind: FileKind::from_path(&path),
            path: path.to_string_lossy().into_owned(),
            // The name the *user* gave stays as-is — it is what the planner names
            // and what the report cites. Only the on-disk name is disambiguated.
            original_name: file.name.clone(),
        });
    }
    Ok(out)
}

/// Shared setup for [`submit_turn`] / [`resume_turn`]: resolve the pipeline config
/// for the active model plan, materialise the uploads on disk, and build the
/// engine. `persist_uploads` is idempotent for a given session, so a resume that
/// re-sends the same files just rewrites them.
fn turn_setup(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    session_id: &str,
    files: &[UploadedFile],
    mode: TurnMode,
) -> Result<(PipelineConfig, Vec<InputFile>, OllamaEngine, CancelFlag), String> {
    let plan = state.plan();
    let config = pipeline_config(app, &plan)?;
    let uploads = persist_uploads(&config, session_id, files)?;

    // Fresh stop signal for this turn. Registered in AppState so `cancel_turn`
    // can reach it, and handed to the engine so a Stop lands mid model-call, not
    // only between steps.
    let cancel = CancelFlag::new();
    *state.cancel.lock().expect("cancel mutex poisoned") = Some(cancel.clone());

    // Bound context, threads and output against the real host. Ollama's own
    // defaults would take every core (freezing the desktop for the whole turn)
    // and silently truncate long attachments to the model's default window.
    let hw = detect_hardware();
    let limits = workbench_core::engine::ResourceLimits {
        num_thread: hw.worker_threads() as u32,
        // The tier chose this: 4096 on the smallest CPU model where the KV cache
        // and prefill are a real cost, 8192 elsewhere.
        num_ctx: plan.num_ctx,
        ..Default::default()
    };
    tracing::info!(
        model = %plan.llm,
        num_thread = limits.num_thread,
        num_ctx = limits.num_ctx,
        timeout_s = limits.call_timeout.as_secs(),
        "turn engine limits"
    );

    let engine = OllamaEngine::new(
        &config.ollama_url,
        plan.llm.clone(),
        plan.vision.clone(),
        plan.embed.clone(),
    )
    .with_limits(limits)
    .with_cancel(cancel.clone())
    .with_mode(mode);
    Ok((config, uploads, engine, cancel))
}

/// Request cancellation of the turn currently running. No-op if none is.
#[tauri::command]
fn cancel_turn(state: State<'_, AppState>) {
    if let Some(flag) = state.cancel.lock().expect("cancel mutex poisoned").as_ref() {
        tracing::info!("cancel_turn: stop requested by user");
        flag.cancel();
    }
}

/// Clear the in-flight cancel flag once a turn is done (any outcome).
fn clear_cancel(state: &State<'_, AppState>) {
    *state.cancel.lock().expect("cancel mutex poisoned") = None;
}

/// Run one turn of the pipeline, streaming [`StepEvent`]s to the front-end.
///
/// **Inputs:** the user prompt, any attached files (base64), a session id, and a
/// `Channel` the UI listens on. Files are written to `uploads/<session>/` and
/// classified by extension before the pipeline runs.
#[tauri::command]
async fn submit_turn(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    prompt: String,
    session_id: String,
    files: Vec<UploadedFile>,
    mode: Option<String>,
    on_event: Channel<StepEvent>,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mode = mode.as_deref().map(TurnMode::parse).unwrap_or_default();
    tracing::info!(
        %session_id,
        ?mode,
        prompt_chars = prompt.len(),
        file_count = files.len(),
        "submit_turn: start"
    );

    let (config, uploads, engine, _cancel) = turn_setup(&app, &state, &session_id, &files, mode)?;
    tracing::debug!(
        model = engine.llm_model(),
        uploads = ?uploads.iter().map(|u| format!("{} [{:?}]", u.original_name, u.kind)).collect::<Vec<_>>(),
        limits = ?engine.limits(),
        "submit_turn: context ready"
    );

    let registry = default_registry();
    let sink = ChannelSink(on_event);

    let result = workbench_core::run_turn(
        &engine, &registry, &config, &session_id, &prompt, &uploads, &sink,
    )
    .await;
    clear_cancel(&state);

    match result {
        Ok(outcome) => {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                outcome = match &outcome {
                    workbench_core::TurnOutcome::Completed(r) =>
                        if r.degraded { "completed(degraded)" } else { "completed" },
                    workbench_core::TurnOutcome::AwaitingUser { .. } => "awaiting_user",
                },
                "submit_turn: done"
            );
            Ok(())
        }
        // A user-pressed Stop is a normal outcome, not a failure: tell the UI
        // with a warning strip, not a red error banner.
        Err(workbench_core::CoreError::Cancelled) => {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "submit_turn: cancelled by user"
            );
            sink.emit(StepEvent::Warning {
                message: "Turn stopped.".into(),
                detail: None,
            });
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                elapsed_ms = started.elapsed().as_millis(),
                "submit_turn: failed"
            );
            sink.emit(StepEvent::Error {
                message: e.to_string(),
            });
            Err(e.to_string())
        }
    }
}

/// Reveal the log directory in the OS file manager, so a user can attach the
/// file to a bug report without hunting for the app-data path.
#[tauri::command]
fn open_log_dir(app: tauri::AppHandle) -> Result<String, String> {
    let dir = logging::log_dir(&app.path().app_data_dir().map_err(|e| e.to_string())?);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let shown = dir.display().to_string();

    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";

    // `explorer` returns a non-zero exit code even when it succeeds, so the
    // spawn is best-effort and the path is returned either way.
    let _ = std::process::Command::new(opener).arg(&dir).spawn();
    Ok(shown)
}

/// Resume a turn the planner parked, running the plan the user reviewed in the
/// HITL modal instead of asking the model for one.
///
/// **Inputs:** the original prompt + files (the front-end still has them), the
/// session id, the (possibly edited) `plan_json` from the modal, and `force` —
/// `true` when the user clicked "run anyway", which skips re-validation. A plan
/// that still fails validation (and `force = false`) re-emits `AwaitingUser`.
#[tauri::command]
async fn resume_turn(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    prompt: String,
    session_id: String,
    files: Vec<UploadedFile>,
    plan_json: String,
    force: bool,
    mode: Option<String>,
    on_event: Channel<StepEvent>,
) -> Result<(), String> {
    let mode = mode.as_deref().map(TurnMode::parse).unwrap_or_default();
    let started = std::time::Instant::now();
    tracing::info!(
        %session_id,
        force,
        plan_chars = plan_json.len(),
        "resume_turn: start"
    );

    let (config, uploads, engine, _cancel) = turn_setup(&app, &state, &session_id, &files, mode)?;
    let registry = default_registry();
    let sink = ChannelSink(on_event);

    let result = workbench_core::resume_turn(
        &engine, &registry, &config, &session_id, &prompt, &uploads, &plan_json, force, &sink,
    )
    .await;
    clear_cancel(&state);

    match result {
        Ok(outcome) => {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                outcome = match &outcome {
                    workbench_core::TurnOutcome::Completed(_) => "completed",
                    workbench_core::TurnOutcome::AwaitingUser { .. } => "awaiting_user (still invalid)",
                },
                "resume_turn: done"
            );
            Ok(())
        }
        Err(workbench_core::CoreError::Cancelled) => {
            tracing::info!("resume_turn: cancelled by user");
            sink.emit(StepEvent::Warning {
                message: "Turn stopped.".into(),
                detail: None,
            });
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "resume_turn: failed");
            sink.emit(StepEvent::Error {
                message: e.to_string(),
            });
            Err(e.to_string())
        }
    }
}

/// Owns the log-file flushing thread for the life of the process.
///
/// Dropping a [`WorkerGuard`](tracing_appender::non_blocking::WorkerGuard) flushes
/// *and joins* the background writer; every line logged afterwards is silently
/// discarded. It therefore cannot be a local in `run()` or a capture of the
/// `setup` closure — Tauri's setup hook is a `FnOnce`, so calling it drops its
/// captures and would kill file logging the instant startup finished. Handing it
/// to Tauri's managed state keeps it alive until the app shuts down, which also
/// means the last buffered lines still make it to disk on a clean exit.
///
/// The `Mutex` is only here to guarantee `Sync`, which managed state requires.
struct LogGuard(#[allow(dead_code)] Mutex<tracing_appender::non_blocking::WorkerGuard>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Cap the OCR runtime's thread pool before anything can build it, so a turn
    // never pins every core and locks up the desktop.
    workbench_core::init_thread_pools();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // The log directory lives under app data, which is only resolvable
            // once the app handle exists — hence setup() rather than the top of
            // run().
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                app.manage(LogGuard(Mutex::new(logging::init(&dir))));
            }

            // If an Ollama server is already up, hand the desktop priority now.
            // The per-model runner is spawned lazily, so `warm_model` calls this
            // again once weights are loaded.
            lower_ollama_priority();
            Ok(())
        })
        .manage(AppState {
            ollama: Ollama::default(),
            model_plan: Mutex::new(None),
            cancel: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            generate_response,
            is_ollama_installed,
            ensure_ollama_installed,
            detect_hardware,
            probe_system,
            ensure_models,
            set_model_plan,
            assess_model,
            cancel_turn,
            warm_model,
            audit_paths,
            submit_turn,
            resume_turn,
            end_session,
            list_sessions,
            load_session,
            delete_session,
            rename_session,
            logging::ui_log,
            open_log_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
