//! Sovereign On-Premise AI Workbench — pipeline core.
//!
//! This crate is deliberately Tauri-free. It exposes:
//!   * [`engine`]   — the single resident LLM (role-swapped) + typed schemas
//!   * [`planner`]  — deterministic task registry + plan validation + retry loop
//!   * [`executor`] — strict sequential tool runner, quality check, report compiler
//!   * [`tools`]    — one module per tool (audio, pdf, ocr, vision, rag)
//!   * [`memory`]   — session context (rolling compression) + encrypted persistent memory
//!   * [`events`]   — [`StepEvent`] stream the front-end stepper renders
//!
//! The Tauri host in `src-tauri` wires a [`ProgressSink`] over a `tauri::ipc::Channel`
//! and calls [`run_turn`] (added in Phase 2).

// `planner/planner.rs`, `executor/executor.rs` etc. — the leaf file carries the
// implementation, the `mod.rs` only re-exports. Intentional.
#![allow(clippy::module_inception)]

pub mod engine;
pub mod events;
pub mod executor;
pub mod memory;
pub mod pipeline;
pub mod planner;
pub mod tools;

use std::path::PathBuf;

pub use engine::schemas;
pub use events::{ProgressSink, StepEvent};
pub use pipeline::{persist_long_term, resume_turn, run_turn, TurnOutcome};

/// Threads a compute-heavy step may use: every core but one.
///
/// This is a desktop app, and a turn must not make the machine unusable while it
/// runs. whisper.cpp and the `rten` OCR runtime both default to grabbing every
/// core, which on a small CPU-only host starves the window manager — frozen
/// tray flyouts, stuttering video calls — for the whole step.
pub fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(1)
        .max(1)
}

/// Bound the global `rayon` pool (used by `rten` under `ocrs`) to
/// [`worker_threads`].
///
/// Call once, before any tool runs. Safe to call repeatedly: after the first
/// call the pool already exists and the builder returns an error we ignore.
pub fn init_thread_pools() {
    // `rayon` reads this when it lazily builds its global pool. Setting it is
    // enough and avoids taking a direct dependency on rayon here.
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        std::env::set_var("RAYON_NUM_THREADS", worker_threads().to_string());
    }
}

/// Everything the pipeline needs to resolve models and on-disk locations.
///
/// Built by the Tauri host from the app-data dir + the hardware-selected
/// [`ModelPlan`](../src-tauri) and handed to each turn.
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    /// Holds `uploads/`, `lancedb/`, `session_context.json`, `persistent_memory.json`.
    pub data_dir: PathBuf,
    /// Seed knowledge-base documents ingested into the vector store on first run.
    pub kb_dir: PathBuf,
    /// Ollama server base URL, e.g. `http://127.0.0.1:11434`.
    pub ollama_url: String,
    /// The one resident text model (role A/B/C), kept alive indefinitely.
    pub llm_model: String,
    /// Compact vision model (`moondream`), called with `keep_alive = 0`.
    pub vision_model: String,
    /// Embedding model (`nomic-embed-text`), called with `keep_alive = 0`.
    pub embed_model: String,
    /// Whisper GGUF model file for the audio tool (Phase 2).
    pub whisper_model_path: PathBuf,
    /// ocrs detection / recognition `.rten` model files (Phase 2).
    pub ocr_detection_model: PathBuf,
    pub ocr_recognition_model: PathBuf,
}

impl PipelineConfig {
    pub fn uploads_dir(&self) -> PathBuf { self.data_dir.join("uploads") }
    pub fn lancedb_dir(&self) -> PathBuf { self.data_dir.join("lancedb") }
    pub fn session_path(&self) -> PathBuf { self.data_dir.join("session_context.json") }
    pub fn persistent_path(&self) -> PathBuf { self.data_dir.join("persistent_memory.json") }
}

/// Unified error for the whole pipeline.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("ollama request failed: {0}")]
    Ollama(String),

    #[error("model returned malformed JSON for role `{role}`: {source}")]
    Schema {
        role: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("tool `{0}` is not registered")]
    UnknownTool(String),

    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },

    #[error("vector store error: {0}")]
    VectorStore(String),

    #[error("audio pipeline error: {0}")]
    Audio(String),

    #[error("ocr pipeline error: {0}")]
    Ocr(String),

    #[error("crypto/memory error: {0}")]
    Crypto(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
