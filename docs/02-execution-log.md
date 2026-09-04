# 02 · Execution log — how this was built

This is the honest narrative: the order things happened, the decisions, and the
walls hit along the way. If you're learning, the walls are the useful part —
real projects are mostly walls.

## Starting point

The repo already had a **Tauri + React + TypeScript scaffold** with some real
work in it: `ollama-rs` wired up, a multi-OS Ollama installer, model-pull with a
progress bar, and a couple of test buttons. Two commits: `init` and
`test buttons`.

There was also a design blueprint (`~/Downloads/gemini-code-…md`) describing a
**Python + Streamlit + FastAPI sidecar** architecture.

## Decision 1 — language and shape

The blueprint said Python. The repo was Rust + a desktop app. Those don't
combine cleanly. We compared:

| | Python sidecar | Pure Rust in the app |
|---|---|---|
| ships an interpreter | yes (or a fragile PyInstaller bundle) | no |
| dependency weight | ~500–800 MB of packages | one binary + 1–2 native libs |
| every capability has a mature library | yes | **also yes** — every Python lib had a Rust crate |
| packaging for an air-gapped machine | hard | easy |

Every Python tool had a Rust equivalent: `pdfplumber` (Python) → `pdfplumber`
(Rust crate); whisper.cpp → `whisper-rs`; Tesseract → `ocrs`; ChromaDB → a Rust
vector store; `cryptography` → RustCrypto. So the call was **pure Rust**, and
delete the sidecar idea entirely. The UI would talk to the logic through Tauri's
built-in `invoke` + `Channel`, not HTTP.

Locked with the user via a few questions: all 5 tool modules, hardware-detected
model tier, `pdfplumber` crate only (no OCR-fallback for scanned PDFs), `ocrs`
for image OCR, `whisper-rs` for audio.

## Decision 2 — workspace layout

Split into two crates:

- `crates/workbench-core` — the pipeline, no Tauri.
- `src-tauri` — the desktop host.

A root `Cargo.toml` ties them into a **workspace**. This is why `target/` moved
from `src-tauri/target/` to the repo root — which later broke Vite (see below).

## Phase 0–1 — the core, and the first long build

Wrote the whole orchestration core: `schemas.rs` (the typed contract),
`registry.rs` (the task table + `validate_plan`), `planner.rs` (the retry loop),
`executor.rs`, `quality.rs`, `compiler.rs`, both memory modules, and 8 tests.

**Wall: the first `cargo check` took ~11 minutes.** On this Windows box, the
combination of Windows Defender scanning every build artifact + `rust-analyzer`
(the editor's background type-checker) running its *own* `cargo check` in
parallel — both fighting for the single lock on `target/` — roughly doubled
build times.

Fix (`.vscode/settings.json`, gitignored):
```json
{
  "rust-analyzer.checkOnSave": false,
  "rust-analyzer.cargo.targetDir": true
}
```
`targetDir: true` gives rust-analyzer its *own* `target/` directory so it never
contends with terminal builds. After this, incremental builds dropped to
~10–40 seconds.

Also removed an over-aggressive setting I'd added:
```toml
[profile.dev.package."*"]
opt-level = 3        # optimise ALL dependencies even in dev — too slow to compile
```
Deleted it. `opt-level = 0` (the default) for dev iteration; specific slow
crates can be bumped individually later.

Result: `workbench-core` compiled clean, 13 tests green. The only compile
surprise was one deprecation warning (`Ollama::new` → `Ollama::builder()`).

## Phase 2 — the tools, and crate-API archaeology

Added `pdfplumber`, `ocrs`, `rten`, `whisper-rs`, `symphonia`, `image`. This is
where you spend time reading `docs.rs` and guessing signatures.

**Wall: `whisper-rs-sys` failed to build — "is `cmake` not installed?"** CMake
*was* installed (via `winget`), but into the *machine* PATH, and this shell had
started before that. Same story for Rust and Ollama earlier. Lesson: on Windows,
"installed" and "on this process's PATH" are different things. Fix: prepend the
real locations to `PATH` and set `LIBCLANG_PATH` for the `whisper-rs` bindgen
step:
```bash
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:$PATH"
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"
```

**Wall: three real compile errors in the tool code**, all from guessing crate
APIs wrong:

1. `pdfplumber`'s `page.extract_text()` returns `String`, not `Option<String>`
   — I'd written `if let Some(t) = …`.
2. `whisper-rs` 0.16 renamed segment access — no `full_get_segment_text`; use
   `state.as_iter()` yielding `WhisperSegment` with `.to_str_lossy()`.
3. `WhisperContext::new_with_params` wants `AsRef<Path>`, not `Cow<str>` — pass
   the `&Path` directly.

All three are the normal cost of binding to libraries whose docs are thin. Fixed
in one pass; `cargo check` then clean.

## Phase 3–4 — memory wiring and the UI

Wired `persist_long_term()` into `run_turn` (encrypt the session digest every
turn, so a crash never loses history) and added an `end_session` command.

Wrote the React frontend: `BootstrapGate`, `PromptPanel`, `Stepper`,
`HitlModal`, `ReportView`, `AuditSidebar`, plus `lib/pipeline.ts` (typed wrappers
over every Tauri command) and `types.ts` (a TypeScript mirror of the Rust
`StepEvent` enum).

**Wall: `npm run tauri dev` aborted with `EBUSY: resource busy or locked, watch
'…/target/debug/deps/tauri_app_lib.dll'`.** Vite watches the project directory
for file changes. The scaffold's `vite.config.ts` ignored `src-tauri/**` — but
with the workspace, `target/` is now at the *repo root*, unignored, so Vite was
trying to watch thousands of build artifacts and choked on a locked DLL
mid-build. Fix:
```ts
watch: { ignored: ["**/src-tauri/**", "**/target/**", "**/crates/**/target/**"] }
```

After that, `cargo tauri dev` built (~43 s incremental) and **the window
launched**. It also auto-ran the Ollama installer, because the spawned
`tauri-app.exe` inherited the stale PATH and `is_ollama_installed()`'s
`where ollama` came back empty — a dev-session artifact, not a bug.

## The real fix — structured outputs

First full end-to-end run against live models:
```
idle → parsing_context → validating_plan ×2 → ERROR:
  model returned malformed JSON for role `task_planner`:
  invalid type: map, expected a string
```

`llama3.2:3b` is a small model. Asked for JSON with plain `format: "json"`, it
would put an object where the schema wanted a string (e.g. `depends_on`
entries). Two retries didn't save it.

The fix is **structured outputs**: give Ollama the actual JSON *schema* and it
constrains generation so the shape can't be wrong.

```rust
// schemas.rs — derive a schema from the Rust type
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Plan { pub steps: Vec<TaskStep> }

// ollama_engine.rs — send that schema with the request
.format(FormatType::StructuredJson(Box::new(JsonStructure::new::<T>())))
```

`schemars` turns the Rust type into a JSON Schema at compile time;
`ollama-rs` forwards it to Ollama. Now the model *cannot* emit `depends_on: [{}]`
— the schema says `array of string`. The retry loop is left to fix only the
things a schema can't express: "you scheduled analysis before extraction",
"that task isn't in the registry".

Re-ran: full pipeline green. `search_knowledge → parse_pdf → analyze_image →
compare_to_sop → quality pass → report citing both SOPs`. A second run with an
audio file exercised `whisper-rs` loading `ggml-base.en.bin` and transcribing.

## Post-Phase-4 — the RAG store, take two

The first RAG implementation (`tools/rag.rs`) was a **hand-rolled cosine loop**
over a JSON file: ~40 lines, zero new dependencies, exact results, built and
iterated fast. It was a deliberate call to keep the build loop short while
everything *else* was still moving.

Once the pipeline was stable it was swapped to **embedded LanceDB** — the store
you'd actually ship. That change:

- added `lancedb`, `arrow-array`, `arrow-schema`, `futures` to `Cargo.toml`
  (LanceDB pulls Apache Arrow + DataFusion — **~238 transitive crates**, one very
  long cold build),
- rewrote `rag.rs` to build an Arrow `RecordBatch` `{id, text, source, vector}`,
  `create_table("sop_kb", vec![batch]).mode(Overwrite)`, and query with
  `table.query().nearest_to(vec)?.distance_type(Cosine).limit(k)`,
- kept `chunk_text` and `embed` unchanged, and kept the whole thing behind two
  functions (`ensure_ingested`, `search`) so nothing else in the crate moved.

Why the round trip instead of LanceDB from day one: see
[doc 06](06-rag-and-why-not-a-vector-db.md#why-this-was-originally-a-hand-rolled-cosine-loop).
Short version — at a few-hundred-chunk KB the two are functionally identical, and
during heavy iteration a 15-minute build tax on every fix isn't free.

**Two walls on the swap itself:**

1. **`lancedb 0.38.0` won't compile with default features.** Its `src/job.rs`
   references `Error::Http`, an enum variant that only exists under the `remote`
   feature — but `default = []`. A 4-day-old release bug. Workaround in
   `Cargo.toml`: `lancedb = { version = "0.38", features = ["remote"] }` (pulls
   `reqwest`/`http`, already in the tree via `ollama-rs`, so no real cost).

2. **`cargo test` deadlocked itself in a rebuild loop.** With LanceDB in the
   graph, `workbench-core`'s own `.rlib` balloons to ~73 MB (all the Arrow +
   DataFusion generics monomorphised in). Under a parallel build, the integration
   test binaries would start linking against that `.rlib` while it was still being
   written and fail with `E0463: can't find crate for workbench_core` /
   `crate ... required to be available in rlib format` — which aborted the run,
   left the fingerprint dirty, and guaranteed the same failure next time.
   `resolver = "2"` made it worse: `cargo build --lib` and `cargo test` resolve
   *different* feature sets for `lance-*`, so each re-triggered the other's
   rebuild. **Fix: `cargo test -p workbench-core -j 1`.** Serialising compilation
   means every `.rlib` is finished before anything links it. Slower, deterministic.

## Post-swap — one more planner fix

Verifying the LanceDB swap end-to-end (`--example e2e`) kept parking at the HITL
gate: `llama3.2:3b` was emitting task names like `"parse_pdf [Extract]"` — it had
copied the registry listing's `name [Stage]` rendering verbatim into the `task`
field, so every step failed the registry lookup. Two fixes, both small:

- **`registry_prompt_block()`** now renders `- "parse_pdf"  (stage: extract; …)` —
  the name is the only quoted token on the line, nothing glued to it.
- **`normalize_task_name()`** strips surrounding quotes/backticks and a trailing
  ` [Stage]` before lookup, and `build_plan` folds the plan's task names through
  it, so a near-miss resolves instead of burning a retry.

After that the e2e ran clean on the first plan attempt: 6 tools in order,
`search_knowledge` hitting LanceDB, a grounded report citing `SOP-ROT-007`,
`degraded: false`.

## Where it ended

Phases 0–4 done, RAG on LanceDB. `cargo test -p workbench-core -j 1` green (20).
`tsc` clean. Headless e2e green (report cites a retrieved SOP).
`feature/sovereign-workbench`.

### Takeaways for an aspiring engineer

- **Most of the time went to the environment, not the logic.** PATH, build
  locks, watcher config, crate-API guessing. Budget for that.
- **Separate "the logic" from "the app" early.** Being able to run the pipeline
  headlessly (`--example e2e`) made every later problem debuggable.
- **When a small model won't behave, constrain it, don't fight it.** Structured
  outputs solved in one edit what better prompting wouldn't have.
- **Commit in phases with passing tests.** Every commit here builds and tests
  green, so `git bisect` would actually work.
