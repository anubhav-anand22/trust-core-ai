//! One-time environment setup: Ollama install, hardware probe, model pulls.
//!
//! Everything here is Tauri-coupled (commands + IPC channels) and runs *before*
//! the pipeline in `workbench-core` does any work.

pub mod hardware;
pub mod models;
pub mod ollama;
