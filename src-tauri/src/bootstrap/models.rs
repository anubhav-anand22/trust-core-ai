//! Model-tier recommendation + on-demand model pulls.
//!
//! The blueprint wants the app to "detect system specs … and based on that install
//! a compatible model". [`recommend_models`] does the picking; [`ensure_models`]
//! does the pulling (adapted from the scaffold's `ensure_required_models` — same
//! streaming progress loop, but the model list is a parameter now).

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::State;
use tokio_stream::StreamExt;

use super::hardware::{detect_hardware, HardwareInfo};
use crate::AppState;

/// The three models one session runs on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPlan {
    /// Resident text model — roles A/B/C, kept alive indefinitely.
    pub llm: String,
    /// Compact vision model, called with `keep_alive = 0`.
    pub vision: String,
    /// Embedding model for RAG, called with `keep_alive = 0`.
    pub embed: String,
    /// Human-readable tier label for the bootstrap screen.
    pub tier_label: String,
}

impl ModelPlan {
    /// Conservative default used before the user has confirmed a tier.
    pub fn fallback() -> Self {
        Self {
            llm: "qwen2.5:3b-instruct".into(),
            vision: "moondream".into(),
            embed: "nomic-embed-text".into(),
            tier_label: "default".into(),
        }
    }

    /// Every model this plan needs, resident model first.
    pub fn all(&self) -> Vec<String> {
        vec![self.llm.clone(), self.vision.clone(), self.embed.clone()]
    }
}

/// Choose a resident LLM the host can actually run.
///
/// **Input:** the hardware probe. **Output:** a [`ModelPlan`]. Dedicated VRAM is
/// used when known; otherwise system RAM, which is always reliable. Vision and
/// embedding models are small and fixed across every tier.
pub fn recommend_models(hw: &HardwareInfo) -> ModelPlan {
    let vram = hw.vram_gb.unwrap_or(0.0);
    let ram = hw.total_ram_gb;

    let (llm, tier) = if vram >= 8.0 || ram >= 16.0 {
        ("qwen2.5:7b-instruct", "7B tier — ≥8 GB VRAM or ≥16 GB RAM")
    } else if vram >= 6.0 || ram >= 12.0 {
        ("phi4-mini", "mini tier — ≥6 GB VRAM or ≥12 GB RAM")
    } else {
        ("qwen2.5:1.5b-instruct", "lite tier — CPU / low RAM")
    };

    ModelPlan {
        llm: llm.into(),
        vision: "moondream".into(),
        embed: "nomic-embed-text".into(),
        tier_label: tier.into(),
    }
}

/// Hardware + recommendation in one call, for the bootstrap screen.
#[derive(Clone, Debug, Serialize)]
pub struct SystemProbe {
    pub hardware: HardwareInfo,
    pub recommended: ModelPlan,
}

#[tauri::command]
pub fn probe_system() -> SystemProbe {
    let hardware = detect_hardware();
    let recommended = recommend_models(&hardware);
    SystemProbe {
        hardware,
        recommended,
    }
}

/// Progress line for one model pull, streamed to the bootstrap screen.
#[derive(Clone, Serialize)]
pub struct ModelProgress {
    pub model: String,
    pub message: String,
    /// 0-100 when the pull reports byte counts, else `None`.
    pub percentage: Option<u8>,
}

/// Ensure each requested model is present locally, pulling any that are missing
/// and streaming download progress on `on_progress`.
///
/// **Inputs:** the model names to guarantee, a progress channel, app state (for the
/// Ollama client). **Output:** `Ok(())` once all are present, or the first pull
/// error.
#[tauri::command]
pub async fn ensure_models(
    models: Vec<String>,
    on_progress: Channel<ModelProgress>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    for model_name in &models {
        let target = if model_name.contains(':') {
            model_name.clone()
        } else {
            format!("{model_name}:latest")
        };

        let local = state
            .ollama
            .list_local_models()
            .await
            .map_err(|e| e.to_string())?;
        if local.iter().any(|m| m.name == target || &m.name == model_name) {
            let _ = on_progress.send(ModelProgress {
                model: model_name.clone(),
                message: format!("✔ \"{model_name}\" already installed."),
                percentage: Some(100),
            });
            continue;
        }

        let _ = on_progress.send(ModelProgress {
            model: model_name.clone(),
            message: format!("Pulling \"{model_name}\"…"),
            percentage: Some(0),
        });

        let mut stream = state
            .ollama
            .pull_model_stream(model_name.clone(), false)
            .await
            .map_err(|e| e.to_string())?;

        while let Some(res) = stream.next().await {
            match res {
                Ok(progress) => {
                    let percentage = match (progress.total, progress.completed) {
                        (Some(total), Some(done)) if total > 0 => {
                            Some(((done as f64 / total as f64) * 100.0).round() as u8)
                        }
                        _ => None,
                    };
                    let _ = on_progress.send(ModelProgress {
                        model: model_name.clone(),
                        message: progress.message,
                        percentage,
                    });
                }
                Err(e) => return Err(format!("failed pulling {model_name}: {e}")),
            }
        }

        let _ = on_progress.send(ModelProgress {
            model: model_name.clone(),
            message: format!("✔ \"{model_name}\" ready."),
            percentage: Some(100),
        });
    }
    Ok(())
}
