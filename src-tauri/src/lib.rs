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

use std::sync::Mutex;

use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::generation::parameters::KeepAlive;
use ollama_rs::Ollama;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{Manager, State};

use bootstrap::hardware::detect_hardware;
use bootstrap::models::{ensure_models, probe_system, ModelPlan};
use bootstrap::ollama::{ensure_ollama_installed, is_ollama_installed};
use base64::Engine as _;
use events::ChannelSink;
use workbench_core::engine::OllamaEngine;
use workbench_core::memory::SessionContext;
use workbench_core::schemas::{FileKind, InputFile};
use workbench_core::tools::default_registry;
use workbench_core::{persist_long_term, PipelineConfig, ProgressSink, StepEvent};

/// Where the local Ollama server listens. Fixed — this is an offline desktop app.
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Tauri-managed shared state.
pub struct AppState {
    /// Client used by the bootstrap commands (list / pull models).
    pub ollama: Ollama,
    /// The tier the user confirmed in the bootstrap screen. `None` until then.
    pub model_plan: Mutex<Option<ModelPlan>>,
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

/// Finalise a session: fold its rolling context into encrypted long-term memory.
/// Safe to call at any time (e.g. window close, "new session" button).
#[tauri::command]
fn end_session(app: tauri::AppHandle, state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let config = pipeline_config(&app, &state.plan())?;
    let session = SessionContext::load_or_new(&config.session_path(), &session_id);
    persist_long_term(&config, &session);
    Ok(())
}

/// Warm the resident model so the first real turn is not cold. Idempotent.
#[tauri::command]
async fn warm_model(state: State<'_, AppState>) -> Result<(), String> {
    let model = state
        .model_plan
        .lock()
        .expect("model_plan mutex poisoned")
        .as_ref()
        .map(|p| p.llm.clone())
        .ok_or("no model plan selected yet")?;
    let request = GenerationRequest::new(model, String::new()).keep_alive(KeepAlive::Indefinitely);
    let _ = state.ollama.generate(request).await;
    Ok(())
}

/// Absolute paths the audit sidebar displays.
#[derive(Serialize)]
struct AuditPaths {
    data_dir: String,
    uploads: String,
    lancedb: String,
    session_context: String,
    persistent_memory: String,
}

#[tauri::command]
fn audit_paths(app: tauri::AppHandle) -> Result<AuditPaths, String> {
    let cfg = pipeline_config(&app, &ModelPlan::fallback())?;
    Ok(AuditPaths {
        data_dir: cfg.data_dir.display().to_string(),
        uploads: cfg.uploads_dir().display().to_string(),
        lancedb: cfg.lancedb_dir().display().to_string(),
        session_context: cfg.session_path().display().to_string(),
        persistent_memory: cfg.persistent_path().display().to_string(),
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
    for file in files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content_base64.trim())
            .map_err(|e| format!("invalid base64 for `{}`: {e}", file.name))?;
        let safe_name: String = file
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
            .collect();
        let path = dir.join(if safe_name.is_empty() { "upload" } else { &safe_name });
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        out.push(InputFile {
            kind: FileKind::from_path(&path),
            path: path.to_string_lossy().into_owned(),
            original_name: file.name.clone(),
        });
    }
    Ok(out)
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
    on_event: Channel<StepEvent>,
) -> Result<(), String> {
    let plan = state.plan();
    let config = pipeline_config(&app, &plan)?;
    let uploads = persist_uploads(&config, &session_id, &files)?;

    let engine = OllamaEngine::new(
        &config.ollama_url,
        plan.llm.clone(),
        plan.vision.clone(),
        plan.embed.clone(),
    );
    let registry = default_registry();
    let sink = ChannelSink(on_event);

    match workbench_core::run_turn(
        &engine, &registry, &config, &session_id, &prompt, &uploads, &sink,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            sink.emit(StepEvent::Error {
                message: e.to_string(),
            });
            Err(e.to_string())
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            ollama: Ollama::default(),
            model_plan: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            generate_response,
            is_ollama_installed,
            ensure_ollama_installed,
            detect_hardware,
            probe_system,
            ensure_models,
            set_model_plan,
            warm_model,
            audit_paths,
            submit_turn,
            end_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
