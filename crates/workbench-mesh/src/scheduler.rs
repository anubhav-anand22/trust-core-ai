//! Which device should run a step, and who should plan. Pure functions over
//! what each device reported at join, so every rule here is a plain unit test.
//!
//! The executor (`workbench_core::mesh::execute_plan_mesh`) takes the ranked list
//! and handles the rest: it skips busy devices, retries on failure, and falls
//! back to the Host.

use serde::{Deserialize, Serialize};
use workbench_core::mesh::Node;

/// What a device can do, sent in its `Hello` (see [`crate::frame::Hello`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub physical_cores: usize,
    pub ram_gb: f32,
    /// `HardwareInfo::has_usable_gpu`: a GPU Ollama can offload to with ≥3.5 GB.
    pub gpu: bool,
    #[serde(default)]
    pub gpu_name: String,
    /// Installed and selected models. `None` = that role can't run here.
    #[serde(default)]
    pub llm_model: Option<String>,
    #[serde(default)]
    pub vision_model: Option<String>,
    #[serde(default)]
    pub embed_model: Option<String>,
    #[serde(default)]
    pub whisper: bool,
    #[serde(default)]
    pub ocr: bool,
    /// Measured decode speed of `llm_model`, from a short warm-up generate.
    #[serde(default)]
    pub decode_tok_s: Option<f64>,
}

/// What a step needs from the device that runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    /// Never leaves the Host: the knowledge base lives there (MESH-14 exception).
    HostOnly,
    Pdf,
    Ocr,
    Whisper,
    Vision,
    /// A prose model call only (`answer_followup`, `answer_directly`).
    Llm,
    /// The analysis tools also rank evidence with the embedding model
    /// (`rag::select_relevant`), so the Worker needs the Host's embed model too,
    /// or the ranking, and so the report, differs.
    Analysis,
}

/// Unknown tasks stay on the Host: a new tool might read the knowledge base.
pub fn needs(task: &str) -> Needs {
    match task {
        "parse_pdf" => Needs::Pdf,
        "ocr_image" => Needs::Ocr,
        "transcribe_audio" => Needs::Whisper,
        "analyze_image" => Needs::Vision,
        "answer_followup" | "answer_directly" => Needs::Llm,
        "summarize" | "compare_to_sop" | "assess" => Needs::Analysis,
        _ => Needs::HostOnly,
    }
}

/// One connected, consented, unpaused Worker as the scheduler sees it.
#[derive(Clone, Debug)]
pub struct WorkerView {
    pub device_id: String,
    pub caps: Capabilities,
    /// Measured at join. `None` = not measured: no transfer limit applied.
    pub link_mbps: Option<f64>,
}

/// A file transfer that would take longer than this is not worth it: the Host
/// keeps the step (or another Worker with a faster link takes it).
pub const TRANSFER_BUDGET_SECS: f64 = 20.0;

fn can_run(caps: &Capabilities, needs: Needs, host_embed: Option<&str>) -> bool {
    match needs {
        Needs::HostOnly => false,
        Needs::Pdf => true,
        Needs::Ocr => caps.ocr,
        Needs::Whisper => caps.whisper,
        Needs::Vision => caps.vision_model.is_some(),
        Needs::Llm => caps.llm_model.is_some(),
        Needs::Analysis => caps.llm_model.is_some() && caps.embed_model.as_deref() == host_embed,
    }
}

/// Higher is better. Model work is GPU first, then measured speed. File parsing
/// and OCR run on the CPU, so cores decide.
fn score(caps: &Capabilities, needs: Needs) -> f64 {
    let cores = caps.physical_cores as f64;
    match needs {
        Needs::Pdf | Needs::Ocr | Needs::Whisper => cores,
        _ => {
            let gpu = if caps.gpu { 1e6 } else { 0.0 };
            gpu + caps.decode_tok_s.unwrap_or(0.0) * 100.0 + cores
        }
    }
}

/// Ranked nodes for a step, best first, for `Dispatcher::candidates`.
///
/// * Host-only steps run on the Host.
/// * Workers that can't run the step, or whose link would take longer than
///   [`TRANSFER_BUDGET_SECS`] to send its file, are left out.
/// * The Host is ranked among them only if it has a usable GPU (MESH-14). A
///   GPU-less Host offloads everything it can; the executor still falls back to
///   it if every Worker fails.
pub fn rank(task: &str, file_bytes: u64, host: &Capabilities, workers: &[WorkerView]) -> Vec<Node> {
    let needs = needs(task);
    if needs == Needs::HostOnly {
        return vec![Node::Local];
    }
    let host_embed = host.embed_model.as_deref();

    let mut ranked: Vec<(f64, Node)> = workers
        .iter()
        .filter(|w| can_run(&w.caps, needs, host_embed))
        .filter(|w| match w.link_mbps {
            Some(mbps) if file_bytes > 0 && mbps > 0.0 => {
                (file_bytes as f64 * 8.0) / (mbps * 1e6) <= TRANSFER_BUDGET_SECS
            }
            _ => true,
        })
        .map(|w| (score(&w.caps, needs), Node::Worker(w.device_id.clone())))
        .collect();
    if host.gpu && can_run(host, needs, host_embed) {
        ranked.push((score(host, needs), Node::Local));
    }
    // Stable, so equal scores keep the input order (the hub sorts by device id)
    // and the same mesh always ranks the same way.
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().map(|(_, n)| n).collect()
}

/// A Worker this much faster than the Host plans instead of it (MESH-10).
pub const DELEGATE_SPEEDUP: f64 = 2.0;

/// A Worker whose model has this many times the Host's parameters plans, even
/// at the Host's speed (MESH-10, revised 2026-11-01): a 3B plans and writes
/// far better than a 0.5B, and speed alone missed that.
pub const DELEGATE_MODEL_RATIO: f64 = 2.0;

/// Parameter count in billions from an Ollama tag: `llama3.2:3b` → 3.0,
/// `qwen2.5:0.5b-instruct` → 0.5. `None` when the tag doesn't say
/// (`phi3:mini`, `mixtral:8x7b`, no tag), so an unknown size never wins on size.
pub fn model_params_b(model: &str) -> Option<f64> {
    let tag = model.split_once(':')?.1;
    let digits: String = tag.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let next = tag[digits.len()..].chars().next()?;
    if !next.eq_ignore_ascii_case(&'b') {
        return None;
    }
    digits.parse().ok().filter(|b: &f64| *b > 0.0)
}

/// Who should plan this turn: `None` = the Host itself.
///
/// A pinned device wins if it is connected and has a model. Otherwise a Worker
/// qualifies if it decodes at least [`DELEGATE_SPEEDUP`]× the Host's speed, or
/// runs a model at least [`DELEGATE_MODEL_RATIO`]× the Host's size while being
/// no slower. Among those, the biggest model wins, then the fastest. With no
/// Host measurement, the Host plans: handing off on a guess would be worse
/// than not handing off.
pub fn delegator(host: &Capabilities, workers: &[WorkerView], pinned: Option<&str>) -> Option<String> {
    let has_llm = |w: &&WorkerView| w.caps.llm_model.is_some();
    if let Some(pin) = pinned {
        if let Some(w) = workers.iter().filter(has_llm).find(|w| w.device_id == pin) {
            return Some(w.device_id.clone());
        }
    }
    let host_speed = host.decode_tok_s?;
    let size = |c: &Capabilities| c.llm_model.as_deref().and_then(model_params_b);
    let host_size = size(host);
    let bigger = |c: &Capabilities| match (size(c), host_size) {
        (Some(w), Some(h)) => w >= h * DELEGATE_MODEL_RATIO,
        _ => false,
    };
    workers
        .iter()
        .filter(has_llm)
        .filter_map(|w| w.caps.decode_tok_s.map(|s| (s, w)))
        .filter(|(s, w)| *s >= host_speed * DELEGATE_SPEEDUP || (bigger(&w.caps) && *s >= host_speed))
        .max_by(|a, b| {
            let (sa, sb) = (size(&a.1.caps).unwrap_or(0.0), size(&b.1.caps).unwrap_or(0.0));
            sa.total_cmp(&sb).then(a.0.total_cmp(&b.0))
        })
        .map(|(_, w)| w.device_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(gpu: bool, tok_s: f64, cores: usize) -> Capabilities {
        Capabilities {
            physical_cores: cores,
            ram_gb: 16.0,
            gpu,
            llm_model: Some("qwen2.5:1.5b".into()),
            vision_model: Some("moondream".into()),
            embed_model: Some("nomic-embed-text".into()),
            whisper: true,
            ocr: true,
            decode_tok_s: Some(tok_s),
            ..Default::default()
        }
    }

    fn worker(id: &str, caps: Capabilities) -> WorkerView {
        WorkerView { device_id: id.into(), caps, link_mbps: None }
    }

    fn w(id: &str) -> Node {
        Node::Worker(id.into())
    }

    #[test]
    fn the_knowledge_base_step_and_unknown_tasks_stay_on_the_host() {
        let workers = [worker("gpu", caps(true, 90.0, 8))];
        let host = caps(false, 8.0, 2);
        assert_eq!(rank("search_knowledge", 0, &host, &workers), [Node::Local]);
        assert_eq!(rank("some_future_tool", 0, &host, &workers), [Node::Local]);
    }

    #[test]
    fn a_gpu_less_host_offloads_and_a_gpu_host_competes() {
        let workers = [worker("cpu", caps(false, 10.0, 8)), worker("gpu", caps(true, 90.0, 8))];
        assert_eq!(rank("summarize", 0, &caps(false, 8.0, 2), &workers), [w("gpu"), w("cpu")]);
        let strong_host = caps(true, 200.0, 8);
        assert_eq!(rank("summarize", 0, &strong_host, &workers), [Node::Local, w("gpu"), w("cpu")]);
    }

    #[test]
    fn cpu_tools_rank_by_cores_not_gpu() {
        let workers = [worker("gpu-2core", caps(true, 90.0, 2)), worker("cpu-16core", caps(false, 5.0, 16))];
        assert_eq!(rank("parse_pdf", 0, &caps(false, 8.0, 2), &workers), [w("cpu-16core"), w("gpu-2core")]);
    }

    #[test]
    fn a_worker_missing_the_model_or_the_hosts_embed_model_is_never_chosen() {
        let mut no_vision = caps(true, 90.0, 8);
        no_vision.vision_model = None;
        let mut other_embed = caps(true, 90.0, 8);
        other_embed.embed_model = Some("bge-m3".into());
        let workers = [worker("nv", no_vision), worker("oe", other_embed)];
        let host = caps(false, 8.0, 2);
        assert_eq!(rank("analyze_image", 0, &host, &workers), [w("oe")]);
        assert_eq!(rank("summarize", 0, &host, &workers), [w("nv")], "same embed model, same ranking");
        assert_eq!(rank("answer_directly", 0, &host, &workers).len(), 2, "prose needs no embedder");
    }

    #[test]
    fn a_slow_link_keeps_a_big_file_off_that_worker() {
        let mut slow = worker("slow-wifi", caps(true, 90.0, 8));
        slow.link_mbps = Some(5.0);
        let mut fast = worker("ethernet", caps(false, 10.0, 4));
        fast.link_mbps = Some(900.0);
        let host = caps(false, 8.0, 2);
        let fifty_mb = 50 * 1024 * 1024;
        assert_eq!(rank("transcribe_audio", fifty_mb, &host, &[slow.clone(), fast.clone()]), [w("ethernet")]);
        assert_eq!(rank("transcribe_audio", 0, &host, &[slow, fast]).len(), 2);
    }

    #[test]
    fn planning_moves_only_to_a_clearly_faster_worker_or_the_pinned_one() {
        let host = caps(false, 10.0, 2);
        let twice = [worker("a", caps(true, 25.0, 8))];
        assert_eq!(delegator(&host, &twice, None), Some("a".into()));
        let barely = [worker("b", caps(false, 15.0, 8))];
        assert_eq!(delegator(&host, &barely, None), None, "1.5× is not worth a hop");
        assert_eq!(delegator(&host, &barely, Some("b")), Some("b".into()), "the user's pin wins");
        assert_eq!(delegator(&host, &barely, Some("gone")), None, "a pin to a missing device is ignored");
        let unmeasured = Capabilities { decode_tok_s: None, ..host };
        assert_eq!(delegator(&unmeasured, &twice, None), None);
    }

    fn with_model(mut c: Capabilities, model: &str) -> Capabilities {
        c.llm_model = Some(model.into());
        c
    }

    /// The lead's 2026-11-01 test: a 0.5B Host measured at 25 tok/s, a Worker
    /// running llama3.2:3b at 28 tok/s. Speed alone kept planning on the 0.5B.
    #[test]
    fn a_much_bigger_model_plans_when_it_is_no_slower() {
        let host = with_model(caps(false, 25.0, 2), "qwen2.5:0.5b-instruct");
        let big = [worker("3b", with_model(caps(false, 28.0, 8), "llama3.2:3b"))];
        assert_eq!(delegator(&host, &big, None), Some("3b".into()));

        let big_but_slow = [worker("3b", with_model(caps(false, 20.0, 8), "llama3.2:3b"))];
        assert_eq!(delegator(&host, &big_but_slow, None), None, "a slower Worker would stretch the turn");

        let same_size = [worker("q", with_model(caps(false, 28.0, 8), "qwen2.5:0.5b"))];
        assert_eq!(delegator(&host, &same_size, None), None, "same model, 1.1× speed: no hop");

        let unknown = [worker("u", with_model(caps(false, 28.0, 8), "phi3:mini"))];
        assert_eq!(delegator(&host, &unknown, None), None, "an unknown size never wins on size");
    }

    #[test]
    fn the_bigger_model_beats_the_faster_small_one() {
        let host = with_model(caps(false, 10.0, 2), "qwen2.5:0.5b");
        let workers = [
            worker("fast-small", with_model(caps(true, 90.0, 8), "qwen2.5:1.5b")),
            worker("big", with_model(caps(true, 30.0, 8), "qwen2.5:7b")),
        ];
        assert_eq!(delegator(&host, &workers, None), Some("big".into()));
    }

    #[test]
    fn model_sizes_parse_from_ollama_tags() {
        assert_eq!(model_params_b("llama3.2:3b"), Some(3.0));
        assert_eq!(model_params_b("qwen2.5:0.5b-instruct"), Some(0.5));
        assert_eq!(model_params_b("qwen2.5:7B-instruct-q4_K_M"), Some(7.0));
        assert_eq!(model_params_b("phi3:mini"), None);
        assert_eq!(model_params_b("mixtral:8x7b"), None);
        assert_eq!(model_params_b("llama3.2"), None);
        assert_eq!(model_params_b("x:b"), None);
    }
}
