//! Headless end-to-end run of the full pipeline against a live Ollama server and
//! real files. This is the Phase 2 verification gate that needs models running.
//!
//! ```text
//! cargo run -p workbench-core --example e2e -- "<prompt>" [file ...]
//! ```
//!
//! Env overrides: `WB_LLM`, `WB_VISION`, `WB_EMBED`, `WB_DATA_DIR`, `WB_KB_DIR`,
//! `WB_MODELS_DIR` (whisper/ocrs model files).

use std::path::PathBuf;

use workbench_core::engine::schemas::{FileKind, InputFile};
use workbench_core::engine::OllamaEngine;
use workbench_core::events::{ProgressSink, StepEvent};
use workbench_core::tools::default_registry;
use workbench_core::{run_turn, PipelineConfig, TurnOutcome};

/// Prints each pipeline checkpoint as it happens.
struct StdoutSink;
impl ProgressSink for StdoutSink {
    fn emit(&self, event: StepEvent) {
        match &event {
            StepEvent::Done { .. } => println!("  * done"),
            other => println!("  * {}", serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let prompt = args.next().unwrap_or_else(|| {
        "Assess corrosion risk on pump P-101 and check it against our SOPs.".to_string()
    });
    let file_args: Vec<String> = args.collect();

    let data_dir = env_path("WB_DATA_DIR", "target/e2e-data");
    let kb_dir = env_path("WB_KB_DIR", "crates/workbench-core/kb");
    let models_dir = std::env::var("WB_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| app_data_dir().join("models"));
    std::fs::create_dir_all(&data_dir).unwrap();

    let config = PipelineConfig {
        data_dir,
        kb_dir,
        ollama_url: "http://127.0.0.1:11434".to_string(),
        llm_model: std::env::var("WB_LLM").unwrap_or_else(|_| "llama3.2:3b".to_string()),
        vision_model: std::env::var("WB_VISION").unwrap_or_else(|_| "moondream".to_string()),
        embed_model: std::env::var("WB_EMBED").unwrap_or_else(|_| "nomic-embed-text".to_string()),
        whisper_model_path: models_dir.join("ggml-base.en.bin"),
        ocr_detection_model: models_dir.join("text-detection.rten"),
        ocr_recognition_model: models_dir.join("text-recognition.rten"),
    };

    let uploads: Vec<InputFile> = file_args
        .iter()
        .map(|p| {
            let path = PathBuf::from(p);
            InputFile {
                kind: FileKind::from_path(&path),
                original_name: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string(),
                path: path
                    .canonicalize()
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned(),
            }
        })
        .collect();

    println!("prompt : {prompt}");
    println!(
        "files  : {:?}",
        uploads
            .iter()
            .map(|f| format!("{} [{:?}]", f.original_name, f.kind))
            .collect::<Vec<_>>()
    );
    println!(
        "models : llm={} vision={} embed={}",
        config.llm_model, config.vision_model, config.embed_model
    );
    println!("---- run ----");

    let engine = OllamaEngine::new(
        &config.ollama_url,
        config.llm_model.clone(),
        config.vision_model.clone(),
        config.embed_model.clone(),
    );
    let registry = default_registry();

    match run_turn(
        &engine,
        &registry,
        &config,
        "e2e-session",
        &prompt,
        &uploads,
        &StdoutSink,
    )
    .await
    {
        Ok(TurnOutcome::Completed(report)) => {
            println!("\n==== FINAL REPORT ====");
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        Ok(TurnOutcome::AwaitingUser { errors, plan_json }) => {
            println!("\n==== AWAITING USER ====");
            for e in errors {
                println!("  - {e}");
            }
            println!("last plan: {plan_json}");
        }
        Err(e) => {
            eprintln!("\n!! pipeline error: {e}");
            std::process::exit(1);
        }
    }
}

fn env_path(key: &str, default: &str) -> PathBuf {
    std::env::var(key)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(default))
}

fn app_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("com.anubhav_anand.tauri-app")
}
