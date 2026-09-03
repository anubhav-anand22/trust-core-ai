//! Phase 2 gate (offline portion): the tool registry is complete, and the tools
//! that guard on inputs degrade cleanly when those inputs are missing — without
//! needing Ollama or model files.
//!
//! The full end-to-end (real PDF + image + audio + a running Ollama) is exercised
//! from the desktop app; see the plan's verification section.

use std::collections::HashMap;
use std::path::PathBuf;

use workbench_core::engine::schemas::TaskStep;
use workbench_core::engine::OllamaEngine;
use workbench_core::planner::TASK_REGISTRY;
use workbench_core::tools::{default_registry, ToolContext};
use workbench_core::PipelineConfig;

fn cfg(dir: &std::path::Path) -> PipelineConfig {
    PipelineConfig {
        data_dir: dir.to_path_buf(),
        kb_dir: dir.join("kb"),
        ollama_url: "http://127.0.0.1:11434".into(),
        llm_model: "test-llm".into(),
        vision_model: "test-vision".into(),
        embed_model: "test-embed".into(),
        whisper_model_path: dir.join("models/ggml-base.en.bin"),
        ocr_detection_model: dir.join("models/text-detection.rten"),
        ocr_recognition_model: dir.join("models/text-recognition.rten"),
    }
}

fn step(task: &str) -> TaskStep {
    TaskStep {
        id: "s1".into(),
        task: task.into(),
        args: serde_json::Value::Null,
        depends_on: vec![],
    }
}

#[test]
fn registry_covers_every_task() {
    let registry = default_registry();
    for spec in TASK_REGISTRY {
        assert!(
            registry.get(spec.name).is_some(),
            "no tool registered for `{}`",
            spec.name
        );
    }
    assert!(!registry.is_empty());
}

#[tokio::test]
async fn extraction_tools_fail_cleanly_with_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = cfg(dir.path());
    let engine = OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c");
    let outputs: HashMap<String, serde_json::Value> = HashMap::new();
    let ctx = ToolContext {
        config: &config,
        uploads: &[], // nothing attached
        outputs: &outputs,
        prompt: "inspect pump P-101",
        engine: &engine,
    };

    // These four check for their input file before touching Ollama or a model.
    let registry = default_registry();
    for task in ["transcribe_audio", "parse_pdf", "ocr_image", "analyze_image"] {
        let tool = registry.get(task).expect("tool present");
        let result = tool.run(&step(task), &ctx).await.unwrap();
        assert!(!result.ok, "`{task}` should report failure with no file");
        assert!(
            result.error.as_deref().unwrap_or("").contains("attached"),
            "`{task}` error should mention the missing attachment, got {:?}",
            result.error
        );
        assert_eq!(result.step_id, "s1");
    }
}

#[tokio::test]
async fn analysis_tool_fails_cleanly_with_no_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let config = cfg(dir.path());
    let engine = OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c");
    let outputs: HashMap<String, serde_json::Value> = HashMap::new();
    let ctx = ToolContext {
        config: &config,
        uploads: &[],
        outputs: &outputs,
        prompt: "summarise",
        engine: &engine,
    };

    let registry = default_registry();
    let tool = registry.get("summarize").unwrap();
    let result = tool.run(&step("summarize"), &ctx).await.unwrap();
    assert!(!result.ok);
    assert!(result.error.as_deref().unwrap_or("").contains("evidence"));
}

#[test]
fn config_paths_resolve_under_data_dir() {
    let config = cfg(&PathBuf::from("/data"));
    assert!(config.session_path().ends_with("session_context.json"));
    assert!(config.persistent_path().ends_with("persistent_memory.json"));
    assert!(config.uploads_dir().ends_with("uploads"));
}
