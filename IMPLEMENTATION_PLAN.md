# TrustCore AI — Build Status

> **Stack:** pure-Rust Tauri v2 desktop app. No Python, no sidecar, single binary.
> **Event:** SIH 2026, PS SIH26117 (Mangalore Refinery & Petrochemicals). Fully air-gappable after first setup.
> **Branch:** `feature/sovereign-workbench`

---

## Where it stands

| Phase | Scope | State |
|---|---|---|
| 0 | Cargo workspace, toolchain | ✅ done |
| 1 | Orchestration core (engine, registry, deterministic validator, retry-ceiling planner) | ✅ done, 8 tests |
| 2 | Five tool modules + analysis + executor + quality gate + compiler | ✅ done, 4 smoke tests |
| 3 | Session memory (rolling compression) + AES-256-GCM persistent memory | ✅ done, 3 tests |
| 4 | React UI: bootstrap gate, stepper, HITL modal, report, audit sidebar | ✅ done, tsc clean |
| 4.1 | Profile-aware, OS-aware model recommendation + merged analysis call (see below) | 🚧 in progress (2026-10-10) |
| 5 | PyInstaller-free packaging + air-gap doc | ⏳ not started (stretch) |

`cargo test -p workbench-core -j 1` → **45/45** and `cargo test -p tauri-app -j 1` → **20/20** (2026-10-10, after the hackathon fix pass and the timing instrumentation). `tsc` clean, no warnings.

---

## Phase 4.1: profile-aware, OS-aware model recommendation (in progress)

**Goal:** an ordinary consumer machine should run a model that is both smooth and as smart as the hardware allows. Today the recommender is a 4-step ladder on cores and RAM. It knows nothing about the user's work, it reads every non-NVIDIA machine (Apple Silicon, AMD) as CPU-only, and its catalogue predates newer small models on Ollama: qwen3.5 0.8b/2b/4b with built-in vision, gemma4 e2b/e4b, granite4.2 3b/8b, and minicpm-v4.6 1b for vision.

**Design rule:** the recommender is deterministic code and tables, never an LLM. It runs before any model exists, and this way it can explain itself and be unit-tested.

| Item | What | Owner | State |
|---|---|---|---|
| E0 | `examples/model_check.rs`: runs a model through the app's real JSON schemas (intent, plan, report, plus vision if multimodal) and reports pass/fail and load/prefill/decode speed. Curated shortlist checked on the 2-core laptop and the GPU laptop; a model that fails a schema is never recommended. | fix | ⏳ |
| R1 | OS/backend-aware detection: Apple Silicon (Metal, unified memory), AMD VRAM on Linux (sysfs) and Windows (registry, real 64-bit VRAM), CPU AVX2/AVX-512 (no AVX2 → one tier down), per-OS RAM headroom (Win/mac 3 GB, Linux 1.5 GB). | fix | ⏳ |
| E1 | Profile page before hardware detection: industry, kind of work, inputs (docs/photos/voice/code), speed-vs-quality priority, language. Saved as `profile.json` and asked once. Maps deterministically to use-case weights, vision/audio needs, speed target, context size, a short persona line and example prompts. | fix | ⏳ |
| R2 | Scored recommender: memory budget per backend → filter by verified + fits → estimated tok/s vs the profile's speed target → best profile-fit quality. Returns reasons plus faster/smarter alternatives. Skips the vision model when no photos are expected; uses a verified multimodal resident model instead of a second vision model when possible. | fix | ⏳ |
| E2 | Profile at runtime: a one-line persona in the system prompts, and industry-specific example prompts. | fix | ⏳ |
| M | **Merge `summarize` + `compare_to_sop` into one `assess` call.** Both currently get the same inputs, and each re-embeds the evidence (an embedding-model reload) and prefills about half the context. New registry task `assess` (Analyze stage); a deterministic post-validation rewrite in `planner/` folds the pair into one step (union of dependencies, dependents repointed, also applied to resumed plans); `fallback_plan` emits `assess` when knowledge is needed; the single tasks stay valid. Measured before/after with the `e2e` timing table. | feature | ⏳ |

Phase 5 (packaging) stays reserved for the installer / Ollama sidecar / offline bundle and is still deferred.

---

## Architecture (as built)

```
Tauri app  (one binary)
├─ React/TS frontend (src/)                 Rust host (src-tauri/src/)
│   BootstrapGate ── invoke() ───────────►  bootstrap/ollama.rs   is/ensure_ollama_installed (unchanged from scaffold)
│   PromptPanel (file drop → base64)        bootstrap/hardware.rs  detect_hardware (sysinfo RAM + nvidia-smi/CIM GPU)
│   Stepper / HitlModal / ReportView        bootstrap/models.rs    recommend_models tier table + ensure_models(list)
│   AuditSidebar                            lib.rs   submit_turn / probe_system / warm_model / *_memory / audit_paths
│        │  invoke + Channel<StepEvent>     events.rs  ChannelSink: StepEvent → Tauri Channel
│        ▼
└─ crates/workbench-core/   (Tauri-free, unit-testable)
     engine/schemas.rs     IntentResult · TaskStep · Plan · ToolResult · QualityReport · FinalReport  (serde)
     engine/ollama_engine  ONE resident model; roles A/B/C = system-prompt swap; keep_alive:-1.
                           analyze_image / embeddings call specialists with keep_alive:0.
     planner/registry.rs   TASK_REGISTRY (9 tasks) + validate_plan() — pure-Rust DAG + file-existence checks
     planner/planner.rs    parse_intent → build_plan → validate; MAX_PLAN_ATTEMPTS = 3 → HITL park
     executor/executor.rs  strict sequential loop; per-step StepEvent; a failing tool degrades, never aborts
     executor/quality.rs   rule-based assertions (all steps reported, no failures, RAG non-empty, report non-empty)
     executor/compiler.rs  role C synthesis; plain-text fallback if JSON is bad twice
     tools/document.rs     parse_pdf   — pdfplumber crate (digital PDF: text + tables). Scanned PDFs out of scope.
     tools/ocr.rs          ocr_image   — ocrs + rten (pure Rust). Degrades if .rten models absent.
     tools/audio.rs        transcribe_audio — whisper-rs; symphonia decode + linear resample to 16k mono.
     tools/vision.rs       analyze_image — moondream via Ollama, keep_alive:0
     tools/rag.rs          search_knowledge — chunk + embed (nomic-embed-text) into an embedded LanceDB
                           table (data/lancedb/sop_kb); cosine-distance nearest-neighbour query.
     tools/analysis.rs     summarize / compare_to_sop — resident model over accumulated evidence
     memory/session.rs     session_context.json — append + rolling compression via the resident model
     memory/persistent.rs  persistent_memory.json — AES-256-GCM, PBKDF2-HMAC-SHA256 over machine-uid
     pipeline.rs           run_turn: the full sequence (memory written only on a user confirm)
     kb/                   two seed SOPs (SOP-COR-014 corrosion, SOP-ROT-007 pump condition)
```

### Turn flow
`idle → parse intent (A) → build+validate plan (B, ≤3 tries) → [HITL if unresolved] → sequential tools → quality check → synthesise (C) → append session memory → persist long-term → done`, every checkpoint streamed as a `StepEvent`.

---

## Deviations from the original blueprint (all deliberate)

| Blueprint said | Built | Why |
|---|---|---|
| Python + Streamlit + FastAPI sidecar | Pure Rust in the Tauri app; events over Tauri `Channel` | No interpreter to ship; ~500–800 MB of Python deps avoided; no PyInstaller fragility |
| `pdfplumber` (Python) with OCR fallback for scanned PDFs | `pdfplumber` **Rust crate**; scanned PDFs explicitly unsupported (tool reports "no extractable text") | Keeps it single-binary; scanned docs are a separate OCR path anyway |
| whisper.cpp binary + Tesseract installer | `whisper-rs` (whisper.cpp compiled in) + `ocrs`/`rten` (pure Rust) | No external binaries; Windows-friendly |
| ChromaDB (Python) | **Embedded LanceDB** (Arrow-backed, `data/lancedb/sop_kb`) | Pure-Rust, no server. (Briefly a hand-rolled JSON cosine loop during development to keep build times down; swapped to LanceDB once the rest was stable.) |
| "AES-GCM via `cryptography.fernet`" | Literal AES-256-GCM (RustCrypto `aes-gcm`) | The blueprint's phrasing was self-contradictory; this is the stronger option. Machine-derived key = obfuscation-grade, documented in `persistent.rs` |
| 2 automated re-prompts then HITL | `MAX_PLAN_ATTEMPTS = 3` (1 initial + 2 retries) then park for the user | Exactly the blueprint's ceiling |

---

## Toolchain (installed on the dev machine)

Rust 1.98 (MSVC) · CMake 4.4.3 · LLVM 22.1.8 (`LIBCLANG_PATH` for `whisper-rs` bindgen) · Node 24 · Ollama 0.33 with `moondream`, `nomic-embed-text` (+ `llama3.2:3b`, `qwen3:4b`) pulled.

Build env for a clean shell:
```bash
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:$PATH"
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"
```

---

## Run it

```bash
git checkout feature/sovereign-workbench
npm install                    # already done
npm run tauri dev              # first build ~25 min (whisper.cpp + tauri); incremental ~10 s
```

The window opens on `BootstrapGate`: it checks Ollama, probes RAM/GPU, shows the recommended model tier with an override dropdown, pulls anything missing (streamed), warms the resident model, then hands off to the workbench.

**Optional tool models** (the app degrades gracefully without them; drop into `%APPDATA%\com.anubhav_anand.tauri-app\models\`):
- `ggml-base.en.bin` — whisper (HuggingFace `ggml-org/whisper.cpp`)
- `text-detection.rten`, `text-recognition.rten` — ocrs (`ocrs-models.s3-accelerate.amazonaws.com`)

---

## Demo script (SIH26117)

1. Bootstrap: override LLM to a locally-present model (`llama3.2:3b`) so nothing downloads → Download & start.
2. Prompt: *"Assess corrosion risk on pump P-101 and check it against our SOPs."* Attach `inspection.pdf` + `valve.png` (+ a voice note if the whisper model is installed).
3. Watch the stepper: `Parsing → Validating → parse_pdf → analyze_image → search_knowledge → summarize/compare_to_sop → Synthesizing → Done`.
4. Report panel shows findings + SOP citations (SOP-COR-014 / SOP-ROT-007) + a `degraded` badge if audio/OCR models are absent.
5. Expand the audit sidebar: per-step timings, active model context, resolved local paths.
6. ~~HITL demo~~ — **not demonstrable as of 2026-10-10.** Its only practical trigger was the empty-fallback bug (README problems 4 and 11). Show the deterministic validator on the audit sidebar's "How it works" list instead, or add a deliberate "review plan" entry point first.
7. Click **Remember** on a proposed fact under the report → it appears in the sidebar's *Remembered facts* → open a **new chat** and ask about it: it's known there, but chat 1's conversation is not → restart the app: the fact reloads (`persistent_memory.json`, ciphertext on disk).

---

## Open items

1. **Phase 5 packaging** — `cargo tauri build` installer; bundle Ollama as a sidecar and pre-cache the models + whisper/ocrs weights for the air-gapped machine; tighten `tauri.conf.json` CSP.
2. **LanceDB ANN index** — the table currently does a flat (exact) scan; `table.create_index(Index::Auto)` would add an approximate index if the KB ever grows past ~10⁴ chunks.
3. **HITL entry point** — the modal works but nothing reaches it from the GUI any more (README problem 11).

### Done since Phase 4
- **RAG on embedded LanceDB** — `tools/rag.rs`, verified end-to-end (SOP retrieved + cited).
- **`resume_turn`** — the HITL modal's *Re-run with this plan* / *Run anyway* buttons resume a parked turn from the user-edited plan (`workbench_core::resume_turn` → shared `run_from_plan` tail; `resume_turn` Tauri command; `resumeTurn` UI wrapper).
- **Planner robustness** — task names are tolerated with a trailing ` [Stage]` label / quotes (`normalize_task_name`), and the registry prompt block quotes just the name.

### Hackathon fix pass (2026-10-10) — execution log parts 10–14
- **Linux Ollama without sudo** — per-user install, app-managed `ollama serve`; `renice` now actually applies (it never did against the systemd `ollama` user).
- **`answer_directly`** — closes the empty-fallback hole that parked a bare "hi"; registry task #9.
- **`Exchange.ts`** — was saved as 0 on every turn; now stamped.
- **Cross-chat memory** — propose → you confirm; `GLOBAL_MEMORY_ENABLED`, `persist_long_term` and `end_session` deleted.
- **Bootstrap** — tier from total RAM (free RAM only warns); installed models marked and never counted as downloads.
- **`.gitattributes`** — LF by default, `.bat` CRLF, binaries marked; no renormalisation.
