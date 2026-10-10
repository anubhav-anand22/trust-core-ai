//! Model recommendation + on-demand model pulls.
//!
//! The blueprint wants the app to "detect system specs … and based on that install
//! a compatible model". [`recommend`] does the picking; [`ensure_models`] does the
//! pulling.
//!
//! The recommender is deliberately **not** an LLM — it runs before any model is
//! installed. It is a scored search over a catalogue:
//!
//! 1. **Budget** — how much memory a model may use on this host's backend
//!    (GPU VRAM, Apple unified memory, or system RAM minus the OS's share).
//! 2. **Filter** — only models verified to drive this app's structured-JSON
//!    pipeline (see `examples/model_check.rs`), and only ones that fit.
//! 3. **Speed** — an estimate of decode tokens/s on this backend and core count.
//! 4. **Pick** — the best fit for the user's profile among models fast enough for
//!    the speed they asked for. Ties go to the smaller (and an installed model wins
//!    a near-tie, so nobody downloads 2 GB for a two-point gain).
//!
//! Every step leaves a human-readable reason, shown on the bootstrap screen.

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{Manager, State};
use tokio_stream::StreamExt;

use super::hardware::{detect_hardware, Backend, HardwareInfo};
use super::profile::{self, ProfileNeeds};
use crate::AppState;

/// The three models one session runs on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPlan {
    /// Resident text model — roles A/B/C, kept alive indefinitely.
    pub llm: String,
    /// Vision model. Empty when the profile expects no photos (nothing is pulled);
    /// equal to `llm` when the resident model is multimodal and handles images.
    pub vision: String,
    /// Embedding model for RAG, called with `keep_alive = 0`.
    pub embed: String,
    /// Human-readable label for the bootstrap screen.
    pub tier_label: String,
    /// Context window the resident model should run with.
    #[serde(default = "default_num_ctx")]
    pub num_ctx: u64,
}

fn default_num_ctx() -> u64 {
    8192
}

impl ModelPlan {
    /// Conservative default used before the user has confirmed a plan.
    ///
    /// Deliberately the *floor* model: this is what runs if the confirmed plan was
    /// lost, and a too-small model gives a slow-but-working answer where a
    /// too-large one can swap the machine to a halt.
    pub fn fallback() -> Self {
        Self {
            llm: LLM_FLOOR.name.into(),
            vision: VISION.name.into(),
            embed: EMBED.name.into(),
            tier_label: "default (floor model)".into(),
            num_ctx: LLM_FLOOR.num_ctx,
        }
    }
}

/// A resident-LLM option: what it costs, what it is good at, whether it is
/// proven to work in this app.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct LlmSpec {
    /// Ollama tag.
    pub name: &'static str,
    /// Approximate resident footprint in GB: q4 weights plus KV-cache headroom.
    pub ram_gb: f32,
    /// Download size in GB (Ollama registry, 2026-10-10).
    pub download_gb: f32,
    /// Largest context window worth running it with.
    pub num_ctx: u64,
    /// Parameters active per token, in billions. Decode speed on a CPU scales
    /// with this, not with the file size.
    pub active_params_b: f32,
    /// Reads images itself, so no separate vision model has to be loaded.
    pub multimodal: bool,
    /// Passed all of the app's structured-output calls (`model_check`) or has a
    /// production record in this app. **Unverified models are listed but never
    /// recommended** — a model that can't produce the plan JSON can't run the
    /// pipeline, however strong it is elsewhere.
    pub verified: bool,
    /// General capability, 0–100. Editorial: size and generation based, to be
    /// refined from `model_check` runs.
    pub quality: u8,
    /// Strength bonuses, 0–10, weighted by the user's profile.
    pub docs: u8,
    pub reasoning: u8,
    pub code: u8,
    pub multilingual: u8,
}

const fn llm(
    name: &'static str,
    ram_gb: f32,
    download_gb: f32,
    num_ctx: u64,
    active_params_b: f32,
    multimodal: bool,
    verified: bool,
    quality: u8,
    [docs, reasoning, code, multilingual]: [u8; 4],
) -> LlmSpec {
    LlmSpec {
        name,
        ram_gb,
        download_gb,
        num_ctx,
        active_params_b,
        multimodal,
        verified,
        quality,
        docs,
        reasoning,
        code,
        multilingual,
    }
}

// Every verified entry is an *instruct* model, because the pipeline constrains
// decoding to a JSON schema (`FormatType::StructuredJson`). "Thinking" models
// (e.g. `qwen3:4b`) emit reasoning first and return an empty body under that mode
// (`EOF while parsing a value`) — so a new model stays `verified: false` until
// `model_check` has actually run it, whatever its reputation.
pub const LLM_MINIMAL: LlmSpec = llm("qwen2.5:0.5b-instruct", 0.9, 0.40, 4096, 0.5, false, true, 20, [3, 2, 2, 6]);
pub const LLM_FLOOR: LlmSpec = llm("qwen2.5:1.5b-instruct", 1.8, 0.99, 8192, 1.5, false, true, 38, [4, 3, 3, 7]);
pub const LLM_STANDARD: LlmSpec = llm("qwen2.5:3b-instruct", 2.9, 1.93, 8192, 3.1, false, true, 52, [5, 4, 4, 7]);
pub const LLM_LARGE: LlmSpec = llm("qwen2.5:7b-instruct", 5.6, 4.68, 8192, 7.6, false, true, 70, [6, 6, 6, 8]);

/// Everything the bootstrap dropdown may offer.
pub const LLM_CATALOG: &[LlmSpec] = &[
    LLM_MINIMAL,
    llm("qwen3.5:0.8b", 1.7, 1.32, 8192, 0.8, true, false, 34, [4, 4, 3, 7]),
    LLM_FLOOR,
    llm("qwen2.5-coder:1.5b-instruct", 1.8, 0.99, 8192, 1.5, false, false, 34, [2, 3, 10, 5]),
    llm("granite4.2:3b", 2.9, 2.24, 8192, 3.0, false, false, 54, [8, 5, 4, 3]),
    LLM_STANDARD,
    llm("qwen2.5-coder:3b-instruct", 2.9, 1.93, 8192, 3.1, false, false, 48, [2, 4, 10, 5]),
    llm("llama3.2:3b", 3.0, 2.02, 8192, 3.2, false, true, 50, [4, 4, 3, 3]),
    llm("qwen3.5:2b", 3.2, 2.68, 8192, 2.0, true, false, 50, [5, 5, 4, 7]),
    llm("phi4-mini", 3.8, 2.49, 8192, 3.8, false, false, 58, [4, 8, 5, 3]),
    llm("qwen3.5:4b", 4.2, 3.32, 8192, 4.0, true, false, 64, [6, 7, 5, 8]),
    LLM_LARGE,
    llm("granite4.2:8b", 6.3, 5.35, 8192, 8.0, false, false, 70, [9, 6, 5, 3]),
    llm("gemma4:e4b", 7.5, 6.58, 8192, 4.0, true, false, 66, [5, 6, 4, 7]),
];

/// Separate vision models, used only when the resident model can't read images.
/// `minicpm-v4.6:1b` is the lighter candidate; it takes over once verified.
const VISION: LlmSpec = llm("moondream", 1.7, 1.74, 2048, 1.9, true, true, 0, [0, 0, 0, 0]);
const VISION_CANDIDATES: &[LlmSpec] = &[
    llm("minicpm-v4.6:1b", 1.6, 1.64, 2048, 1.0, true, false, 0, [0, 0, 0, 0]),
    VISION,
];
const EMBED: LlmSpec = llm("nomic-embed-text", 0.5, 0.27, 2048, 0.1, false, true, 0, [0, 0, 0, 0]);

// --- speed model ---------------------------------------------------------
//
// Decode on a CPU is bounded twice: by compute (threads × SIMD ÷ active params)
// and by memory bandwidth (every token streams the whole weight file through
// RAM). On a GPU it is bandwidth-bound. The constants are placeholders until the
// `model_check` runs on the 2-core laptop and the GPU laptop calibrate them;
// what matters today is that they rank machines and models in the right order.

/// Decode tok/s per CPU thread per 1B active parameters, with AVX2.
const CPU_K: f32 = 4.0;
/// Effective memory bandwidth of a laptop's dual-channel RAM, GB/s.
const CPU_BANDWIDTH_GBPS: f32 = 18.0;

fn gpu_bandwidth_gbps(backend: Backend) -> f32 {
    match backend {
        Backend::Cuda | Backend::Rocm => 200.0,
        // Base M-series chips; Pro/Max are faster. Conservative on purpose.
        Backend::Metal => 60.0,
        Backend::Cpu => 0.0,
    }
}

/// Where a model would run on this host, and how fast.
#[derive(Clone, Copy, Debug)]
struct Placement {
    on_gpu: bool,
    tok_s: f32,
}

fn place(m: &LlmSpec, hw: &HardwareInfo) -> Option<Placement> {
    if hw.has_usable_gpu() && m.ram_gb <= hw.gpu_budget_gb() {
        return Some(Placement {
            on_gpu: true,
            tok_s: gpu_bandwidth_gbps(hw.backend) / m.download_gb,
        });
    }
    if m.ram_gb <= hw.cpu_budget_gb() {
        let simd = if !hw.avx2 {
            0.35
        } else if hw.avx512 {
            1.15
        } else {
            1.0
        };
        let compute = CPU_K * hw.worker_threads() as f32 * simd / m.active_params_b;
        let bandwidth = CPU_BANDWIDTH_GBPS / m.download_gb;
        return Some(Placement {
            on_gpu: false,
            tok_s: compute.min(bandwidth),
        });
    }
    None
}

/// Profile-weighted capability. A multimodal model gets a bonus when photos are
/// expected: it spares loading (and paging in and out) a second vision model.
fn fit_score(m: &LlmSpec, needs: &ProfileNeeds) -> f32 {
    m.quality as f32
        + m.docs as f32 * needs.docs
        + m.reasoning as f32 * needs.reasoning
        + m.code as f32 * needs.code * 1.5
        + m.multilingual as f32 * needs.multilingual
        + if needs.needs_vision && m.multimodal { 5.0 } else { 0.0 }
}

/// Catalogue entry for a model name, if we know it.
pub fn lookup_llm(name: &str) -> Option<LlmSpec> {
    LLM_CATALOG
        .iter()
        .chain(VISION_CANDIDATES)
        .chain(std::iter::once(&EMBED))
        .copied()
        .find(|m| m.name == name)
}

/// The output of [`recommend`].
#[derive(Clone, Debug, Serialize)]
pub struct Recommendation {
    pub plan: ModelPlan,
    /// Why this pick, one plain-language line each. Shown on the bootstrap screen.
    pub reasons: Vec<String>,
    /// The next smaller model that fits — snappier, less capable.
    pub faster: Option<String>,
    /// The next more capable model that fits — slower than the speed target.
    pub smarter: Option<String>,
}

/// Choose the resident model, the vision path and the context size for this host
/// and this user.
pub fn recommend(hw: &HardwareInfo, needs: &ProfileNeeds, installed: &[String]) -> Recommendation {
    let mut reasons = vec![backend_reason(hw)];
    if !hw.avx2 {
        reasons.push(
            "This CPU has no AVX2, so model maths runs several times slower per core — \
             a smaller model is chosen to compensate."
                .into(),
        );
    }

    // Verified + fits, smallest first.
    let mut fits: Vec<(LlmSpec, Placement)> = LLM_CATALOG
        .iter()
        .filter(|m| m.verified)
        .filter_map(|m| place(m, hw).map(|p| (*m, p)))
        .collect();
    fits.sort_by(|a, b| a.0.ram_gb.total_cmp(&b.0.ram_gb));

    let below_minimum = hw.physical_cores < 2 || fits.is_empty();
    let (pick, placement) = if fits.is_empty() {
        (LLM_MINIMAL, Placement { on_gpu: false, tok_s: 0.0 })
    } else {
        let fast_enough: Vec<&(LlmSpec, Placement)> =
            fits.iter().filter(|(_, p)| p.tok_s >= needs.min_tok_s).collect();
        match fast_enough
            .iter()
            .max_by(|a, b| fit_score(&a.0, needs).total_cmp(&fit_score(&b.0, needs)))
        {
            Some(best) => {
                let best_score = fit_score(&best.0, needs);
                // A near-tie that is already on disk beats a download.
                let installed_alt = fast_enough.iter().find(|(m, _)| {
                    m.name != best.0.name
                        && model_present(installed, m.name)
                        && fit_score(m, needs) >= best_score - 4.0
                });
                match installed_alt {
                    Some((m, p)) if !model_present(installed, best.0.name) => {
                        reasons.push(format!(
                            "\"{}\" is already installed and nearly as good, so nothing \
                             large has to download.",
                            m.name
                        ));
                        (*m, *p)
                    }
                    _ => (best.0, best.1),
                }
            }
            None => {
                reasons.push(format!(
                    "Even the smallest model runs below your speed target here \
                     (~{:.0} tokens/s), so the fastest one is chosen.",
                    fits[0].1.tok_s
                ));
                fits[0]
            }
        }
    };

    if !below_minimum {
        reasons.push(format!(
            "\"{}\" ≈ {:.0} tokens/s here ({}), fits in {:.1} GB.",
            pick.name,
            placement.tok_s,
            if placement.on_gpu { "on the GPU" } else { "on the CPU" },
            pick.ram_gb
        ));
        reasons.push(priority_reason(needs));
    }

    let idx = fits.iter().position(|(m, _)| m.name == pick.name);
    let pick_score = fit_score(&pick, needs);
    let faster = idx
        .and_then(|i| fits[..i].iter().rev().find(|(m, _)| m.ram_gb < pick.ram_gb))
        .map(|(m, _)| m.name.to_string());
    let smarter = fits
        .iter()
        .filter(|(m, _)| m.ram_gb > pick.ram_gb && fit_score(m, needs) > pick_score)
        .min_by(|a, b| a.0.ram_gb.total_cmp(&b.0.ram_gb))
        .map(|(m, _)| m.name.to_string());

    let vision = vision_for(&pick, needs);
    if !needs.needs_vision {
        reasons.push("No photos in your profile, so no vision model is downloaded.".into());
    } else if vision == pick.name {
        reasons.push(format!("\"{}\" reads images itself — no separate vision model.", pick.name));
    }

    // Bigger context only if the budget has room for the extra KV cache.
    let budget = if placement.on_gpu { hw.gpu_budget_gb() } else { hw.cpu_budget_gb() };
    let num_ctx = if budget - pick.ram_gb >= 0.5 {
        needs.prefer_ctx.min(pick.num_ctx)
    } else {
        4096
    };

    let tier_label = if below_minimum {
        "below minimum — this machine cannot run the workbench well".to_string()
    } else {
        format!(
            "{} · {} · ~{:.0} tok/s",
            backend_short(hw, placement.on_gpu),
            pick.name,
            placement.tok_s
        )
    };

    Recommendation {
        plan: ModelPlan {
            llm: pick.name.into(),
            vision,
            embed: EMBED.name.into(),
            tier_label,
            num_ctx,
        },
        reasons,
        faster,
        smarter,
    }
}

/// The vision path for a resident model under a profile.
fn vision_for(resident: &LlmSpec, needs: &ProfileNeeds) -> String {
    if !needs.needs_vision {
        String::new()
    } else if resident.multimodal && resident.verified {
        resident.name.into()
    } else {
        VISION_CANDIDATES
            .iter()
            .find(|v| v.verified)
            .unwrap_or(&VISION)
            .name
            .into()
    }
}

fn backend_reason(hw: &HardwareInfo) -> String {
    match hw.backend {
        Backend::Cuda | Backend::Rocm if hw.has_usable_gpu() => format!(
            "{} with {:.1} GB VRAM → models run on the GPU ({:.1} GB usable).",
            hw.gpu_name,
            hw.vram_gb.unwrap_or(0.0),
            hw.gpu_budget_gb()
        ),
        Backend::Metal => format!(
            "{} → Metal GPU on unified memory ({:.1} GB usable for a model).",
            hw.gpu_name,
            hw.gpu_budget_gb()
        ),
        _ => format!(
            "{} physical core(s), no usable GPU → CPU inference on {} thread(s), \
             {:.1} GB of {:.1} GB RAM usable for a model ({}).",
            hw.physical_cores,
            hw.worker_threads(),
            hw.cpu_budget_gb(),
            hw.total_ram_gb,
            hw.os
        ),
    }
}

fn backend_short(hw: &HardwareInfo, on_gpu: bool) -> String {
    if !on_gpu {
        return format!("CPU · {} thread(s)", hw.worker_threads());
    }
    match hw.backend {
        Backend::Cuda => "NVIDIA GPU".into(),
        Backend::Rocm => "AMD GPU".into(),
        Backend::Metal => "Apple GPU".into(),
        Backend::Cpu => "CPU".into(),
    }
}

fn priority_reason(needs: &ProfileNeeds) -> String {
    let p = if needs.min_tok_s >= 10.0 {
        "Fastest answers"
    } else if needs.min_tok_s <= 3.0 {
        "Best quality"
    } else {
        "Balanced"
    };
    format!(
        "Profile: {p} (target ≥ {:.0} tokens/s), {} context.",
        needs.min_tok_s,
        if needs.prefer_ctx >= 8192 { "document-sized" } else { "short" }
    )
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

/// How risky is running `model` on this host? Drives the bootstrap warning.
#[derive(Clone, Debug, Serialize)]
pub struct LlmAssessment {
    pub model: String,
    pub approx_ram_gb: f32,
    /// Download still needed for the resident model plus its specialists —
    /// models already installed count as zero.
    pub approx_download_gb: f32,
    /// Which of (resident, vision, embed) are not installed yet. Empty means
    /// "Start" needs no download at all.
    pub missing_models: Vec<String>,
    /// The vision model that goes with this choice ("" = none, or the model
    /// itself when it is multimodal). The screen builds its plan from this.
    pub vision: String,
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
pub fn assess_llm(
    model: &str,
    hw: &HardwareInfo,
    installed: &[String],
    needs: &ProfileNeeds,
) -> LlmAssessment {
    let recommended = recommend(hw, needs, installed).plan;
    let spec = lookup_llm(model);
    let mut warnings = Vec::new();
    let mut severity = "ok";

    let ram_gb = match spec {
        Some(m) => {
            if !m.verified {
                severity = "caution";
                warnings.push(format!(
                    "\"{model}\" hasn't been verified with this app's structured output yet \
                     (it may plan badly or return empty answers). Run `model_check` on it, \
                     or pick a verified model."
                ));
            }
            m.ram_gb
        }
        None => {
            severity = "caution";
            warnings.push(format!(
                "\"{model}\" is not in the catalogue, so its memory cost is unknown. Models \
                 that emit reasoning before their answer (any \"thinking\" model) return an \
                 empty result under this app's structured-output mode."
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
        let needed = ram_gb + hw.os_headroom_gb();
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

    let vision = match spec {
        Some(m) => vision_for(&m, needs),
        None if needs.needs_vision => VISION.name.into(),
        None => String::new(),
    };
    let missing_models: Vec<String> = [model, vision.as_str(), EMBED.name]
        .into_iter()
        .filter(|m| !m.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|m| !model_present(installed, m))
        .map(str::to_string)
        .collect();

    LlmAssessment {
        model: model.to_string(),
        approx_ram_gb: ram_gb,
        approx_download_gb: download_estimate_gb(&missing_models),
        missing_models,
        vision,
        severity: severity.into(),
        warnings,
    }
}

/// One dropdown entry.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    pub name: String,
    pub verified: bool,
    pub multimodal: bool,
    pub download_gb: f32,
}

/// Hardware + recommendation in one call, for the bootstrap screen.
#[derive(Clone, Debug, Serialize)]
pub struct SystemProbe {
    pub hardware: HardwareInfo,
    pub recommended: ModelPlan,
    /// The recommendation judged against this host. The field is here so the
    /// bootstrap screen gets the download size without a second call.
    pub assessment: LlmAssessment,
    /// Why this model, one line each.
    pub reasons: Vec<String>,
    pub faster: Option<String>,
    pub smarter: Option<String>,
    /// The dropdown's options — from here, so the screen and the recommender can
    /// never disagree about what exists.
    pub catalog: Vec<CatalogEntry>,
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

fn profile_needs(app: &tauri::AppHandle) -> ProfileNeeds {
    profile::load(app).unwrap_or_default().needs()
}

#[tauri::command]
pub async fn probe_system(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<SystemProbe, String> {
    let hardware = detect_hardware();
    let needs = profile_needs(&app);
    let installed = installed_models(&state).await;
    let rec = recommend(&hardware, &needs, &installed);
    let assessment = assess_llm(&rec.plan.llm, &hardware, &installed, &needs);
    tracing::info!(
        recommended = %rec.plan.llm,
        vision = %rec.plan.vision,
        num_ctx = rec.plan.num_ctx,
        tier = %rec.plan.tier_label,
        faster = ?rec.faster,
        smarter = ?rec.smarter,
        installed = ?installed,
        approx_download_gb = assessment.approx_download_gb,
        "system probe -> recommendation"
    );
    Ok(SystemProbe {
        hardware,
        recommended: rec.plan,
        assessment,
        reasons: rec.reasons,
        faster: rec.faster,
        smarter: rec.smarter,
        catalog: LLM_CATALOG
            .iter()
            .map(|m| CatalogEntry {
                name: m.name.into(),
                verified: m.verified,
                multimodal: m.multimodal,
                download_gb: m.download_gb,
            })
            .collect(),
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
pub async fn assess_model(
    model: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<LlmAssessment, String> {
    let hw = detect_hardware();
    let installed = installed_models(&state).await;
    let assessment = assess_llm(&model, &hw, &installed, &profile_needs(&app));
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
/// error. Empty names (no vision model in this plan) and duplicates (a
/// multimodal resident listed as its own vision model) are skipped.
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
    let mut seen = std::collections::HashSet::new();
    let models: Vec<String> = models
        .into_iter()
        .filter(|m| !m.trim().is_empty() && seen.insert(m.clone()))
        .collect();

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
    use super::super::profile::UserProfile;
    use super::*;

    fn machine(
        physical: usize,
        total: f32,
        available: f32,
        backend: Backend,
        vram: Option<f32>,
        os: &str,
        avx2: bool,
    ) -> HardwareInfo {
        HardwareInfo {
            total_ram_gb: total,
            available_ram_gb: available,
            cpu_cores: physical * 2,
            physical_cores: physical,
            gpu_vendor: "test".into(),
            gpu_name: "test gpu".into(),
            vram_gb: vram,
            cuda_available: backend == Backend::Cuda,
            backend,
            os: os.into(),
            arch: "x86_64".into(),
            avx2,
            avx512: false,
        }
    }

    fn cpu(physical: usize, total: f32) -> HardwareInfo {
        machine(physical, total, total * 0.6, Backend::Cpu, None, "linux", true)
    }

    fn balanced() -> ProfileNeeds {
        UserProfile::default().needs()
    }

    fn with(priority: &str) -> ProfileNeeds {
        UserProfile {
            priority: priority.into(),
            ..Default::default()
        }
        .needs()
    }

    fn pick(hw: &HardwareInfo, needs: &ProfileNeeds) -> String {
        recommend(hw, needs, &[]).plan.llm
    }

    // --- the machines the plan names -------------------------------------

    /// The author's laptop: i7-4600U, 2 physical cores (1 inference thread),
    /// ~15 GB. RAM is plentiful; cores keep it on the smallest model.
    #[test]
    fn two_core_laptop_stays_on_the_smallest_model() {
        let hw = cpu(2, 15.4);
        assert_eq!(pick(&hw, &balanced()), LLM_MINIMAL.name);
        assert_eq!(pick(&hw, &with("quality")), LLM_MINIMAL.name, "1 thread can't carry 1.5B at 3 tok/s");
    }

    /// The declared floor: 8 GB, 4 cores, no GPU.
    #[test]
    fn eight_gb_four_core_gets_the_floor_model_and_no_warning() {
        let hw = cpu(4, 7.7);
        let needs = balanced();
        assert_eq!(pick(&hw, &needs), LLM_FLOOR.name);
        assert_eq!(assess_llm(LLM_FLOOR.name, &hw, &[], &needs).severity, "ok");
    }

    /// Same machine, different user: "fastest" goes smaller, "best quality" trades
    /// speed for a 3B.
    #[test]
    fn profile_changes_the_pick_on_the_same_machine() {
        let hw = cpu(4, 7.7);
        assert_eq!(pick(&hw, &with("fast")), LLM_MINIMAL.name);
        assert_eq!(pick(&hw, &balanced()), LLM_FLOOR.name);
        assert_eq!(pick(&hw, &with("quality")), LLM_STANDARD.name);
    }

    #[test]
    fn many_cores_can_afford_the_mid_model() {
        assert_eq!(pick(&cpu(8, 15.5), &balanced()), LLM_STANDARD.name);
    }

    /// A 16 GB M1 used to be read as CPU-only and handed the 0.5B model.
    #[test]
    fn apple_silicon_gets_a_real_model() {
        let mac = machine(8, 16.0, 9.0, Backend::Metal, None, "macos", true);
        assert_eq!(pick(&mac, &balanced()), LLM_LARGE.name);
    }

    #[test]
    fn an_rtx_4060_gets_the_7b() {
        let gpu = machine(8, 16.0, 9.0, Backend::Cuda, Some(8.0), "windows", true);
        assert_eq!(pick(&gpu, &balanced()), LLM_LARGE.name);
    }

    #[test]
    fn an_amd_8gb_card_on_linux_is_a_gpu_too() {
        let amd = machine(6, 16.0, 9.0, Backend::Rocm, Some(8.0), "linux", true);
        assert_eq!(pick(&amd, &balanced()), LLM_LARGE.name);
    }

    /// A small card that can't hold the model falls back to CPU placement.
    #[test]
    fn a_tiny_gpu_falls_back_to_cpu() {
        let hw = machine(4, 7.7, 5.0, Backend::Cuda, Some(2.0), "windows", true);
        assert!(!hw.has_usable_gpu());
        assert_eq!(pick(&hw, &balanced()), LLM_FLOOR.name);
    }

    /// No AVX2 (an old Celeron): slower per core, and the machine says so.
    #[test]
    fn no_avx2_drops_a_tier_and_says_why() {
        let celeron = machine(4, 7.7, 5.0, Backend::Cpu, None, "windows", false);
        let rec = recommend(&celeron, &balanced(), &[]);
        assert_eq!(rec.plan.llm, LLM_MINIMAL.name);
        assert!(rec.reasons.iter().any(|r| r.contains("AVX2")), "{:?}", rec.reasons);
    }

    #[test]
    fn a_one_core_machine_says_it_is_below_minimum() {
        let rec = recommend(&cpu(1, 6.0), &balanced(), &[]);
        assert!(rec.plan.tier_label.contains("below minimum"), "{}", rec.plan.tier_label);
    }

    /// The tier is a property of the machine: opening a browser (less available
    /// RAM) must not change the pick between launches.
    #[test]
    fn available_ram_does_not_move_the_pick() {
        let idle = machine(8, 15.5, 12.0, Backend::Cpu, None, "linux", true);
        let busy = machine(8, 15.5, 3.0, Backend::Cpu, None, "linux", true);
        assert_eq!(pick(&idle, &balanced()), pick(&busy, &balanced()));
    }

    /// Windows idles heavier: the same 4 GB laptop has less room on Windows.
    #[test]
    fn small_total_ram_limits_the_model_by_os() {
        let linux = machine(4, 3.7, 2.5, Backend::Cpu, None, "linux", true);
        let windows = machine(4, 3.7, 2.5, Backend::Cpu, None, "windows", true);
        assert_eq!(pick(&linux, &balanced()), LLM_FLOOR.name, "1.8 GB fits in 3.7 − 1.5");
        assert_eq!(pick(&windows, &balanced()), LLM_MINIMAL.name, "only 0.7 GB left after Windows");
    }

    #[test]
    fn hyperthreads_do_not_promote_a_tier() {
        let host = cpu(4, 16.0);
        assert_eq!(host.cpu_cores, 8, "fixture should look hyperthreaded");
        assert_eq!(pick(&host, &balanced()), LLM_FLOOR.name);
        assert_eq!(host.worker_threads(), 3);
    }

    // --- safety rules -------------------------------------------------------

    /// Unverified models are listed but never recommended, on any machine.
    #[test]
    fn never_recommends_an_unverified_model() {
        for hw in [
            cpu(2, 15.4),
            cpu(8, 32.0),
            machine(16, 64.0, 40.0, Backend::Cuda, Some(24.0), "linux", true),
            machine(10, 32.0, 20.0, Backend::Metal, None, "macos", true),
        ] {
            for p in ["fast", "balanced", "quality"] {
                let name = pick(&hw, &with(p));
                assert!(lookup_llm(&name).unwrap().verified, "{name} on {:?}/{p}", hw.backend);
            }
        }
    }

    #[test]
    fn the_pick_always_fits_the_budget() {
        for hw in [cpu(2, 3.7), cpu(4, 7.7), cpu(8, 15.5)] {
            let spec = lookup_llm(&pick(&hw, &with("quality"))).unwrap();
            assert!(spec.ram_gb <= hw.cpu_budget_gb(), "{} on {} GB", spec.name, hw.total_ram_gb);
        }
    }

    #[test]
    fn alternatives_bracket_the_pick() {
        let rec = recommend(&cpu(4, 7.7), &balanced(), &[]);
        assert_eq!(rec.faster.as_deref(), Some(LLM_MINIMAL.name));
        assert!(rec.smarter.is_some(), "a 3B fits a 7.7 GB box");
    }

    /// A near-tie already on disk beats downloading the slightly better one.
    #[test]
    fn an_installed_near_tie_wins() {
        let hw = cpu(8, 15.5);
        let installed = vec!["llama3.2:3b".to_string()];
        let rec = recommend(&hw, &balanced(), &installed);
        assert_eq!(rec.plan.llm, "llama3.2:3b", "{:?}", rec.reasons);
    }

    // --- vision ------------------------------------------------------------

    #[test]
    fn no_photos_means_no_vision_model() {
        let needs = UserProfile {
            inputs: vec!["documents".into()],
            ..Default::default()
        }
        .needs();
        let rec = recommend(&cpu(4, 7.7), &needs, &[]);
        assert_eq!(rec.plan.vision, "");
        let a = assess_llm(&rec.plan.llm, &cpu(4, 7.7), &[], &needs);
        assert!(!a.missing_models.iter().any(|m| m.contains("moondream")), "{:?}", a.missing_models);
    }

    #[test]
    fn photos_keep_a_verified_vision_model() {
        let rec = recommend(&cpu(4, 7.7), &balanced(), &[]);
        assert_eq!(rec.plan.vision, VISION.name);
    }

    // --- assessment ------------------------------------------------------------

    #[test]
    fn a_model_that_cannot_fit_is_blocked() {
        let a = assess_llm(LLM_LARGE.name, &cpu(4, 6.0), &[], &balanced());
        assert_eq!(a.severity, "blocked", "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("6.0 GB in total")), "{:?}", a.warnings);
    }

    #[test]
    fn fits_in_total_but_not_now_is_a_caution() {
        let hw = machine(4, 7.7, 2.0, Backend::Cpu, None, "linux", true);
        let a = assess_llm(LLM_FLOOR.name, &hw, &[], &balanced());
        assert_eq!(a.severity, "caution", "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("available right now")));
    }

    #[test]
    fn an_oversized_but_fitting_model_warns() {
        let a = assess_llm(LLM_STANDARD.name, &cpu(4, 16.0), &[], &balanced());
        assert_eq!(a.severity, "caution", "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("slower")));
    }

    #[test]
    fn an_unverified_catalogue_model_is_a_caution() {
        let a = assess_llm("qwen3.5:2b", &cpu(8, 16.0), &[], &balanced());
        assert_eq!(a.severity, "caution");
        assert!(a.warnings.iter().any(|w| w.contains("verified")), "{:?}", a.warnings);
    }

    #[test]
    fn an_unknown_model_is_flagged() {
        let a = assess_llm("qwen3:4b", &cpu(8, 16.0), &[], &balanced());
        assert_eq!(a.severity, "caution");
        assert!(a.warnings.iter().any(|w| w.contains("thinking")));
    }

    #[test]
    fn installed_models_are_not_counted_as_downloads() {
        let installed = vec![
            LLM_FLOOR.name.to_string(),
            "moondream:latest".to_string(),
            "nomic-embed-text:latest".to_string(),
        ];
        let a = assess_llm(LLM_FLOOR.name, &cpu(4, 16.0), &installed, &balanced());
        assert!(a.missing_models.is_empty(), "{:?}", a.missing_models);
        assert!(a.approx_download_gb.abs() < 0.01);

        let partial = vec!["moondream:latest".to_string()];
        let b = assess_llm(LLM_FLOOR.name, &cpu(4, 16.0), &partial, &balanced());
        assert_eq!(b.missing_models.len(), 2, "{:?}", b.missing_models);
    }

    #[test]
    fn untagged_names_match_latest() {
        let installed = vec!["phi4-mini:latest".to_string()];
        assert!(model_present(&installed, "phi4-mini"));
        assert!(!model_present(&installed, "phi4"));
    }

    #[test]
    fn download_size_includes_the_specialists() {
        let a = assess_llm(LLM_FLOOR.name, &cpu(4, 16.0), &[], &balanced());
        let expect = LLM_FLOOR.download_gb + VISION.download_gb + EMBED.download_gb;
        assert!((a.approx_download_gb - expect).abs() < 0.01, "got {}", a.approx_download_gb);
    }

    #[test]
    fn every_catalogue_entry_is_self_consistent() {
        for m in LLM_CATALOG {
            assert!(m.ram_gb > 0.0 && m.download_gb > 0.0, "{} has no sizes", m.name);
            assert!(m.active_params_b > 0.0, "{} has no param count", m.name);
            assert!(m.ram_gb >= m.download_gb, "{}: resident cost should exceed download", m.name);
            assert!(lookup_llm(m.name).is_some(), "{} not findable", m.name);
        }
    }
}
