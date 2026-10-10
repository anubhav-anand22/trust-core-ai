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

/// Is `name` among the locally installed tags? Ollama lists an untagged pull as
/// `name:latest`, so `moondream` matches `moondream:latest`.
///
/// The single definition of "installed" — the bootstrap screen's labels, the
/// download estimate and the pull loop in [`ensure_models`] all go through it,
/// so the screen can never claim a model is present that the pull then fetches.
pub fn model_present(installed: &[String], name: &str) -> bool {
    let target = if name.contains(':') {
        name.to_string()
    } else {
        format!("{name}:latest")
    };
    installed.iter().any(|m| *m == target || m == name)
}

/// Every model tag Ollama has locally, or empty (with a warning) if the server
/// can't be asked — a failed listing must not block the bootstrap screen, it
/// just means nothing is marked installed.
async fn installed_models(state: &AppState) -> Vec<String> {
    match state.ollama.list_local_models().await {
        Ok(list) => list.into_iter().map(|m| m.name).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list local models; treating none as installed");
            Vec::new()
        }
    }
}

/// Models a pre-existing **system-wide** Linux Ollama would have pulled. That
/// install keeps them under the `ollama` system user's home, which the
/// per-user server (see `bootstrap::ollama`) cannot read without sudo — so they
/// look missing and would be downloaded again. Detected only to tell the user
/// why, not to read them.
fn legacy_system_models_present() -> bool {
    cfg!(target_os = "linux")
        && std::path::Path::new("/usr/share/ollama/.ollama/models").is_dir()
}

/// Choose a resident LLM the host can actually run.
///
/// **Input:** the hardware probe. **Output:** a [`ModelPlan`].
///
/// Two different worlds. With GPU offload, VRAM sets the ceiling and a 7B is
/// comfortable. Without it every token is computed on the CPU, and **model size is
/// the latency**: RAM only decides whether the weights fit, not how fast they run.
/// So the CPU ladder is gated on *physical* cores (hyperthreads share execution
/// units and add little throughput) and on **total** RAM.
///
/// Total, not available: available RAM moves with whatever else is open, so the
/// tier jumped between launches (open a browser, lose a tier). The tier is a
/// property of the machine; whether it fits *right now* is a separate, live
/// question that [`assess_llm`] answers with a warning instead.
pub fn recommend_models(hw: &HardwareInfo) -> ModelPlan {
    let cores = hw.physical_cores;
    let ram = hw.total_ram_gb;

    let (llm, tier) = if hw.has_usable_gpu() {
        let vram = hw.vram_gb.unwrap_or(0.0);
        if vram >= 8.0 && hw.total_ram_gb >= 8.0 {
            (LLM_LARGE, "7B tier — GPU offload, ≥8 GB VRAM")
        } else if vram >= 6.0 {
            (LLM_STANDARD, "3B tier — GPU offload, ≥6 GB VRAM")
        } else {
            (LLM_FLOOR, "1.5B tier — GPU offload, <6 GB VRAM")
        }
    // Thresholds sit just under the marketed sizes because the OS reports usable
    // GiB: a "16 GB" laptop shows ~15.5, an "8 GB" ~7.7, a "4 GB" ~3.7.
    } else if cores >= 6 && ram >= 14.0 {
        (LLM_STANDARD, "3B tier — CPU-only, ≥6 physical cores")
    } else if cores >= 4 && ram >= 7.0 {
        (
            LLM_FLOOR,
            "1.5B tier — CPU-only (small model keeps turns responsive)",
        )
    } else if cores >= 2 && ram >= 3.5 {
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
    /// Download still needed for the resident model plus both specialists —
    /// models already installed count as zero.
    pub approx_download_gb: f32,
    /// Which of (resident, vision, embed) are not installed yet. Empty means
    /// "Start" needs no download at all.
    pub missing_models: Vec<String>,
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
pub fn assess_llm(model: &str, hw: &HardwareInfo, installed: &[String]) -> LlmAssessment {
    let recommended = recommend_models(hw);
    let spec = lookup_llm(model);
    let mut warnings = Vec::new();
    let mut severity = "ok";

    let ram_gb = match spec {
        Some(m) => m.ram_gb,
        None => {
            severity = "caution";
            warnings.push(format!(
                "\"{model}\" is not in the tested catalogue, so its memory cost is \
                 unknown. Models that emit reasoning before their answer (any \
                 \"thinking\" model) return an empty result under this app's \
                 structured-output mode."
            ));
            0.0
        }
    };

    // Two different questions. Can this machine hold the model at all (total
    // RAM)? Blocked if not — no amount of closing apps fixes that. Does it fit
    // beside what is open *right now* (available RAM)? Only a caution: the user
    // can close a browser, but if they don't, the resident weights get swapped
    // out mid-generation, which is exactly what a desktop freeze feels like.
    if ram_gb > 0.0 {
        let needed = ram_gb + HOST_HEADROOM_GB;
        if needed > hw.total_ram_gb {
            severity = "blocked";
            warnings.push(format!(
                "Needs about {needed:.1} GB of RAM but this machine only has {:.1} GB in \
                 total. Choose a smaller model.",
                hw.total_ram_gb
            ));
        } else if needed > hw.available_ram_gb {
            severity = "caution";
            warnings.push(format!(
                "Needs about {needed:.1} GB free but only {:.1} GB is available right now. \
                 Close other applications before starting a turn, or the model will be \
                 swapped out mid-answer and the desktop will stall.",
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

    let missing_models: Vec<String> = [model, VISION.name, EMBED.name]
        .into_iter()
        .filter(|m| !model_present(installed, m))
        .map(str::to_string)
        .collect();

    LlmAssessment {
        model: model.to_string(),
        approx_ram_gb: ram_gb,
        approx_download_gb: download_estimate_gb(&missing_models),
        missing_models,
        severity: severity.into(),
        warnings,
    }
}

/// Hardware + recommendation in one call, for the bootstrap screen.
#[derive(Clone, Debug, Serialize)]
pub struct SystemProbe {
    pub hardware: HardwareInfo,
    pub recommended: ModelPlan,
    /// The recommendation judged against this host. The field is here so the
    /// bootstrap screen gets the download size without a second call.
    pub assessment: LlmAssessment,
    /// Locally installed tags, with Ollama's implicit `:latest` stripped so they
    /// compare directly against catalogue names (`phi4-mini:latest` →
    /// `phi4-mini`). The front-end does exact matching only; all tag rules live
    /// in [`model_present`].
    pub installed_models: Vec<String>,
    /// A system-wide Linux Ollama's model store exists, and its models are
    /// invisible to this app's per-user server. See
    /// [`legacy_system_models_present`].
    pub legacy_system_models: bool,
}

#[tauri::command]
pub async fn probe_system(state: State<'_, AppState>) -> Result<SystemProbe, String> {
    let hardware = detect_hardware();
    let recommended = recommend_models(&hardware);
    let installed = installed_models(&state).await;
    let assessment = assess_llm(&recommended.llm, &hardware, &installed);
    tracing::info!(
        recommended = %recommended.llm,
        tier = %recommended.tier_label,
        installed = ?installed,
        approx_download_gb = assessment.approx_download_gb,
        "system probe -> recommendation"
    );
    Ok(SystemProbe {
        hardware,
        recommended,
        assessment,
        installed_models: installed
            .iter()
            .map(|m| m.strip_suffix(":latest").unwrap_or(m).to_string())
            .collect(),
        legacy_system_models: legacy_system_models_present(),
    })
}

/// Judge a resident-model choice against this host, for the bootstrap override
/// warning. Re-probes the hardware so the RAM figure reflects the machine's state
/// right now, and re-lists local models so the download figure is current.
#[tauri::command]
pub async fn assess_model(model: String, state: State<'_, AppState>) -> Result<LlmAssessment, String> {
    let hw = detect_hardware();
    let installed = installed_models(&state).await;
    let assessment = assess_llm(&model, &hw, &installed);
    tracing::info!(
        model = %model,
        severity = %assessment.severity,
        approx_ram_gb = assessment.approx_ram_gb,
        missing = ?assessment.missing_models,
        warnings = assessment.warnings.len(),
        "model assessment for override"
    );
    Ok(assessment)
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
    let local: Vec<String> = state
        .ollama
        .list_local_models()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|m| m.name)
        .collect();
    let is_present = |name: &str| model_present(&local, name);
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
            assess_llm(&plan.llm, &hw(4, 5.5, None, false), &[]).severity,
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

    /// Explicit total / available, for the cases where the two must differ.
    fn hw_ram(physical: usize, total: f32, available: f32) -> HardwareInfo {
        HardwareInfo {
            total_ram_gb: total,
            available_ram_gb: available,
            ..hw(physical, available, None, false)
        }
    }

    /// The tier is a property of the machine: opening a browser (less available
    /// RAM) must not drop a tier between launches. That jumpiness is why the
    /// recommendation moved from available to total RAM.
    #[test]
    fn available_ram_does_not_move_the_tier() {
        let idle = recommend_models(&hw_ram(8, 15.5, 12.0));
        let busy = recommend_models(&hw_ram(8, 15.5, 3.0));
        assert_eq!(idle.llm, busy.llm, "tier changed with available RAM");
        assert_eq!(idle.llm, LLM_STANDARD.name);
    }

    /// A machine that is genuinely small in total still drops a tier, using the
    /// usable-GiB figures the OS actually reports for 8 GB and 4 GB laptops.
    #[test]
    fn small_total_ram_drops_a_tier() {
        assert_eq!(recommend_models(&hw_ram(8, 7.7, 5.0)).llm, LLM_FLOOR.name);
        assert_eq!(recommend_models(&hw_ram(8, 3.7, 2.5)).llm, LLM_MINIMAL.name);
    }

    /// The author's own laptop: i7-4600U, 2 physical cores, ~15 GiB. Cores, not
    /// RAM, are what keep it on the smallest model.
    #[test]
    fn two_cores_stay_minimal_however_much_ram() {
        assert_eq!(recommend_models(&hw_ram(2, 15.4, 10.0)).llm, LLM_MINIMAL.name);
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

    /// A model bigger than the machine's total RAM can never run: blocked, with
    /// the real numbers in the message.
    #[test]
    fn a_model_that_cannot_fit_is_blocked() {
        let a = assess_llm(LLM_LARGE.name, &hw_ram(4, 6.0, 4.0), &[]);
        assert_eq!(a.severity, "blocked", "{:?}", a.warnings);
        assert!(
            a.warnings.iter().any(|w| w.contains("6.0 GB in total")),
            "the warning should quote the real numbers: {:?}",
            a.warnings
        );
    }

    /// Fits the machine, but not beside what is open right now: a caution telling
    /// the user to close apps — not a block, because closing apps fixes it.
    #[test]
    fn fits_in_total_but_not_now_is_a_caution() {
        let a = assess_llm(LLM_FLOOR.name, &hw_ram(4, 7.7, 2.0), &[]);
        assert_eq!(a.severity, "caution", "{:?}", a.warnings);
        assert!(
            a.warnings.iter().any(|w| w.contains("available right now")),
            "{:?}",
            a.warnings
        );
    }

    /// Fits in RAM, but far bigger than this host should run: caution, not silence.
    #[test]
    fn an_oversized_but_fitting_model_warns() {
        let a = assess_llm(LLM_STANDARD.name, &hw(4, 12.0, None, false), &[]);
        assert_eq!(a.severity, "caution", "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("slower")));
    }

    /// An unknown tag cannot be sized, and thinking models are the trap worth naming.
    #[test]
    fn an_unknown_model_is_flagged() {
        let a = assess_llm("qwen3:4b", &hw(8, 16.0, None, false), &[]);
        assert_eq!(a.severity, "caution");
        assert!(a.warnings.iter().any(|w| w.contains("thinking")));
    }

    /// Models already pulled cost nothing: the screen must not claim a download
    /// that the pull loop then skips.
    #[test]
    fn installed_models_are_not_counted_as_downloads() {
        let installed = vec![
            LLM_FLOOR.name.to_string(),
            "moondream:latest".to_string(),
            "nomic-embed-text:latest".to_string(),
        ];
        let a = assess_llm(LLM_FLOOR.name, &hw(4, 12.0, None, false), &installed);
        assert!(a.missing_models.is_empty(), "{:?}", a.missing_models);
        assert!(a.approx_download_gb.abs() < 0.01, "got {}", a.approx_download_gb);

        let partial = vec!["moondream:latest".to_string()];
        let b = assess_llm(LLM_FLOOR.name, &hw(4, 12.0, None, false), &partial);
        assert_eq!(b.missing_models, vec![LLM_FLOOR.name, "nomic-embed-text"]);
    }

    #[test]
    fn untagged_names_match_latest() {
        let installed = vec!["phi4-mini:latest".to_string()];
        assert!(model_present(&installed, "phi4-mini"));
        assert!(!model_present(&installed, "phi4"));
    }

    /// The download figure must cover the specialists too, or the disk check lies.
    #[test]
    fn download_size_includes_the_specialists() {
        let a = assess_llm(LLM_FLOOR.name, &hw(4, 12.0, None, false), &[]);
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
