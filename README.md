# Sovereign On-Premise AI Workbench

An air-gappable desktop assistant for industrial inspection work. You give it a
question plus attachments (a digital PDF, a photo, a voice note); it parses your
intent, builds an execution plan, **validates that plan deterministically in Rust
rather than asking an LLM to grade itself**, runs lightweight tools one at a time,
and synthesises a report with citations back to your standing SOPs.

Everything runs locally against [Ollama](https://ollama.com). No cloud calls, no
Python runtime, no interpreter to ship — one Rust binary plus a webview.

> SIH 2026 · PS SIH26117 (Mangalore Refinery & Petrochemicals) · branch
> `feature/sovereign-workbench`

---

## Status

Phases 0–4 are done and the pipeline runs end to end. Phase 5 (packaging into an
installer) has not started. See [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)
for the phase table and the deliberate deviations from the original blueprint.

---

## Prerequisites

Install these **before** cloning. On Windows the C toolchain pieces are not
optional — `whisper-rs` compiles whisper.cpp from source and needs them.

| Tool | Why | Windows |
|---|---|---|
| Rust (stable, MSVC) | everything | `winget install Rustlang.Rustup` |
| CMake | builds whisper.cpp | `winget install Kitware.CMake` |
| LLVM / libclang | `whisper-rs` bindgen | `winget install LLVM.LLVM` |
| Node 20+ | the frontend | `winget install OpenJS.NodeJS` |
| Ollama | local inference | `winget install Ollama.Ollama` |

`winget install` puts these on the **machine** PATH, which an already-open shell
will not see. Open a fresh terminal afterwards, or export them:

```bash
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:$PATH"
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"
```

**Budget ~15 GB of free disk and 45–60 minutes for the first build.** The
dependency graph is ~400 crates (Apache Arrow + DataFusion + LanceDB + Tauri).
Later builds are seconds.

---

## Setup

```bash
git clone https://github.com/anubhav-anand22/ai-local-tauri-ollama.git
cd ai-local-tauri-ollama
git checkout feature/sovereign-workbench

npm install
ollama serve                  # if it is not already running as a service
npm run tauri dev             # first run: long. be patient.
```

The app opens on a bootstrap screen that probes your hardware, recommends a
model tier you can actually run, pulls what is missing, and hands over to the
workbench. **Let it pick the model** — the recommendation accounts for whether
you have a usable GPU, and a machine without one needs a much smaller model than
its RAM alone would suggest.

To pre-pull manually:

```bash
ollama pull qwen2.5:1.5b-instruct   # CPU-only hosts
ollama pull llama3.2:3b             # or this, if you have cores/GPU to spare
ollama pull moondream               # vision
ollama pull nomic-embed-text        # embeddings (required for retrieval)
```

### Optional tool models

Audio and OCR degrade gracefully without these — the step reports that the model
is missing rather than failing the turn. To enable them, drop the files into
`%APPDATA%\com.anubhav_anand.tauri-app\models\`:

| File | Source |
|---|---|
| `ggml-base.en.bin` | `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin` |
| `text-detection.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten` |
| `text-recognition.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten` |

### Demo fixtures

Not committed (they are generated). To recreate them:

```bash
python demo/make_fixtures.py    # needs Pillow
```

---

## Gotchas that will cost you an hour if you skip them

**Run tests with `-j 1`.**

```bash
cargo test -p workbench-core -j 1
```

`workbench-core`'s compiled library is large (all the Arrow/DataFusion generics
land in it). Under a parallel build the integration-test binaries try to link it
while it is still being written and fail with `E0463: can't find crate for
workbench_core`. That aborts the run and leaves the fingerprint dirty, so the
next run fails identically. `-j 1` serialises compilation so every `.rlib` is
finished before anything links it.

**Pick one cargo subcommand and stay on it.** `cargo build --lib`, `cargo test`
and `cargo check --all-targets` resolve *different* feature sets for the
`lance-*` crates under resolver v2. Alternating between them forces a full
recompile of the whole tree each time.

**Let rust-analyzer use its own target directory**, or it will run a second
`cargo check` that contends with your terminal builds and roughly doubles build
times. `.vscode/settings.json` in this repo sets that up.

---

## Documentation

The [`docs/`](docs/) folder is written for someone learning the codebase, not
just running it:

| | |
|---|---|
| [README](docs/README.md) | index + glossary of every term used |
| [01 Overview](docs/01-overview.md) | what the system is, the shape of one turn |
| [02 Execution log](docs/02-execution-log.md) | how it was built, including the walls hit |
| [03 Codebase tour](docs/03-codebase-tour.md) | every module and why it exists |
| [04 Agentic pipeline](docs/04-agentic-pipeline.md) | one real turn traced event by event |
| [05 Rust for this codebase](docs/05-rust-for-this-codebase.md) | 15 concepts tied to concrete lines |
| [06 RAG and the vector store](docs/06-rag-and-why-not-a-vector-db.md) | embeddings, cosine, the LanceDB store |
| [07 Build / test / run](docs/07-build-test-run.md) | commands, env, expected times |
| [08 Extending](docs/08-extending.md) | adding a tool, model, event, or KB doc |

---

## Layout

```
crates/workbench-core/   the pipeline. no Tauri, unit-testable.
  engine/                one resident Ollama model; roles A/B/C are prompt swaps
  planner/               TASK_REGISTRY + deterministic validate_plan + retry ceiling
  executor/              strict sequential tool loop, quality gate, report compiler
  tools/                 pdf · ocr · audio · vision · rag · analysis
  memory/                session context + AES-256-GCM long-term memory
  kb/                    seed SOP documents

src-tauri/               the desktop host: bootstrap, commands, event bridge
src/                     React frontend: bootstrap gate, stepper, HITL modal, report
```

## Running the pipeline without the GUI

Useful for debugging — needs Ollama running, prints every step event:

```bash
cargo run -p workbench-core --example e2e -j 1 -- \
  "Assess corrosion risk on pump P-101 and check it against our SOPs." \
  demo/fixtures/inspection.pdf demo/fixtures/valve.png
```
