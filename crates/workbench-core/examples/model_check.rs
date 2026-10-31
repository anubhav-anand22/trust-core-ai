//! Quick check: can this model run TrustCore's pipeline, and how fast, on THIS machine?
//!
//! Runs the model through the app's real structured-output calls — intent (role
//! A), plan with and without files (role B), report (role C) — plus one vision
//! call when asked. A model that can't return valid JSON for all three schemas
//! can't drive the pipeline, however good it is elsewhere; the bootstrap
//! recommender only recommends models that pass here.
//!
//! ```text
//! cargo run -p workbench-core --example model_check -j 1 -- [--vision] <tag> [tag ...]
//! ```
//!
//! Env: `WB_CTX` (default 4096), `WB_THREADS` (default: what the app would use).
//! Models must already be pulled (`ollama pull <tag>`). Prints one row per call and
//! a markdown summary table at the end for docs/09.

use workbench_core::engine::schemas::{FileKind, InputFile, ToolResult};
use workbench_core::engine::{CallTiming, OllamaEngine, ResourceLimits};
use workbench_core::planner::validate_plan;

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    timing: Option<CallTiming>,
}

#[derive(Default)]
struct Summary {
    tag: String,
    passed: usize,
    total: usize,
    decode_tok_s: Vec<f64>,
    load_ms: Option<u64>,
    notes: Vec<String>,
}

#[tokio::main]
async fn main() {
    let mut vision = false;
    let mut tags = Vec::new();
    for a in std::env::args().skip(1) {
        if a == "--vision" {
            vision = true;
        } else {
            tags.push(a);
        }
    }
    if tags.is_empty() {
        eprintln!("usage: model_check [--vision] <tag> [tag ...]");
        std::process::exit(2);
    }

    let num_ctx: u64 = env_or("WB_CTX", 4096);
    // Match what the app hands Ollama, so the speed is the speed a user sees.
    let threads: u32 = env_or("WB_THREADS", physical_cores().saturating_sub(1).max(1) as u32);
    println!("num_ctx={num_ctx} num_thread={threads}\n");

    // Real files on disk: validate_plan checks they exist.
    let dir = tempfile::tempdir().expect("tempdir");
    let pdf = dir.path().join("inspection.pdf");
    let png = dir.path().join("valve.png");
    std::fs::write(&pdf, b"%PDF-1.4 placeholder").unwrap();
    write_test_image(&png);
    let uploads = vec![input(&pdf, FileKind::Pdf), input(&png, FileKind::Image)];

    let mut summaries = Vec::new();
    for tag in &tags {
        println!("==== {tag} ====");
        let engine = OllamaEngine::new("http://127.0.0.1:11434", tag.clone(), tag.clone(), "nomic-embed-text")
            .with_limits(ResourceLimits {
                num_ctx,
                num_thread: threads,
                ..Default::default()
            });
        let checks = run_checks(&engine, &uploads, &png, vision).await;
        let mut s = Summary { tag: tag.clone(), total: checks.len(), ..Default::default() };
        for c in &checks {
            let speed = c.timing.as_ref().map(fmt_timing).unwrap_or_default();
            println!(
                "  {:<16} {:<4} {:<44} {}",
                c.name,
                if c.ok { "ok" } else { "FAIL" },
                c.detail.chars().take(44).collect::<String>(),
                speed
            );
            if c.ok {
                s.passed += 1;
            } else {
                s.notes.push(format!("{}: {}", c.name, c.detail));
            }
            if let Some(t) = &c.timing {
                if let Some(tps) = tok_per_s(t) {
                    s.decode_tok_s.push(tps);
                }
                if s.load_ms.is_none() {
                    s.load_ms = t.load_ms;
                }
            }
        }
        println!();
        summaries.push(s);
    }

    println!("| model | schemas | decode tok/s (median) | first load | verdict |");
    println!("|---|---|---|---|---|");
    for s in &summaries {
        let mut v = s.decode_tok_s.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let median = v.get(v.len() / 2).map(|x| format!("{x:.1}")).unwrap_or("–".into());
        let verdict = if s.passed == s.total { "PASS" } else { "fail" };
        println!(
            "| `{}` | {}/{} | {} | {} | {} |",
            s.tag,
            s.passed,
            s.total,
            median,
            s.load_ms.map(|m| format!("{:.1} s", m as f64 / 1000.0)).unwrap_or("–".into()),
            verdict
        );
    }
    for s in summaries.iter().filter(|s| !s.notes.is_empty()) {
        println!("\n{} failures:", s.tag);
        for n in &s.notes {
            println!("  - {n}");
        }
    }
}

async fn run_checks(
    engine: &OllamaEngine,
    uploads: &[InputFile],
    png: &std::path::Path,
    vision: bool,
) -> Vec<Check> {
    let mut out = Vec::new();
    let prompt = "Assess corrosion risk on pump P-101 and check it against our SOPs.";

    // Role A.
    let intent = match engine.parse_intent(prompt, "", uploads).await {
        Ok(i) => {
            out.push(check("intent", true, format!("{} intent(s), kb={}", i.intents.len(), i.needs_knowledge), engine));
            Some(i)
        }
        Err(e) => {
            out.push(check("intent", false, short(&e.to_string()), engine));
            None
        }
    };

    // Role B, with files. Schema-valid is the gate; validator-clean is reported
    // too (a plan the validator rejects costs a retry, then the fallback).
    if let Some(intent) = &intent {
        match engine.build_plan(prompt, "", intent, uploads, &[]).await {
            Ok(p) => {
                let errs = validate_plan(&p, uploads);
                let detail = if errs.is_empty() {
                    format!("{} steps, validator clean", p.steps.len())
                } else {
                    format!("{} steps, {} validator issue(s)", p.steps.len(), errs.len())
                };
                out.push(check("plan+files", true, detail, engine));
            }
            Err(e) => out.push(check("plan+files", false, short(&e.to_string()), engine)),
        }
    }

    // Role B, bare message: should be a single `answer_directly`.
    let hi = "hi, what can you do?";
    if let Ok(i) = engine.parse_intent(hi, "", &[]).await {
        match engine.build_plan(hi, "", &i, &[], &[]).await {
            Ok(p) => {
                let tasks: Vec<_> = p.steps.iter().map(|s| s.task.as_str()).collect();
                out.push(check("plan bare", true, tasks.join(","), engine));
            }
            Err(e) => out.push(check("plan bare", false, short(&e.to_string()), engine)),
        }
    }

    // Role C over realistic tool output, including the memory-proposal field.
    let results = vec![
        tool("x1", "parse_pdf", serde_json::json!({
            "text": "Inspection 2026-09-02, Unit-2, MRPL Phase-3. Pump P-101 casing: \
                     surface rust 30% of area, wall loss 1.2 mm vs 6 mm nominal. Seal weep observed.",
            "source": "inspection.pdf"
        })),
        tool("kb", "search_knowledge", serde_json::json!({ "chunks": [
            { "text": "Wall loss above 15% of nominal requires engineering review within 7 days.",
              "source": "SOP-COR-014" }
        ]})),
    ];
    match engine.compile_report(prompt, &results, "", "").await {
        Ok(r) => out.push(check(
            "report",
            !r.summary.trim().is_empty(),
            format!("{} findings, {} cites, {} proposals", r.findings.len(), r.citations.len(), r.memory_proposals.len()),
            engine,
        )),
        Err(e) => out.push(check("report", false, short(&e.to_string()), engine)),
    }

    if vision {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(std::fs::read(png).unwrap());
        match engine.analyze_image(&b64, "Describe any visible corrosion or damage, briefly.").await {
            Ok(t) => out.push(check("vision", !t.trim().is_empty(), format!("{} chars", t.len()), engine)),
            Err(e) => out.push(check("vision", false, short(&e.to_string()), engine)),
        }
    }
    out
}

fn check(name: &'static str, ok: bool, detail: String, engine: &OllamaEngine) -> Check {
    Check { name, ok, detail, timing: engine.timings().last().cloned() }
}

/// A thinking model under structured output returns an empty body, which serde
/// reports as "EOF while parsing" — name it, it is the most common failure.
fn short(e: &str) -> String {
    if e.contains("EOF while parsing") {
        "empty body (thinking model?)".into()
    } else {
        e.chars().take(80).collect()
    }
}

fn tok_per_s(t: &CallTiming) -> Option<f64> {
    match (t.gen_tokens, t.decode_ms) {
        (Some(n), Some(ms)) if ms > 0 => Some(n as f64 / (ms as f64 / 1000.0)),
        _ => None,
    }
}

fn fmt_timing(t: &CallTiming) -> String {
    format!(
        "wall {:>5.1}s  load {:>5}ms  prefill {:>6}ms  {}",
        t.wall_ms as f64 / 1000.0,
        t.load_ms.map(|v| v.to_string()).unwrap_or("–".into()),
        t.prefill_ms.map(|v| v.to_string()).unwrap_or("–".into()),
        tok_per_s(t).map(|v| format!("{v:.1} tok/s")).unwrap_or_default()
    )
}

fn tool(id: &str, task: &str, data: serde_json::Value) -> ToolResult {
    ToolResult {
        step_id: id.into(),
        task: task.into(),
        ok: true,
        data,
        error: None,
        warning: None,
        elapsed_ms: 0,
    }
}

fn input(path: &std::path::Path, kind: FileKind) -> InputFile {
    InputFile {
        path: path.to_string_lossy().into_owned(),
        kind,
        original_name: path.file_name().unwrap().to_string_lossy().into_owned(),
    }
}

/// A grey pipe with an orange-brown patch — enough for a vision model to have
/// something to describe, without depending on the Pillow-generated fixtures.
fn write_test_image(path: &std::path::Path) {
    let img = image::RgbImage::from_fn(256, 192, |x, y| {
        let in_pipe = (60..132).contains(&y);
        let in_rust = in_pipe && (90..170).contains(&x) && (70..122).contains(&y);
        if in_rust {
            image::Rgb([150 + ((x * 7 + y * 3) % 40) as u8, 70, 20])
        } else if in_pipe {
            image::Rgb([140, 145, 150])
        } else {
            image::Rgb([235, 235, 230])
        }
    });
    img.save(path).expect("write test image");
}

/// Physical cores via `sysinfo` (a dev-dependency), the same count the app's
/// hardware probe uses — so the benchmark runs on the thread budget users get,
/// on every OS. Falls back to half the logical count, as the probe does.
fn physical_cores() -> usize {
    sysinfo::System::new()
        .physical_core_count()
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).max(1))
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
