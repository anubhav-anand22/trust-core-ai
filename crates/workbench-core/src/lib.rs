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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cooperative stop signal shared between the Tauri command that starts a turn
/// and the pipeline running it.
///
/// The pipeline is one long `await` with no way in from outside; this is the way
/// in. It is polled before every model call and between every tool step, so a
/// user pressing **Stop** ends the turn within a step rather than after it. Tools
/// that block a thread (PDF parsing, OCR, whisper) still run to the end of the
/// *current* step — nothing can safely kill a `spawn_blocking` mid-call — but no
/// further step begins.
///
/// Cloning shares the same underlying flag.
/// How hard the pipeline should work on a long document.
///
/// * `Fast` — retrieve the passages that bear on the question and answer from
///   those. One analysis call per step; seconds on a small machine.
/// * `Deep` — map over the *whole* document in windows, analyse each, then reduce
///   the partials into one answer. Nothing is skipped, at the cost of one model
///   call per window — minutes on CPU, so it is opt-in and reports per-window
///   progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnMode {
    #[default]
    Fast,
    Deep,
}

impl TurnMode {
    /// Parse the string the front-end sends (`"fast"` / `"deep"`); anything else
    /// is treated as `Fast`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "deep" => Self::Deep,
            _ => Self::Fast,
        }
    }
}

#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(CoreError::Cancelled)` if stop was requested, else `Ok(())`.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(CoreError::Cancelled)
        } else {
            Ok(())
        }
    }
}

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
    pub fn persistent_path(&self) -> PathBuf { self.data_dir.join("persistent_memory.json") }

    /// Directory holding one JSON file per chat session.
    ///
    /// The old single `session_context.json` was shared by every session, so
    /// starting a new chat silently overwrote the previous one and a page reload
    /// (which minted a fresh id) orphaned it. One file per id fixes both, and
    /// lets the UI list past chats.
    pub fn sessions_dir(&self) -> PathBuf { self.data_dir.join("sessions") }

    /// On-disk path for one session. `session_id` is sanitised to a safe file
    /// stem so it cannot escape `sessions/`.
    pub fn session_path_for(&self, session_id: &str) -> PathBuf {
        let stem: String = session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
            .collect();
        let stem = if stem.is_empty() { "session".to_string() } else { stem };
        self.sessions_dir().join(format!("{stem}.json"))
    }

    /// Legacy single-file path, kept so a one-time migration can find it.
    pub fn legacy_session_path(&self) -> PathBuf {
        self.data_dir.join("session_context.json")
    }
}

/// Unified error for the whole pipeline.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("ollama request failed: {0}")]
    Ollama(String),

    #[error("{what} did not finish within {secs}s and was abandoned")]
    Timeout { what: String, secs: u64 },

    #[error("the turn was cancelled")]
    Cancelled,

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
