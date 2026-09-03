# Sovereign On-Premise AI Workbench — Prototype Plan

> **Status:** DRAFT for team review · **Event:** SIH 2026, PS SIH26117 (Mangalore Refinery & Petrochemicals) · **Target:** fully air-gapped after first setup, ≤ 8 GB VRAM / 16 GB RAM, CPU-fallback capable

---

## TL;DR

- **Stack:** keep the existing **Tauri v2 desktop app** (React UI + Rust host) as the shell; the agentic pipeline runs as a **Python FastAPI sidecar** on `http://127.0.0.1:8765` that Rust spawns and kills.
- **Rust** keeps what it already has (Ollama installer, model pull-with-progress, warmup) and gains **hardware detection → model-tier auto-selection**.
- **Python** implements the blueprint pipeline: intent parse → deterministic plan validation (pure Python, 2 retries then human-in-the-loop) → sequential tool execution → rule-based quality check → synthesis → two-tier encrypted memory.
- **One resident LLM.** Roles A/B/C are a system-prompt swap, not a model reload. Vision + embedding models are called with `keep_alive: 0`.
- **All 5 tools** in scope: audio, PDF, OCR, vision/VLM, RAG.
- Built in the blueprint's phase order (0→5), each with a validation gate.

---

## Decisions locked

| Question | Decision |
|---|---|
| Blueprint says Python+Streamlit; repo is Tauri+Rust+React | **Tauri shell + Python sidecar.** React stepper UI replaces Streamlit. |
| How many tool modules for the demo | **All 5** end-to-end (audio, PDF, OCR, vision, RAG). |
| whisper.cpp / Tesseract need non-pip installs on Windows | **Substitute pip-only libs:** `faster-whisper`, `rapidocr-onnxruntime`. |
| Which Ollama model | **None fixed.** App detects RAM / GPU / VRAM on startup and picks a compatible tier; user can override in the UI. |

---

## Toolchain status (checked on the dev machine)

| Dependency | Status | Note |
|---|---|---|
| Node / npm | 24.18 / 11.16, `node_modules` present | ready |
| Python | 3.13.9 + 3.14.7 only | sidecar pins **3.12** via `uv` (chromadb / onnxruntime / faster-whisper wheels) |
| uv | 0.11.28 | used for venv + pinned `requirements.txt` |
| **Ollama** | **not installed** | one-time online install via existing Rust `ensure_ollama_installed` |
| **Rust / cargo** | **not installed** | needs `rustup`; first `cargo build` of Tauri ≈ 10–20 min |
| whisper.cpp / Tesseract | n/a | replaced by pip libs above |

---

## Architecture

```
Tauri app
├─ React/TS frontend (src/)            Rust host (src-tauri/src/)
│   BootstrapGate ─ invoke() ───────►  is_ollama_installed / ensure_ollama_installed   (EXISTS, reuse)
│   Stepper / HITL modal               detect_hardware / recommend_models              (NEW: hardware.rs)
│   PromptPanel (file dropzone)        ensure_required_models(models: Vec<String>)     (MODIFY: was hardcoded)
│   AuditSidebar                       start_sidecar / stop_sidecar / sidecar_health   (NEW: sidecar.rs)
│        │  fetch + SSE                 warmup + write runtime_config.json              (MODIFY setup())
│        ▼
└─ Python sidecar  http://127.0.0.1:8765   (workbench/, spawned & killed by Rust)
     api/server.py    FastAPI: /health, POST /session/run (multipart), GET /session/{id}/events (SSE),
                       POST /session/{id}/resume (HITL), POST /session/{id}/end
     core/config.py   reads workbench/data/runtime_config.json (model ids + paths) written by Rust
     core/engine.py   ONE resident Ollama model, roles A/B/C = system-prompt swap; Pydantic-typed JSON
     core/schemas.py  IntentResult, TaskStep, Plan, ToolResult, QualityReport, FinalReport
     core/registry.py TASK_REGISTRY + validate_plan()  (pure-Python DAG + file-existence checks)
     core/planner.py  parse_intent → build_plan → validate; MAX_PLAN_ATTEMPTS=3 (1+2) → HITL pause
     core/executor.py strict sequential tool queue, emits StepEvent
     core/quality.py  rule-based sanity assertions
     core/compiler.py role C synthesis → FinalReport
     core/memory.py   session_context.json (append + rolling compress) · persistent_memory.json (Fernet)
     tools/ rag.py document.py ocr.py audio.py vision.py   + tools/base.py
     kb/              seed SOP / safety docs   ·   data/  chroma/ uploads/ *.json
```

**Notes**

- **File uploads:** React sends `File` objects as `multipart/form-data` straight to FastAPI; it writes them under `workbench/data/uploads/<session>/`. No Tauri fs/dialog plugin needed. `tauri.conf.json` already has `csp: null`, so localhost `fetch` + `EventSource` work.
- **Single-resident-LLM discipline:** only the text model gets `keep_alive: -1`. `moondream` (vision) and `nomic-embed-text` (embeddings) use `keep_alive: 0`. The executor is strictly sequential, so two models never load at once.
- **Deterministic validation:** `registry.validate_plan()` is pure Python — checks unknown tasks, DAG ordering (no analysis step before the extraction/retrieval it depends on), and that referenced upload paths exist on disk. On failure the planner is re-prompted with the error text at most **2 times**, then the run pauses for the user (`POST /session/{id}/resume`).

---

## Hardware detection → model tier

`src-tauri/src/hardware.rs`

- **RAM:** `sysinfo` crate → `System::total_memory()`.
- **GPU / VRAM:** try `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`; else Windows PowerShell `Get-CimInstance Win32_VideoController` (name; `AdapterRAM` is a lower bound only); else "integrated / none".

`recommend_models(hw)` — best available signal wins:

| Signal | Resident LLM |
|---|---|
| VRAM ≥ 8 GB **or** RAM ≥ 16 GB | `qwen2.5:7b-instruct` |
| VRAM ≥ 6 GB **or** RAM ≥ 12 GB | `phi4-mini` (fallback `qwen2.5:3b-instruct`) |
| else (CPU-only, low RAM) | `qwen2.5:1.5b-instruct` |

Always added: `moondream` (~1.7 GB) + `nomic-embed-text` (~275 MB). If RAM < 8 GB the UI warns the vision step will be slow and offers to disable it.

React `BootstrapGate` shows detected specs + recommended tier + an override dropdown → calls the existing `ensure_required_models` (Channel progress) with the chosen list → Rust writes `workbench/data/runtime_config.json`, the single source of truth read by `core/config.py`.

---

## Phased implementation

Branch first: `git checkout -b feature/sovereign-workbench`. Each phase ends with its **Validate** gate before the next begins. All new Python functions get beginner-readable docstrings (inputs / outputs / why).

### Phase 0 — Setup & sidecar handshake
- `rustup` install; confirm a clean `cargo build` of the untouched app.
- `workbench/` package: `uv init`, pin Python 3.12, add deps, export pinned `requirements.txt`.
  Deps: `fastapi uvicorn[standard] pydantic python-multipart httpx sse-starlette pdfplumber faster-whisper rapidocr-onnxruntime chromadb cryptography psutil pytest`.
- `workbench/workbench/api/server.py` with just `GET /health`.
- `Cargo.toml`: add `sysinfo`. New `src-tauri/src/sidecar.rs` (`start_sidecar` / `stop_sidecar` / `sidecar_health`); spawn `<venv>/python -m workbench.api.server --port 8765` from `lib.rs` `setup()`, kill on `RunEvent::ExitRequested`.
- `.gitignore`: add `.venv/`, `__pycache__/`, `workbench/data/`.
- **Validate:** `npm run tauri dev` → app opens, sidecar boots, temp UI button shows `/health` = ok; closing the window kills the Python process.

### Phase 1 — Orchestration core (blueprint Phase 1)
- `core/config.py`, `core/schemas.py` (Pydantic models).
- `core/engine.py` — `OllamaEngine.chat(role, user_msg, schema) -> BaseModel`: role A/B/C system prompt, `/api/chat` with `format:"json"` + `keep_alive:-1`, typed-error on bad JSON.
- `core/registry.py` — `TASK_REGISTRY` (`transcribe_audio`, `parse_pdf`, `ocr_image`, `analyze_image`, `search_knowledge`, `summarize`, `compare_to_sop`), each `{produces, requires_files, stage ∈ {extract, retrieve, analyze}}`; `validate_plan(plan, uploads) -> list[str]`.
- `core/planner.py` — `parse_intent()` (A), `build_plan()` (B), validate loop, `MAX_PLAN_ATTEMPTS = 3`, then emit `awaiting_user` and block on resume.
- `core/events.py` — `StepEvent{stage, tool?, attempt?, elapsed_ms?, detail?, payload?}`, asyncio pub/sub; wire `POST /session/run` + `GET /session/{id}/events` (SSE) + `POST /session/{id}/resume`.
- Rust: `hardware.rs`; modify `ensure_required_models` to take `models: Vec<String>`; warmup uses selected LLM; write `runtime_config.json`. React `BootstrapGate` (reuses the existing `Channel<ModelProgress>` pattern in `src/App.tsx`).
- **Validate:** `pytest` on `test_registry.py` + `test_planner_retry.py` (unknown task, bad DAG order, missing file, 2-retries-then-HITL). `curl` a text-only prompt → SSE streams `parsing_context → validating_plan → done` with a schema-valid `Plan`.

### Phase 2 — Sequential tools (blueprint Phase 2)
- `tools/base.py` — `Tool` protocol: `name`, `required_args`, `run(args, ctx) -> ToolResult` (with `elapsed_ms`, `ok`, `error`).
- `tools/rag.py` — `chromadb.PersistentClient(data/chroma)`, collection `sop_kb`, embeddings via Ollama `nomic-embed-text`. `ingest_kb(kb_dir)` chunks `.txt/.md/.pdf` (~800 tok, overlap) on startup if empty. `search(query, top_k=5)`.
- `tools/document.py` — `pdfplumber` text + tables; page with <20 chars but images → flag scanned → route to `ocr_image`.
- `tools/ocr.py` — `RapidOCR()` singleton.
- `tools/audio.py` — `faster_whisper.WhisperModel(size from config, device="cpu", compute_type="int8")`; lazy-load, free after run.
- `tools/vision.py` — `/api/generate` `{model:"moondream", images:[b64], keep_alive:0}`.
- `core/executor.py` — strict `for step in plan.steps`; resolve `depends_on` from a `context` dict; tool exception → `ok=False` + continue (run marked degraded).
- `core/quality.py` — `assert_sane(results, plan) -> QualityReport` (every step has a result; no tracebacks; RAG returned ≥1 chunk if planned; report non-empty).
- `core/compiler.py` — role C: prompt + all `ToolResult`s + session rolling summary + persistent facility metadata → `FinalReport` (1 schema retry, then plain-text fallback).
- **Validate:** `pytest test_tools_smoke.py` on tiny fixtures (1-page PDF, label image, 3-sec wav, 2 seed SOPs). End-to-end `/session/run` with PDF + image + audio → `FinalReport` with ≥1 SOP citation; SSE shows every `executing_tool` stage in order.

### Phase 3 — Memory & state (blueprint Phase 3)
- `core/memory.py`:
  - `session_context.json` — `append_turn(...)`; when turns > N or token estimate > cap, `compress()` folds oldest turns into `rolling_summary` via a lightweight call on the same resident model.
  - `persistent_memory.json` — `{facility_metadata, recurrent_tags, history_digest}`; `load()` decrypts on startup (missing/corrupt → fresh); `persist()` encrypts on `/session/{id}/end` and app reset.
  - Encryption: `cryptography.fernet.Fernet`, key = `PBKDF2HMAC(SHA256, APP_SALT, 200k)` over machine id (Windows `MachineGuid` via `winreg`, fallback `uuid.getnode()`). Documented as obfuscation-grade. *(Blueprint says "AES-GCM (Fernet)" — contradictory; using Fernet as named; `hazmat AESGCM` swap noted if literal GCM is required.)*
- **Validate:** `pytest test_memory.py` (roundtrip, encrypt/decrypt, corrupt-file tolerance). Run 3 turns → `session_context.json` compresses; `persistent_memory.json` is ciphertext on disk and decrypts on next sidecar start.

### Phase 4 — UI & stepper observability (blueprint Phase 4)
Rewrite `src/App.tsx` (drop the test buttons) into components:
- `BootstrapGate` — Ollama check/install → `detect_hardware` → specs + tier + override → `ensure_required_models` → `start_sidecar` → poll `/health`.
- `PromptPanel` — textarea + drag-drop dropzone (audio/pdf/image, multi-file).
- `Stepper` — `Idle → Parsing Context → Validating Plan → Executing Tool X → Synthesizing Output`, driven by SSE `StepEvent`.
- `HitlModal` — on `awaiting_user`: validation errors + editable plan JSON + "Approve anyway" → `POST /session/{id}/resume`.
- `ReportView` — summary / findings / citations / safety notes.
- `AuditSidebar` (collapsible) — per-step `elapsed_ms`, active model + context/token estimate, resolved absolute paths for `data/chroma`, `session_context.json`, `persistent_memory.json`, `data/uploads/`.
- **Validate:** full run in `tauri dev` — attach all 3 modalities, watch the stepper advance live, force a bad plan to trigger `HitlModal`, expand `AuditSidebar`, restart app → persistent memory reloads.

### Phase 5 — Packaging & air-gap (stretch)
- PyInstaller-freeze the sidecar; declare as Tauri `externalBin`; `npm run tauri build` → standalone installer.
- Air-gap doc: pre-pull Ollama models; `uv pip download` wheels to `vendor/`; pre-cache faster-whisper + rapidocr + moondream weights.
- Tighten `tauri.conf.json` CSP: explicit `connect-src http://127.0.0.1:8765`.

---

## File map

**Modify:** `src-tauri/src/lib.rs` (AppState, new commands, `ensure_required_models` signature, warmup, exit hook) · `src-tauri/Cargo.toml` (`sysinfo`) · `src/App.tsx` (full rewrite) · `.gitignore` · `src-tauri/tauri.conf.json` (Phase 5 CSP).

**New — Rust:** `src-tauri/src/hardware.rs`, `src-tauri/src/sidecar.rs`.

**New — Python (`workbench/`):** `pyproject.toml`, `requirements.txt`, `workbench/api/server.py`, `workbench/core/{config,engine,schemas,registry,planner,executor,quality,compiler,memory,events}.py`, `workbench/tools/{base,rag,document,ocr,audio,vision}.py`, `workbench/kb/` (seed SOPs), `workbench/tests/{test_registry,test_planner_retry,test_memory,test_tools_smoke}.py`.

**New — React:** `src/components/{BootstrapGate,PromptPanel,Stepper,HitlModal,ReportView,AuditSidebar}.tsx`, `src/lib/sidecar.ts` (fetch + SSE helpers).

**Reuse as-is:** `is_ollama_installed()`, `ensure_ollama_installed()` (multi-OS installer w/ `Channel<String>` progress). Keep the streaming/percentage loop in `ensure_required_models()` — only the hardcoded model list changes. Keep the `setup()` warmup pattern, repointed to the selected model. Lift the `Channel<ModelProgress>` + `invoke` pattern from `src/App.tsx` into `BootstrapGate`.

---

## Risks

1. **Rust not installed** — needs `rustup`; first `cargo build` of a Tauri app is 10–20 min. Biggest schedule risk; do it first.
2. **Ollama not installed** — handled by the existing installer, but it downloads once (needs internet before going air-gapped).
3. **Python 3.13/3.14 only on the box** — sidecar pins 3.12 via `uv python pin` for wheel coverage.
4. **VRAM detection is unreliable** on non-NVIDIA / Windows — RAM is the primary signal, VRAM a bonus; manual override always available.
5. **"AES-GCM via Fernet" in the blueprint is contradictory** (Fernet = AES-128-CBC + HMAC). Plan follows `Fernet`; swap to `hazmat.AESGCM` if literal GCM is required. Machine-derived key is obfuscation-grade, documented.
6. **First-run model/weight downloads** (faster-whisper, rapidocr, moondream) — pre-cache for the air-gapped demo (Phase 5).

---

## Suggested division of work

| Area | Owner | Depends on |
|---|---|---|
| Phase 0 setup + `sidecar.rs` + `hardware.rs` + `BootstrapGate` | — | nothing |
| Phase 1 `core/` orchestration (engine, schemas, registry, planner) | — | Phase 0 |
| Phase 2 `tools/` + executor + quality + compiler | — | Phase 1 schemas + registry |
| Phase 3 `core/memory.py` | — | Phase 1 engine |
| Phase 4 React UI (Stepper, HITL, Report, Audit) | — | Phase 1 SSE contract |

`core/schemas.py` + the `StepEvent` shape + the FastAPI route list are the contract — freeze those early so UI and pipeline can move in parallel.

---

## Open questions for the team

1. Demo scenario details for SIH26117 — which refinery artefacts do we stage (inspection PDF, valve/corrosion photos, a voice field-note)? Need 3–5 sample files + 5–10 seed SOP docs for `kb/`.
2. Ship as a frozen sidecar (PyInstaller `externalBin`) for the demo, or run the sidecar from a project-local venv? (Plan assumes venv for the prototype, PyInstaller as Phase 5 stretch.)
3. Literal `AES-GCM` required, or is `Fernet` acceptable?
4. Target demo machine spec — so we know which model tier actually runs on the day.

---

## How to run (once built)

```bash
git checkout -b feature/sovereign-workbench
rustup default stable                    # one-time
cd workbench && uv sync && cd ..         # Python sidecar deps
npm install                              # already done
npm run tauri dev                        # BootstrapGate installs Ollama + tier models, starts sidecar
pytest workbench                         # phase tests
```
