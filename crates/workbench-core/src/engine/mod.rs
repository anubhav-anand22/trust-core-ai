//! The inference layer: one resident model, three prompt-swapped roles.

pub mod ollama_engine;
pub mod schemas;

pub use ollama_engine::OllamaEngine;
