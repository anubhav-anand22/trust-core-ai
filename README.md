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

## Manual testing walkthrough

This is the exact sequence to reproduce what has been tested so far. Please
follow it in order — later steps assume earlier ones worked.

### 0. Before you start

- Ollama must be running (`ollama serve`, or as a Windows service). Check with
  `curl http://127.0.0.1:11434/api/tags`.
- Close anything heavy. Inference will use every core it is allowed to, and on a
  small machine you will feel it (see *Known problems* below).
- Generate the fixtures: `python demo/make_fixtures.py` (needs Pillow). You get
  `inspection.pdf`, `valve.png`, `nameplate.png`, `note.wav` in `demo/fixtures/`.

### 1. Bootstrap screen

`npm run tauri dev`. Expect:

- **"Checking for Ollama…"** → then straight to **"Detected hardware"**. If you
  instead see *"Installing Ollama…"* and it sits there, that is a bug we thought
  we fixed — please report it with your OS and how Ollama was installed.
- A hardware card: RAM, GPU name, CUDA yes/no, and a **tier label**.
  **Sanity-check this.** If you have no GPU it should say *"CPU-only"* and
  recommend `qwen2.5:1.5b-instruct`. If it recommends a 7B model to a machine
  with no GPU, that is a regression — tell us your specs.
- **Let it use the recommended model.** Overriding to something bigger on a
  CPU-only box is exactly the mistake that made the app unusable for us.
- Click **Download & start**, watch the pull progress, then it hands over.

### 2. One normal turn

Prompt: *"Assess corrosion risk on pump P-101 and check it against our SOPs."*
Attach `demo/fixtures/inspection.pdf` and `demo/fixtures/valve.png`. Run it.

What should happen:

- The **stepper** advances: parsing context → validating plan → each tool in
  order → quality check → synthesising → done.
- The **activity list** shows each tool with its elapsed time.
- The **report** shows a summary, findings, and **citations naming the SOP files**
  it retrieved (`SOP-COR-014…`, `SOP-ROT-007…`). Citations are the thing to check
  hardest — they are the evidence retrieval actually worked.
- `degraded: false` unless you are missing the whisper/OCR model files.

**This will take minutes, not seconds, on a CPU-only machine.** It is slow, not
hung. See *Known problems*.

### 3. The interesting test: a document with a specific fact buried in it

This is where we currently have a **known failure we are not sure is fixed**.
Attach a long PDF (a service agreement, a rate card — anything with a specific
number deep inside it) and ask a question that requires finding that number and
doing something with it. For example: *"What transaction fee applies to a ₹1000
credit card payment?"*

- **Good outcome:** it quotes the rate exactly and does the arithmetic.
- **Bad outcome:** *"no relevant information available"* — which is the bug we
  hit. If you see this, please grab the PDF (or say what kind it is) and report
  it. We need to distinguish two different causes: the text not fitting in the
  model's context window (believed fixed), versus `pdfplumber` not being able to
  extract the text from that particular PDF at all (not fixed, and a different
  problem entirely).

### 4. Human-in-the-loop (least-tested code path)

Give it something that cannot produce a valid plan — a vague prompt with no
attachments, e.g. *"compare everything"*. After exactly two silent retries you
should get a modal with the validator's complaints and an editable plan.

Test all three buttons: **Re-run with this plan** (re-validates), **Run anyway**
(skips validation), **Dismiss**. This was wired recently and **has never been
exercised through the GUI** — treat anything odd here as expected and report it.

### 5. Audit sidebar and memory

- Expand the audit sidebar: per-step timings, active model, and the resolved
  on-disk paths (`lancedb/`, `session_context.json`, `persistent_memory.json`).
- Run 2–3 turns, close the app, reopen. The knowledge base should **not** re-ingest
  (it is cached), and long-term memory should reload.
- `persistent_memory.json` should be unreadable ciphertext on disk — worth
  opening in a text editor to confirm.

### What to report back

For anything that misbehaves, the useful details are: which step, the elapsed
times from the activity list, whether the report said `degraded`, and — if the
answer was wrong — what the source document actually contained. Running the
headless `--example e2e` (bottom of this file) prints every step event and is
usually the fastest way to see where a turn went wrong.

---

## Known problems and open questions

Please read this before concluding something is broken — and please do brainstorm
on any of it.

### 1. CPU-only performance is the central unsolved problem

On a 4-core machine with no GPU, a full turn took roughly **8 minutes**, with the
two analysis steps (`summarize`, `compare_to_sop`) at ~200s *each*. We have since
capped output length, shrunk prompts via retrieval, and stopped recommending
oversized models — but **the improvement is not yet measured**, and the physics
does not change: local inference on 4 CPU cores is slow.

Open questions worth thinking about:
- Are two separate analysis steps justified, or should `summarize` and
  `compare_to_sop` collapse into one model call?
- Should the vision step be opt-in? `moondream` cost ~48s for a photo.
- Is there a smaller viable resident model than `qwen2.5:1.5b-instruct`?
- Would streaming partial output make the wait *feel* acceptable even if the
  total time does not change?

### 2. Long documents may still fail (unverified fix)

Symptom: asked about a fee buried deep in a PDF, the model answered *"no relevant
information available"*. Diagnosis: the entire document was being stuffed into a
4096-token context window, so the relevant page was silently truncated away. Fix:
the evidence is now chunked, embedded, and only passages relevant to the question
are sent. **This has not been re-tested against the original failing document.**

There is a second possible cause we have not ruled out: `pdfplumber` may simply
fail to extract text from some PDFs. Which of the two it was, we do not yet know.

### 3. Scanned PDFs are not supported, by design

`pdfplumber` reads digital text only. A scanned or image-only PDF reports "no
extractable text" rather than being OCR'd. This was a deliberate scope decision,
but it is a real limitation for a document-heavy demo.

### 4. Small models write bad plans

`llama3.2:3b` has been observed to:
- copy the task registry's formatting into the task name (`"parse_pdf [Extract]"`),
- hallucinate tasks that do not exist (`"ochrage_image"`),
- invent attachments that were never provided.

We added name normalisation and hardened the planner prompt, and it now usually
validates on the first attempt — but the retry-then-HITL path exists precisely
because this is not reliable. **`qwen3:4b` is worse, not better**: it returns an
empty response under structured-output constraints (a thinking-model
incompatibility) and should not be used as the resident model.

### 5. The app could make the whole desktop unresponsive

Ollama took all cores, and whisper.cpp and the OCR runtime each spawned their own
unbounded thread pools on top. On a 4-core host this froze the Windows shell
(dead volume/wifi flyouts) and stuttered video calls. Every path is now capped to
`cores - 1`. **Please stress-test this** — run something in the background during
a turn and see whether your machine stays usable.

### 6. `resume_turn` has never been exercised through the GUI

The HITL resume path compiles, is type-checked, and shares its execution tail with
the normal path, but nobody has clicked those buttons in a running app. See
walkthrough step 4.

### 7. Build environment fragility

- **`cargo test` must use `-j 1`** or it fails with `E0463` and poisons the next
  run. Explained under *Gotchas*.
- Alternating cargo subcommands re-resolves features for the `lance-*` crates and
  forces a full recompile. Pick one and stay on it.
- `target/` reached **23 GB** before we trimmed dependency debuginfo; it is ~12 GB
  now. A build did once fail outright with "no space on device".
- `lancedb 0.38.0` needs `features = ["remote"]` to compile at all — a workaround
  for a bug in that release, not something we want.

### 8. Not started

Phase 5: packaging into an installer, bundling the whisper/OCR weights, and the
true air-gapped test (disconnect networking, repeat the demo). Also no
`.gitattributes`, so line endings are noisy across machines.

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
