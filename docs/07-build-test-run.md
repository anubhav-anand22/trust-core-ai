# 07 · Build, test, run

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| Rust (MSVC toolchain) | everything | `winget install Rustlang.Rustup` |
| VS 2022 Build Tools (C++ workload) | the linker; whisper.cpp | `winget install Microsoft.VisualStudio.2022.BuildTools` + "Desktop development with C++" |
| CMake | builds whisper.cpp | `winget install Kitware.CMake` |
| LLVM | `libclang` for `whisper-rs`'s bindgen step | `winget install LLVM.LLVM` |
| Node 20+ | the frontend | `winget install OpenJS.NodeJS` |
| Ollama | runs the LLMs | `winget install Ollama.Ollama` |

## Environment (every shell before `cargo`)

On this Windows box the installers add themselves to the *machine* PATH, which an
already-open shell won't see. Prepend them, and point bindgen at libclang:

```bash
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:$PATH"
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"
```

(The GUI app doesn't need these — it finds Ollama at its known install path.)

## First build — expect it to be long

| Command | Cold | Incremental |
|---|---|---|
| `cargo test -p workbench-core -j 1` | ~15–25 min (arrow + datafusion + whisper.cpp) | ~30–60 s |
| `cargo check --workspace --all-targets` | ~25 min | ~10 s |
| `npm run tauri dev` (first) | ~25 min | ~40 s + instant HMR for the frontend |

Slowness on this machine is Windows Defender scanning every artifact. `rust-analyzer`
is configured (`.vscode/settings.json`) to use its own `target/` so it doesn't
fight terminal builds.

> **Use `-j 1` for `cargo test`.** Since LanceDB joined the graph, `workbench-core`'s
> `.rlib` is ~73 MB (Arrow + DataFusion generics). Under a parallel build the test
> binaries try to link it mid-write and fail with `E0463: can't find crate for
> workbench_core` or `crate ... required to be available in rlib format`; the run
> aborts and the next one fails the same way. `-j 1` serialises compilation so
> every `.rlib` is complete before anything links it. Also: pick **one** cargo
> subcommand and stick to it — `cargo build --lib` and `cargo test` resolve
> different feature sets for `lance-*` (resolver v2), so alternating them forces a
> full recompile each time.

## Ollama models

```bash
ollama pull llama3.2:3b        # or qwen3:4b — a small resident LLM for testing
ollama pull moondream          # vision
ollama pull nomic-embed-text   # embeddings
```

`recommend_models()` will suggest `qwen2.5:7b-instruct` on a capable machine; the
`BootstrapGate` dropdown lets you override to whatever you've pulled.

## Optional tool models (audio + OCR)

The app degrades gracefully without these (the step reports "model not
installed"). To enable them, drop the files into
`%APPDATA%\com.anubhav_anand.tauri-app\models\`:

| File | Source |
|---|---|
| `ggml-base.en.bin` | `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin` |
| `text-detection.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten` |
| `text-recognition.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten` |

## Run the tests

```bash
cargo test -p workbench-core -j 1
```
- `registry.rs` — `validate_plan` rejects each bad-plan class; `normalize_task_name`
  tolerates a stage-labelled task name
- `planner_retry.rs` — the retry ceiling, using a mock `PlanSource`
- `tools_smoke.rs` — registry completeness, tool file-guards
- inline: `memory/persistent.rs` (encryption round-trip), `tools/rag.rs` (chunking)

None of these need Ollama.

## Run the pipeline headlessly (needs Ollama running)

```bash
python demo/make_fixtures.py     # writes demo/fixtures/  (needs: pip install pillow)

cargo run -p workbench-core --example e2e -- \
  "Assess corrosion risk on pump P-101 and check it against our SOPs." \
  demo/fixtures/inspection.pdf demo/fixtures/valve.png
```

It prints every `StepEvent` and the final report JSON. Overrides:
`WB_LLM`, `WB_VISION`, `WB_EMBED`, `WB_MODELS_DIR`, `WB_KB_DIR`, `WB_DATA_DIR`.

## Run the GUI

```bash
npm install        # once
npm run tauri dev
```

The window opens on `BootstrapGate`. Override the LLM to a model you've pulled,
click **Download & start**, then attach files and run an inspection.

## Where files live at runtime

`app_data_dir` = `%APPDATA%\com.anubhav_anand.tauri-app\` (Windows). Inside it:

| Path | What |
|---|---|
| `uploads/<session>/` | files you attached this session |
| `lancedb/sop_kb/` | the embedded vector table (KB chunks + vectors) |
| `lancedb/sop_kb.model` | sidecar: which embedding model built the table |
| `session_context.json` | this session's rolling context (plain JSON) |
| `persistent_memory.json` | long-term memory (**AES-256-GCM ciphertext**) |
| `models/` | `ggml-base.en.bin`, `*.rten` (if you added them) |

The `AuditSidebar` in the GUI shows these resolved paths live.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `is cmake not installed?` during build | CMake not on this shell's PATH — see [Environment](#environment-every-shell-before-cargo) |
| `Unable to find libclang` | set `LIBCLANG_PATH` (see above) |
| `cargo tauri dev` exits with `EBUSY … tauri_app_lib.dll` | stale Vite watcher config — `vite.config.ts` must ignore `**/target/**` (already fixed) |
| `Blocking waiting for file lock on build directory` | another `cargo` (often rust-analyzer) is building. Wait, or kill `rust-analyzer` |
| planner errors `invalid type: map, expected a string` | structured outputs not applied — the output type must `#[derive(JsonSchema)]` and the request must use `FormatType::StructuredJson` |
| `search_knowledge` fails with a VectorStore error | Ollama not running, or `nomic-embed-text` not pulled |
| audio/OCR steps report "model not installed" | expected without the optional model files; the run continues `degraded` |

Next: [08 · Extending it](08-extending.md).
