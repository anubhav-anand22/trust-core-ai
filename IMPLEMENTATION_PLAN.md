# Sovereign On-Premise AI Workbench — Build Status

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
| 5 | PyInstaller-free packaging + air-gap doc | ⏳ not started (stretch) |

`cargo test -p workbench-core` → **20/20**. `cargo check --workspace --all-targets` → clean, no warnings.

---

## Architecture (as built)

```
Tauri app  (one binary)
├─ React/TS frontend (src/)                 Rust host (src-tauri/src/)
│   BootstrapGate ── invoke() ───────────►  bootstrap/ollama.rs   is/ensure_ollama_installed (unchanged from scaffold)
│   PromptPanel (file drop → base64)        bootstrap/hardware.rs  detect_hardware (sysinfo RAM + nvidia-smi/CIM GPU)
│   Stepper / HitlModal / ReportView        bootstrap/models.rs    recommend_models tier table + ensure_models(list)
│   AuditSidebar                            lib.rs   submit_turn / probe_system / warm_model / end_session / audit_paths
│        │  invoke + Channel<StepEvent>     events.rs  ChannelSink: StepEvent → Tauri Channel
│        ▼
└─ crates/workbench-core/   (Tauri-free, unit-testable)
     engine/schemas.rs     IntentResult · TaskStep · Plan · ToolResult · QualityReport · FinalReport  (serde)
     engine/ollama_engine  ONE resident model; roles A/B/C = system-prompt swap; keep_alive:-1.
                           analyze_image / embeddings call specialists with keep_alive:0.
     planner/registry.rs   TASK_REGISTRY (7 tasks) + validate_plan() — pure-Rust DAG + file-existence checks
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
     pipeline.rs           run_turn: the full sequence + persist_long_term every turn
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
6. HITL demo: a prompt that yields an unschedulable plan → exactly 2 silent retries → modal with the validator's complaints + editable plan → fix the plan and **Re-run with this plan** (or **Run anyway** to bypass the validator) → the turn resumes from `execute_plan`.
7. Restart the app → persistent memory (`persistent_memory.json`, ciphertext on disk) reloads the facility digest.

---

## Open items

1. **Phase 5 packaging** — `cargo tauri build` installer; pre-cache Ollama models + whisper/ocrs weights for the air-gapped machine; tighten `tauri.conf.json` CSP.
2. **`.gitattributes`** — repo is CRLF-noisy on this Windows box; harmless, worth a one-liner.
3. **LanceDB ANN index** — the table currently does a flat (exact) scan; `table.create_index(Index::Auto)` would add an approximate index if the KB ever grows past ~10⁴ chunks.

### Done since Phase 4
- **RAG on embedded LanceDB** — `tools/rag.rs`, verified end-to-end (SOP retrieved + cited).
- **`resume_turn`** — the HITL modal's *Re-run with this plan* / *Run anyway* buttons resume a parked turn from the user-edited plan (`workbench_core::resume_turn` → shared `run_from_plan` tail; `resume_turn` Tauri command; `resumeTurn` UI wrapper).
- **Planner robustness** — task names are tolerated with a trailing ` [Stage]` label / quotes (`normalize_task_name`), and the registry prompt block quotes just the name.
