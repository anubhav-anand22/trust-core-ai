//! Model-tier recommendation + on-demand model pulls.
//!
//! The blueprint wants the app to "detect system specs … and based on that install
//! a compatible model". [`recommend_models`] does the picking; [`ensure_models`]
//! does the pulling (adapted from the scaffold's `ensure_required_models` — same
//! streaming progress loop, but the model list is a parameter now).

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{Manager, State};
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
    /// Context window the resident model should run with, chosen with the tier.
    #[serde(default = "default_num_ctx")]
    pub num_ctx: u64,
}

fn default_num_ctx() -> u64 {
    8192
}

impl ModelPlan {
    /// Conservative default used before the user has confirmed a tier.
    ///
    /// Deliberately the *floor* model: this is what runs if the confirmed plan was
    /// lost, and a too-small model gives a slow-but-working answer where a
    /// too-large one can swap the machine to a halt.
    pub fn fallback() -> Self {
        Self {
            llm: LLM_FLOOR.name.into(),
            vision: "moondream".into(),
            embed: "nomic-embed-text".into(),
            tier_label: "default (floor model)".into(),
            num_ctx: LLM_FLOOR.num_ctx,
        }
    }

    /// Every model this plan needs, resident model first.
    pub fn all(&self) -> Vec<String> {
        vec![self.llm.clone(), self.vision.clone(), self.embed.clone()]
    }
}

/// A resident-LLM option and what it actually costs to run.
///
/// The old tier logic compared core counts and RAM against bare thresholds, which
/// could not answer the only question that matters — *will this model fit on this
/// machine right now?* Carrying the sizes lets us check instead of guess.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct LlmSpec {
    /// Ollama tag.
    pub name: &'static str,
    /// Approximate resident footprint in GB: q4 weights plus KV-cache headroom.
    pub ram_gb: f32,
    /// Approximate download size in GB.
    pub download_gb: f32,
    /// Context window to run this model with. The KV cache grows with this, and
    /// on a small CPU model that memory and the longer prefill are a real cost —
    /// so the smallest tier runs at 4096, not the blanket 8192 the engine
    /// defaulted to on every machine.
    pub num_ctx: u64,
}

// The resident-model ladder. Every one of these is an *instruct* model, because the
// pipeline constrains decoding to a JSON schema (`FormatType::StructuredJson`) and
// needs reliable schema-following all the way down to 0.5B.
//
// Thinking models are deliberately absent: `qwen3:4b` emits its reasoning before the
// JSON and returns an empty body under structured output, surfacing as
// `EOF while parsing a value at line 1 column 0`. That is an incompatibility, not a
// size problem — do not add one back to this list.
pub const LLM_MINIMAL: LlmSpec = LlmSpec {
    name: "qwen2.5:0.5b-instruct",
    ram_gb: 0.9,
    download_gb: 0.4,
    num_ctx: 4096,
};
pub const LLM_FLOOR: LlmSpec = LlmSpec {
    name: "qwen2.5:1.5b-instruct",
    ram_gb: 1.8,
    download_gb: 1.0,
    num_ctx: 8192,
};
pub const LLM_STANDARD: LlmSpec = LlmSpec {
    name: "qwen2.5:3b-instruct",
    ram_gb: 2.9,
    download_gb: 1.9,
    num_ctx: 8192,
};
pub const LLM_LARGE: LlmSpec = LlmSpec {
    name: "qwen2.5:7b-instruct",
    ram_gb: 5.6,
    download_gb: 4.7,
    num_ctx: 8192,
};

/// Everything the bootstrap dropdown may offer, smallest first.
pub const LLM_CATALOG: &[LlmSpec] = &[
    LLM_MINIMAL,
    LLM_FLOOR,
    LLM_STANDARD,
    LlmSpec {
        name: "llama3.2:3b",
        ram_gb: 3.0,
        download_gb: 2.0,
        num_ctx: 8192,
    },
    LlmSpec {
        name: "phi4-mini",
        ram_gb: 3.8,
        download_gb: 2.5,
        num_ctx: 8192,
    },
    LLM_LARGE,
];

/// The two specialists, identical on every tier.
const VISION: LlmSpec = LlmSpec {
    name: "moondream",
    ram_gb: 1.7,
    download_gb: 1.7,
    num_ctx: 2048,
};
const EMBED: LlmSpec = LlmSpec {
    name: "nomic-embed-text",
    ram_gb: 0.5,
    download_gb: 0.3,
    num_ctx: 2048,
};

/// RAM to leave for the OS, the webview and Ollama's own overhead. Without this the
/// arithmetic says a model "fits" right up to the point the machine starts swapping.
const HOST_HEADROOM_GB: f32 = 1.5;

/// Catalogue entry for a model name, if we know it.
pub fn lookup_llm(name: &str) -> Option<LlmSpec> {
    LLM_CATALOG.iter().copied().find(|m| m.name == name)
}

/// Choose a resident LLM the host can actually run.
///
/// **Input:** the hardware probe. **Output:** a [`ModelPlan`].
///
/// Two different worlds. With GPU offload, VRAM sets the ceiling and a 7B is
/// comfortable. Without it every token is computed on the CPU, and **model size is
/// the latency**: RAM only decides whether the weights fit, not how fast they run.
/// So the CPU ladder is gated on *physical* cores (hyperthreads share execution
/// units and add little throughput) and on *available* rather than total RAM.
pub fn recommend_models(hw: &HardwareInfo) -> ModelPlan {
    let cores = hw.physical_cores;
    let free = hw.available_ram_gb;

    let (llm, tier) = if hw.has_usable_gpu() {
        let vram = hw.vram_gb.unwrap_or(0.0);
        if vram >= 8.0 && hw.total_ram_gb >= 8.0 {
            (LLM_LARGE, "7B tier — GPU offload, ≥8 GB VRAM")
        } else if vram >= 6.0 {
            (LLM_STANDARD, "3B tier — GPU offload, ≥6 GB VRAM")
        } else {
            (LLM_FLOOR, "1.5B tier — GPU offload, <6 GB VRAM")
        }
    } else if cores >= 6 && free >= 10.0 {
        (LLM_STANDARD, "3B tier — CPU-only, ≥6 physical cores")
    } else if cores >= 4 && free >= 5.0 {
        (
            LLM_FLOOR,
            "1.5B tier — CPU-only (small model keeps turns responsive)",
        )
    } else if cores >= 2 && free >= 3.0 {
        (
            LLM_MINIMAL,
            "0.5B tier — CPU-only, minimal host (answers will be rough)",
        )
    } else {
        (
            LLM_MINIMAL,
            "below minimum — this machine cannot run the workbench well",
        )
    };

    ModelPlan {
        llm: llm.name.into(),
        vision: VISION.name.into(),
        embed: EMBED.name.into(),
        tier_label: tier.into(),
        num_ctx: llm.num_ctx,
    }
}

/// How risky is running `model` on this host? Drives the bootstrap warning.
#[derive(Clone, Debug, Serialize)]
pub struct LlmAssessment {
    pub model: String,
    pub approx_ram_gb: f32,
    /// Total download for the resident model plus both specialists.
    pub approx_download_gb: f32,
    /// `"ok"` · `"caution"` · `"blocked"`.
    pub severity: String,
    /// Plain-language reasons, shown verbatim to the user.
    pub warnings: Vec<String>,
}

/// Judge a resident-model choice against the host.
///
/// Exists so that overriding the recommendation is an *informed* choice. Previously
/// an override produced only a log line the user never saw, and picking a large
/// model on a small host was the documented cause of unusable turn times.
pub fn assess_llm(model: &str, hw: &HardwareInfo) -> LlmAssessment {
    let recommended = recommend_models(hw);
    let spec = lookup_llm(model);
    let mut warnings = Vec::new();
    let mut severity = "ok";

    let (ram_gb, download_gb) = match spec {
        Some(m) => (m.ram_gb, m.download_gb),
        None => {
            severity = "caution";
            warnings.push(format!(
                "\"{model}\" is not in the tested catalogue, so its memory cost is \
                 unknown. Models that emit reasoning before their answer (any \
                 \"thinking\" model) return an empty result under this app's \
                 structured-output mode."
            ));
            (0.0, 0.0)
        }
    };

    // Does it fit beside the OS, the webview and Ollama itself?
    if ram_gb > 0.0 {
        let needed = ram_gb + HOST_HEADROOM_GB;
        if needed > hw.available_ram_gb {
            severity = "blocked";
            warnings.push(format!(
                "Needs about {needed:.1} GB free but only {:.1} GB is available. Expect \
                 heavy swapping or an out-of-memory failure mid-turn. Close other \
                 applications, or choose a smaller model.",
                hw.available_ram_gb
            ));
        }
    }

    // It fits — but is it sane on this many cores?
    if severity != "blocked" && !hw.has_usable_gpu() {
        if let (Some(chosen), Some(rec)) = (spec, lookup_llm(&recommended.llm)) {
            if chosen.ram_gb > rec.ram_gb * 1.25 {
                let factor = (chosen.ram_gb / rec.ram_gb).max(1.0);
                severity = "caution";
                warnings.push(format!(
                    "Larger than the recommendation for {} physical core(s) with no GPU. \
                     Every token is computed on the CPU, so expect turns roughly \
                     {factor:.1}× slower than with \"{}\".",
                    hw.physical_cores, rec.name
                ));
            }
        }
    }

    LlmAssessment {
        model: model.to_string(),
        approx_ram_gb: ram_gb,
        approx_download_gb: download_gb + VISION.download_gb + EMBED.download_gb,
        severity: severity.into(),
        warnings,
    }
}

/// Hardware + recommendation in one call, for the bootstrap screen.
#[derive(Clone, Debug, Serialize)]
pub struct SystemProbe {
    pub hardware: HardwareInfo,
    pub recommended: ModelPlan,
    /// The recommendation judged against this host. Always `"ok"` for the
    /// recommended model itself, but the field is here so the bootstrap screen
    /// gets the download size without a second call.
    pub assessment: LlmAssessment,
}

#[tauri::command]
pub fn probe_system() -> SystemProbe {
    let hardware = detect_hardware();
    let recommended = recommend_models(&hardware);
    let assessment = assess_llm(&recommended.llm, &hardware);
    tracing::info!(
        recommended = %recommended.llm,
        tier = %recommended.tier_label,
        approx_download_gb = assessment.approx_download_gb,
        "system probe -> recommendation"
    );
    SystemProbe {
        hardware,
        recommended,
        assessment,
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

/// Free space, in GB, on the volume that holds `path`.
///
/// Returns `None` if the volume can't be identified — a missing disk reading must
/// not block a download, only an affirmatively-too-small one does.
fn free_disk_gb(path: &std::path::Path) -> Option<f32> {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        // Longest mount point wins: on Windows every path starts with `C:\`, but a
        // data dir under `D:\` should match `D:\`, not `C:\`.
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space() as f32 / (1024.0 * 1024.0 * 1024.0))
}

/// Rough download budget for a set of model tags not yet present, in GB.
fn download_estimate_gb(missing: &[String]) -> f32 {
    missing
        .iter()
        .map(|name| {
            lookup_llm(name)
                .map(|m| m.download_gb)
                .or_else(|| match name.as_str() {
                    n if n.starts_with("moondream") => Some(1.7),
                    n if n.starts_with("nomic-embed") => Some(0.3),
                    _ => None,
                })
                // An unknown tag: assume a middleweight model so the check errs
                // toward warning rather than toward a surprise out-of-space.
                .unwrap_or(2.5)
        })
        .sum()
}

/// Ensure each requested model is present locally, pulling any that are missing
/// and streaming download progress on `on_progress`.
///
/// **Inputs:** the model names to guarantee, a progress channel, app state (for the
/// Ollama client). **Output:** `Ok(())` once all are present, or the first pull
/// error.
///
/// Before pulling anything it checks free disk against the estimated download so
/// the user is told up front, not after a 20-minute partial download fills the
/// drive — a real failure mode on the machines this targets.
#[tauri::command]
pub async fn ensure_models(
    models: Vec<String>,
    on_progress: Channel<ModelProgress>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // --- what is actually missing -----------------------------------------
    let local = state
        .ollama
        .list_local_models()
        .await
        .map_err(|e| e.to_string())?;
    let is_present = |name: &str| {
        let target = if name.contains(':') {
            name.to_string()
        } else {
            format!("{name}:latest")
        };
        local.iter().any(|m| m.name == target || m.name == name)
    };
    let missing: Vec<String> = models.iter().filter(|m| !is_present(m)).cloned().collect();

    // --- disk pre-flight -------------------------------------------------
    if !missing.is_empty() {
        if let Ok(data_dir) = app.path().app_data_dir() {
            let need = download_estimate_gb(&missing);
            if let Some(free) = free_disk_gb(&data_dir) {
                tracing::info!(
                    missing = ?missing,
                    estimate_gb = need,
                    free_gb = free,
                    "model download pre-flight"
                );
                // Ollama needs headroom to unpack; ask for the download plus 2 GB.
                if free < need + 2.0 {
                    return Err(format!(
                        "Not enough disk space: about {need:.1} GB to download (plus \
                         unpack room) but only {free:.1} GB free on the drive holding \
                         {}. Free up space and try again.",
                        data_dir.display()
                    ));
                }
            }
        }
    }

    // --- pull loop ------------------------------------------------------
    for model_name in &models {
        if is_present(model_name) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `logical` defaults to 2x physical — the hyperthreaded case that used to fool
    /// the tier logic into reading a 4-core laptop as an 8-core machine.
    fn hw(physical: usize, free_ram: f32, vram: Option<f32>, cuda: bool) -> HardwareInfo {
        HardwareInfo {
            total_ram_gb: free_ram + 4.0,
            available_ram_gb: free_ram,
            cpu_cores: physical * 2,
            physical_cores: physical,
            gpu_vendor: if cuda { "NVIDIA".into() } else { "none".into() },
            gpu_name: if cuda { "test gpu".into() } else { "none".into() },
            vram_gb: vram,
            cuda_available: cuda,
        }
    }

    /// The regression that made the app unusable: a CPU-only host with plenty of RAM
    /// was handed a multi-billion-parameter model because the rule read
    /// `vram >= 8 || ram >= 16`. RAM says the weights *fit*, not that they run fast.
    #[test]
    fn plenty_of_ram_without_a_gpu_does_not_get_a_big_model() {
        let plan = recommend_models(&hw(4, 12.0, None, false));
        assert_eq!(plan.llm, LLM_FLOOR.name, "got {}", plan.tier_label);
    }

    /// The declared floor we must never freeze: 8 GB RAM, no GPU, 4 physical cores.
    #[test]
    fn the_eight_gb_no_gpu_floor_gets_the_floor_model() {
        // 8 GB total with a browser open leaves roughly this much.
        let plan = recommend_models(&hw(4, 5.5, None, false));
        assert_eq!(plan.llm, LLM_FLOOR.name, "got {}", plan.tier_label);
        assert_eq!(
            assess_llm(&plan.llm, &hw(4, 5.5, None, false)).severity,
            "ok",
            "the recommendation must never warn about itself"
        );
    }

    /// Hyperthreads must not promote a machine a tier. Same silicon as the floor
    /// case, and the old logical-core rule read it as 8 cores.
    #[test]
    fn hyperthreads_do_not_promote_a_tier() {
        let host = hw(4, 12.0, None, false);
        assert_eq!(host.cpu_cores, 8, "test fixture should look hyperthreaded");
        assert_eq!(recommend_models(&host).llm, LLM_FLOOR.name);
        assert_eq!(host.worker_threads(), 3, "3 of 4 physical cores, not 7 of 8");
    }

    #[test]
    fn a_real_gpu_still_gets_the_big_model() {
        let plan = recommend_models(&hw(8, 24.0, Some(12.0), true));
        assert_eq!(plan.llm, LLM_LARGE.name);
    }

    /// An integrated adapter reporting no usable VRAM is a CPU host, whatever it is
    /// called.
    #[test]
    fn integrated_graphics_counts_as_cpu_only() {
        let plan = recommend_models(&hw(4, 12.0, Some(1.0), false));
        assert_eq!(plan.llm, LLM_FLOOR.name, "got {}", plan.tier_label);
    }

    #[test]
    fn many_cores_and_free_ram_can_afford_the_mid_model() {
        assert_eq!(recommend_models(&hw(8, 16.0, None, false)).llm, LLM_STANDARD.name);
    }

    /// A machine under real memory pressure must drop a tier even with the cores for
    /// a bigger model — available RAM, not total, is what decides.
    #[test]
    fn memory_pressure_drops_a_tier() {
        assert_eq!(recommend_models(&hw(8, 6.0, None, false)).llm, LLM_FLOOR.name);
        assert_eq!(recommend_models(&hw(8, 3.5, None, false)).llm, LLM_MINIMAL.name);
    }

    #[test]
    fn a_machine_that_cannot_cope_says_so() {
        let plan = recommend_models(&hw(1, 2.0, None, false));
        assert!(
            plan.tier_label.contains("below minimum"),
            "expected an explicit refusal, got {}",
            plan.tier_label
        );
    }

    #[test]
    fn one_core_is_always_left_for_the_os() {
        assert_eq!(hw(4, 12.0, None, false).worker_threads(), 3);
        assert_eq!(hw(1, 8.0, None, false).worker_threads(), 1);
    }

    // --- assessment -------------------------------------------------------

    /// The override warning that did not exist: a model too big for free RAM must be
    /// blocked, with the numbers in the message.
    #[test]
    fn a_model_that_cannot_fit_is_blocked() {
        let a = assess_llm(LLM_LARGE.name, &hw(4, 4.0, None, false));
        assert_eq!(a.severity, "blocked", "{:?}", a.warnings);
        assert!(
            a.warnings.iter().any(|w| w.contains("4.0 GB is available")),
            "the warning should quote the real numbers: {:?}",
            a.warnings
        );
    }

    /// Fits in RAM, but far bigger than this host should run: caution, not silence.
    #[test]
    fn an_oversized_but_fitting_model_warns() {
        let a = assess_llm(LLM_STANDARD.name, &hw(4, 12.0, None, false));
        assert_eq!(a.severity, "caution", "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("slower")));
    }

    /// An unknown tag cannot be sized, and thinking models are the trap worth naming.
    #[test]
    fn an_unknown_model_is_flagged() {
        let a = assess_llm("qwen3:4b", &hw(8, 16.0, None, false));
        assert_eq!(a.severity, "caution");
        assert!(a.warnings.iter().any(|w| w.contains("thinking")));
    }

    /// The download figure must cover the specialists too, or the disk check lies.
    #[test]
    fn download_size_includes_the_specialists() {
        let a = assess_llm(LLM_FLOOR.name, &hw(4, 12.0, None, false));
        assert!(
            (a.approx_download_gb - (LLM_FLOOR.download_gb + 1.7 + 0.3)).abs() < 0.01,
            "got {}",
            a.approx_download_gb
        );
    }

    /// Every dropdown option must be sizeable, or the fit check silently degrades to
    /// "unknown" for a model we actually ship.
    #[test]
    fn every_catalogue_entry_is_self_consistent() {
        for m in LLM_CATALOG {
            assert!(m.ram_gb > 0.0 && m.download_gb > 0.0, "{} has no sizes", m.name);
            assert!(
                m.ram_gb >= m.download_gb,
                "{}: resident cost should exceed download size (KV cache)",
                m.name
            );
            assert!(lookup_llm(m.name).is_some(), "{} not findable", m.name);
        }
    }
}
