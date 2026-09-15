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

## Post-demo — the "useless and slow" report, part 1: don't hang the machine

The first real hands-on test on a 4-core, 16 GB, no-GPU laptop went badly. Three
complaints: a wrong answer on a PDF (covered in part 2), turns that took minutes,
and — twice — the whole Windows desktop freezing mid-run: a stuck volume overlay,
a dead Start button, an unusable tray. A background build of *this project* later
reproduced the freeze exactly, which told us the cause was not the model at all.

### Why the desktop froze

Ollama runs as a **separate process**. Our pipeline caps how many CPU threads it
asks Ollama to use (`num_thread`), but nothing capped Ollama's *scheduling
priority*. On a 4-core box, inference at normal priority competes with the window
manager for cores, and the shell loses. The freeze outlives the run because the
shell's XAML hosts, once starved, draw an overlay and then never run the timer
that dismisses it.

Two more multipliers:

- **Logical vs physical cores.** `detect_hardware` read
  `std::thread::available_parallelism()` — hyperthreads. A 4-core/8-thread laptop
  reported `8`, which pushed it up a model tier *and* made `worker_threads()`
  hand out `7` threads for `4` real cores. Now `sysinfo::physical_core_count()` is
  the number every thread budget is derived from.
- **Two uncapped model calls.** `warm_model` (the first, heaviest load) and the
  RAG embedder both issued requests with no options block, so they ran at
  Ollama's default *unbounded* thread count. Both now carry the same ceilings as
  every other call.

### What changed

- **`bootstrap::ollama::lower_ollama_priority()`** — sets every `ollama*` process
  to below-normal priority, at startup and again after the model is warmed (the
  per-model runner is spawned lazily). Inference stays fast; the shell always
  wins a contested core.
- **A sized model catalogue.** `recommend_models` no longer compares core counts
  and RAM to bare thresholds. `LLM_CATALOG` carries each model's approximate
  resident RAM, download size and context window, and the tiers are gated on
  **physical cores** and **available** (not total) RAM:

  | Tier | CPU-only condition | Model | ~RAM |
  |---|---|---|---|
  | standard | ≥6 physical cores, ≥10 GB free | `qwen2.5:3b-instruct` | 1.9 GB |
  | floor | ≥4 physical cores, ≥5 GB free | `qwen2.5:1.5b-instruct` | 1.0 GB |
  | minimal | ≥2 physical cores, ≥3 GB free | `qwen2.5:0.5b-instruct` | 0.4 GB |
  | unusable | below that | (refuses, explains why) | — |

  Every model is an *instruct* model: the pipeline constrains decoding to a JSON
  schema, and a "thinking" model (`qwen3:4b`) emits its reasoning first and
  returns an empty body under that constraint. It was removed from the dropdown.

- **`assess_llm` + the `assess_model` command.** Overriding the recommendation
  used to produce only a log line the user never saw. Now the bootstrap screen
  shows the download size, and a warning strip: *caution* if the pick is much
  bigger than the tier ("expect turns ~2× slower"), *blocked* — with the Download
  button disabled — if it will not fit in available RAM, with the real numbers in
  the message.

- **A disk pre-flight** in `ensure_models`: it sums the download estimate for the
  missing models and refuses up front if the drive holding the app-data dir does
  not have that plus 2 GB. A build here once died with "no space on device" after
  a 20-minute partial download; this is that failure caught early.

- **Timeouts.** Every Ollama call now goes through `OllamaEngine::guarded`, which
  races the request against a wall-clock deadline (`ResourceLimits::call_timeout`,
  240 s by default — generous, because CPU inference of a long prompt is *slow*,
  not stuck) and a poll of the cancel flag. A call that blows the deadline is
  abandoned as `CoreError::Timeout`; the step degrades and the user gets a
  warning strip, instead of the turn hanging forever.

- **A Stop button.** `CancelFlag` is a shared `AtomicBool` — the one way into a
  pipeline that is otherwise one long `await`. It is polled before every model
  call and between every tool step, so Stop ends a turn *within* a step, not
  after it. Blocking tools (PDF, OCR, whisper) still finish their current call —
  nothing can safely kill a `spawn_blocking` mid-flight — but no further step
  begins. A user-pressed Stop is reported as a warning, not a red error.

- **`StepEvent::Warning`** — the pipeline had no way to tell the user anything
  short of a fatal error, so a degraded turn looked identical to a clean one.
  Warnings now render as a dismissible amber strip above the report.

### Deliberately not done

- **Killing a blocking tool mid-call.** `spawn_blocking` tasks (whisper, OCR,
  pdfplumber) cannot be safely aborted from outside. The cancel check between
  steps is the pragmatic compromise; a turn cancelled during a 40-second OCR
  still waits out that OCR.
- **GPU layer offload tuning (`num_gpu`, `low_vram`).** Real value for low-VRAM
  machines, but this milestone was about the CPU-only floor. Left for later.
- **A hard per-*turn* timeout.** Only per-*call* timeouts exist. A pathological
  plan with many slow steps could still run long — but each step is now bounded
  and the user can Stop, which covers the real case.

### Takeaway

When something "hangs the computer", separate *your* process from the ones it
talks to. The model was never the freeze; an unpriotised sibling process was.
And size a thread pool off physical cores — hyperthreads flatter the number and
punish the scheduler.

## Post-demo — the "useless and slow" report, part 2: the answer that was never there

The other half of the bad demo: a PayU rate card was attached and the ask was
"what transaction fee on a 1000 INR credit-card payment?". The number — 2% — was
in a table. The model said *"no relevant information available."*

The README's guess was context-window truncation. It was wrong, or at least
incomplete. A read-only trace found **four** places the figure could be lost, and
the first one is the whole story:

### The tables were extracted, then thrown away

`parse_pdf` puts its tables in `data["tables"]`. A repo-wide grep for that field
returns exactly one hit — a string in a prompt. **No code ever read it.** The
analysis step's `collect()` did `data["text"]` and returned; tables never entered
the evidence. Every table in every PDF had been silently dropped since the tool
was written.

### And three multipliers

- **Lattice-only table detection.** `TableSettings::default()` is
  `Strategy::Lattice`, which needs ruled lines. A zebra-striped or whitespace
  rate card yields *zero* tables. Now: try Lattice, fall back to `Strategy::Stream`
  (text-alignment) per page when it finds nothing.
- **Flat text extraction.** `TextOptions::default()` has `layout: false`, so a
  row collapses to `"Credit card 2.00 0.00 18%"` — no columns, an ambiguous
  number soup. Now `layout: true`, which keeps the whitespace grid.
- **Role C still dumped the whole document.** Commit `5c0a3b6` narrowed the
  *analysis* step's input via retrieval but left `compile_report` doing
  `serde_json::to_string_pretty(results)` — the entire raw text, into an 8k
  window. And `serde_json` runs with `preserve_order` in this build, so the giant
  `text` field serialised *before* `tables`, making the tables the first thing
  truncation ate. Now role C renders each result to readable text (capped per
  result), tables included, and runs it through the same retrieval narrowing.

### Retrieval got three fixes of its own

- **Chunking split decimals.** The splitter broke on `
` *or* `.`, and in a
  numeric table the last `.` before the window edge is usually a decimal point —
  `2.00%` became `"… 2."` + `"00% …"`. Now it breaks only on line boundaries, so
  a rendered table row stays whole.
- **Half the context window was unreachable.** `select_relevant` capped at
  `top_k` (8) chunks × 1100 chars ≈ 8.8k, against a 16k budget. `top_k` is a
  floor now; it fills the budget.
- **No embedding task prefixes.** `nomic-embed-text` is trained to receive
  `search_query:` on queries and `search_document:` on passages and is
  measurably worse without them. Both are applied now; the KB rebuilds once to
  re-embed with the scheme.

### And the model was being told the question was out of scope

Every role prompt said *"offline industrial inspection assistant"*, three times.
Hand that a payments fee schedule and ask for `safety_notes` and SOP clauses, and
*"no relevant information"* is a very natural completion. The framing is now
"analysis assistant … industrial *and* business documents", role C is told to
quote any rate/fee/percentage verbatim and show the arithmetic, and `analyze()`
finally has a real `.system(...)` instead of that instruction tacked onto the end
of the user message.

### Fast vs Deep

Two modes now, picked with a checkbox:

- **Fast** (default): the retrieval path above. Seconds; the right passage or
  nothing.
- **Deep**: `windows()` splits the whole document into gap-free windows, the
  analysis instruction is mapped over every one, and the partials are reduced
  into one answer. Nothing is skipped, at one model call per window — minutes on
  CPU, so each window emits a progress event and the cancel flag is checked
  between them.

### A quality check that could have caught it

`quality.rs` only checked that `text` was non-empty, so *"No relevant
information."* passed every gate. New check: if `parse_pdf` produced tables, their
flat rendering must be non-empty — a `render_tables` regression fails the run
loudly instead of silently dropping every figure.

### Deliberately not done

- **Rebuilding pdfplumber's table detection.** Stream is a real improvement but
  a genuinely adversarial layout (nested cells, multi-line cells) will still
  defeat it. The escape hatch stays Deep mode, which reads the raw text.
- **A dedicated table-QA fixture in CI.** `demo/make_fixtures.py` now emits a
  borderless `fee_card.pdf`, and there are unit tests on `render_tables`,
  `chunk_text` and `collect()` — but a full extract-to-answer assertion needs a
  running Ollama, so it stays a manual check.

### Takeaway

"The model can't find X" has three very different causes — X was never extracted,
X was extracted but dropped before the prompt, or X was in the prompt but past
the truncation point. They need different fixes and the only way to tell them
apart is a log line at each hop. That is why `parse_pdf: extracted` now logs
`text_chars`, `tables`, `tables_text_chars` and `stream_fallback_pages`.

## Post-demo — part 3: many files, mixed types, one turn

"Analyse this audio, this photo and this PDF together" did not work, and not
where you would guess. Validation was fine; the results map was fine. The failure
was one line, repeated in four tools:

```rust
let file = ctx.first_file_of(FileKind::Image);   // ← the FIRST image, always
```

Attach three photos and the planner emits three `ocr_image` steps? All three read
**image #1**, three times, at triple the cost. Images 2 and 3 are never opened.
The helper that would have fixed it — `files_of` — was written when the trait was
designed and has *zero* call sites.

### The fix: name the file

- **`ToolContext::file_for(step, kind)`** resolves `step.args["file"]` (the
  planner names an attachment by its `original_name`) against the uploads, and
  falls back to `first_file_of` only when no name is given — so single-file turns
  still work with empty `args`. Matching is by **name, never path**: a
  hallucinated `/etc/passwd` resolves to `None`, not a file read. All four
  extraction tools now call it.
- **Role B's rules were rewritten.** They used to say *"Leave `args` as {}"* and
  *"schedule a task whose required file kind is in ATTACHED FILES"* — both of
  which push the model toward one step per *kind*. Now: *one extraction step per
  attached file*, each with `{"file": "<exact name>"}`, and the file list is
  rendered numbered with `name="…"` so copying the name is unambiguous.

### One bad file no longer blocks the good ones

`FileKind::Unknown` (a `.mov`, a `.zip`) used to become a validation error the
planner could never repair, so it burned every retry and parked the turn — one
unsupported attachment blocking four fine ones. Now `pipeline::screen_uploads`
drops unusable attachments *before* planning, with a `StepEvent::Warning` naming
each, and the turn proceeds on what remains. (A file missing from disk is still a
hard error — that means something is genuinely broken.)

### A plan can never dead-end now

When the model exhausts its retries, `plan_turn` builds a **deterministic
fallback**: one extraction step per attachment (`parse_pdf` / `transcribe_audio`
/ `analyze_image`), a `search_knowledge` step if intent flagged it, then
`summarize` (+ `compare_to_sop` when knowledge is in play). It is validated like
any other plan; only if *that* fails does the turn actually park in the HITL
modal. On a 0.5–1.5B CPU model this is the difference between "usually answers"
and "answers".

### Smaller fixes

- **`warn_uncovered_files`** — `validate_plan` counts nothing, so a 1-step plan
  "covers" fifty images silently. Now any attachment no step reads produces a
  warning.
- **Per-source evidence cap** (`collect()`): 12k chars per attachment, so one
  verbose PDF cannot crowd the audio and photos out of the ranking.
- **Upload de-duplication** (`persist_uploads`): `IMG 1.jpg` and `IMG_1.jpg`
  sanitise to the same on-disk name and used to overwrite each other, leaving two
  `InputFile`s pointing at one file. The on-disk name is now disambiguated; the
  user-facing `original_name` is untouched.
- **`ACCEPT` synced** with `FileKind::from_extension` (it was missing `.aac` /
  `.wma`) and the drop zone now screens by extension — it previously bypassed the
  picker's filter entirely.

### Deliberately not done

- **Video.** No `FileKind::Video`, no ffmpeg. A dropped `.mp4` warns and is
  skipped; the rest of the turn runs. Out of scope for now by decision.
- **`ocr_image` *and* `analyze_image` on every image.** The fallback picks
  `analyze_image` (visual condition) for a photo; a nameplate that needs OCR
  still needs the model to plan it, or a second turn.

### Takeaway

A helper written "for later" with no caller is a latent bug, not a convenience.
`files_of` sat unused while `first_file_of` quietly dropped the user's data in
four places. If you add the plural form, wire it the same day.

## Post-demo — part 4: it is a chat now, not a form

The app was a single-shot form: type a prompt, get one report, and the next
prompt wiped it. Three structural facts made a real conversation impossible:

- **One shared `session_context.json`.** The path had no session id in it, so
  every chat wrote the same file. Starting a new chat overwrote the old one, and
  a page reload minted a fresh id that orphaned whatever was there.
- **Only 200 characters survived per turn.** `TurnSummary` kept a truncated
  prompt and a truncated summary — findings, citations and safety notes were
  dropped — so a past chat could not be redrawn even if you found it.
- **The planner never saw the conversation.** Only role C got a session blob.
  A follow-up like *"and what about debit cards?"* was planned with zero context,
  and `validate_plan` then rejected any analysis step with no preceding
  extraction step, so the turn parked in the HITL modal with no answer.

### What changed

- **One file per session** under `sessions/<id>.json`, keeping the existing
  atomic temp+rename write. A one-time migration moves a pre-Stage-4
  `session_context.json` in on first launch so an existing user's last chat is
  not lost.
- **`SessionContext` now carries a full `exchanges` transcript** — prompt, the
  complete `FinalReport`, attachment names, timestamp — alongside the bounded
  `turns` + `rolling_summary` that feed the model. The transcript on disk is
  complete; only what the model sees is capped (still `MAX_VERBATIM_TURNS`, then
  compress).
- **The session is loaded *before* planning** and its `context_blob` is passed
  into role A and role B, not just role C. `plan_turn` gained `history` and
  `has_history` parameters.
- **`answer_followup`** — a new `Stage::Retrieve` task, valid as a lone step,
  that runs the transcript through the resident model. The planner is told to
  use it for a follow-up that needs no new files; the deterministic fallback
  emits it when there are no uploads, no knowledge need, and a conversation
  exists. A bare follow-up resolves instead of parking.
- **Commands** `list_sessions` / `load_session` / `delete_session` /
  `rename_session`, backed by `SessionContext::list` (a `read_dir` over
  `sessions/`, newest-updated first). No fs plugin, so the frontend cannot read
  those files directly — every listing goes through a command.
- **The UI is a chat.** `App.tsx` holds a transcript list with the prompt at the
  bottom; a new `SessionSidebar` lists past chats (click to reopen, rename,
  delete, New chat). The session id lives in `localStorage`, so a reload keeps
  the same conversation instead of orphaning it.

### Deliberately not done

- **LanceDB for sessions.** Its only pattern in this repo is
  `CreateTableMode::Overwrite` with a mandatory `vector` column — storing chats
  there would mean embedding every message. Plain per-session JSON matches the
  existing `session.rs` idiom and needs no model call to list.
- **Capping how many sessions are kept.** The sidebar and `sessions/` grow
  unbounded for now; pruning is a later concern.
- **Streaming the assistant's answer token by token.** The turn still lands as
  one report. The stepper and per-window progress cover "is it working"; live
  token streaming is a separate piece of work.

### Takeaway

"Add a session id to the filename" sounds like a one-line fix. It was — but it
sat behind two others (keep the whole transcript, show the planner the history)
that only became visible once the first was done. A feature that "does not work"
often has a stack of causes, and you find the second only after fixing the first.

## Post-demo — part 5: one input, three identical answers

One file, one prompt, **three identical report cards** on screen. The obvious
suspects — the model looping, the pipeline running three times, a stale event
listener — were all wrong. Worth writing down because the triage order is the
lesson, not the fix.

### Ruling out the backend first

The answer can only reach the screen through paths you can enumerate, so
enumerate them:

| Checked | Finding |
|---|---|
| `StepEvent::Done` construction sites | exactly one, `pipeline.rs:287` |
| the command's return value | `submit_turn` returns `Ok(())` and drops the report — not a second path |
| global events / multi-window fan-out | none. No `app.emit`, no `emit_all`, no `listen` anywhere: the transport is a `tauri::ipc::Channel` built fresh per invoke |
| model-call loops | the planner's ≤3 retries re-roll the **plan**, never the answer; the compiler's retry returns on first success; deep-read's per-window calls reduce to one `ToolResult` |
| the executor | one `ToolResult` per step, no step re-runs |

Ten minutes of grep, and three quarters of the search space is gone. The `3` in
`MAX_PLAN_ATTEMPTS` is a **coincidence** with the symptom — the kind of
coincidence that sends you down a two-hour dead end if you start from a hunch
instead of from the delivery paths.

### Cause 1 — a side effect inside a state updater

```tsx
setReport((r) => {
  if (r) setTranscript((prev) => [...prev, { … , report: r }]);
  return r;                       // ← setTranscript ran as a side effect
});
```

React treats the function you hand to a set-function as **pure** and is free to
call it more than once. `<React.StrictMode>` double-invokes it *on purpose*, in
dev, to make exactly this kind of impurity visible. Each invocation queued its
own `setTranscript(prev => [...prev, x])`, and every queued append is applied —
so one turn landed in the transcript more than once.

It was written that way for an understandable reason: `run()` has no local handle
on the report, which arrives asynchronously on the channel callback. Reaching for
it through `setReport` was the path of least resistance. The right way to carry a
value out of an async callback is a **ref** — `liveReport`.

> **Rule:** an updater passed to `setState` may read `prev` and return a new
> value. Nothing else. No `setOther(...)`, no logging, no fetch.

### Cause 2 — the live pane was never retired

`clearLive()` only ran at the *start of the next* turn. When a turn finished,
`busy` went false but `events` was still full, so `showLive` stayed true and the
live block drew the same prompt bubble and the same `ReportView` one more time,
on top of the transcript copies. Transcript copies + one live copy = three.

The underlying design error is **two places rendering one thing with no rule
about which owns it**. `commitTurn` now *moves* the result: appends to the
transcript, then clears `report` and `livePrompt`.

### The trap in the obvious fix

"Clear the live state on commit" is a one-liner that breaks two things:

- `clearLive()` also clears `hitl`, and `commitTurn` runs after **every** turn —
  including one the planner parked. Calling it there closes the HITL modal the
  instant it opens. So `commitTurn` clears fields explicitly and guards on
  `if (finished)`, which is null on a parked turn.
- Clearing `events` blanks the **audit sidebar**, whose timings table is derived
  from them, exactly when the user wants to read it. So `events` survives the
  commit and a separate `turnCommitted` flag retires the live block instead.

Both were caught by asking "who else reads this state?" before deleting it —
cheaper than finding out from a demo.

### Not a testing bug

Tempting conclusion: "StrictMode caused it, turn StrictMode off." Wrong twice
over. StrictMode is a dev-only harness that *revealed* the impurity; cause 2 is
unconditional, so a production build would still have shown **two** cards. It
stays on.

### Smaller fixes in the same pass

- **A real in-flight guard.** `PromptPanel.submit()` guarded on the `busy`
  *prop*, which the parent sets asynchronously — a double-click on Run, or
  Ctrl+Enter twice, both read the stale `busy === false` and fired two genuine
  `submit_turn` invokes. That is two pipeline runs and two exchanges on disk, not
  a render artifact. `run()`/`resume()` now hold an `inFlight` ref, which updates
  synchronously.
- **`installGlobalErrorLogging` is idempotent.** It added `window` listeners with
  no cleanup and no guard, so StrictMode registered them twice and every uncaught
  error was logged twice — actively misleading when the log is the debugging tool.

### Deliberately not done

- **Stable ids on `Exchange`.** Index keys neither cause nor mask this bug: an
  append-only list that is never reordered renders correctly with them, and
  `Exchange` mirrors the Rust struct.
- **A frontend test runner.** There is no vitest/RTL in `package.json`, so none
  of this is pinned by an automated test yet, and `workbench-core/tests/` has no
  assertion that a turn emits exactly one `done`. Both are worth doing; both are
  bigger than this fix.

### Takeaway

Three identical outputs from one input feels like a duplication bug and is
usually a **rendering** bug. Count the delivery paths in the backend before
theorising about the model: if there is provably one emit site, one channel and
one return value, the duplication is downstream and nothing about the model or
the prompt can explain it. Then look for state that two components render with
no rule about which owns it.

## Post-demo — part 6: input reset, and a dropdown nobody could read

Two UX fixes from the same demo session. Both small; both worth writing down
because each has a non-obvious constraint behind it.

### The prompt box did not empty itself

After a turn delivered its answer, the prompt text and the attached files stayed
in the panel. Asking a follow-up meant manually clearing both first, every time.

The fix is a `resetToken` counter that `App` bumps and `PromptPanel` watches.
The interesting part is **what it is not keyed on**. The obvious trigger is
`busy` going false — and it is wrong, because `busy` also goes false when a turn
**fails** or **parks in the HITL modal**. Clearing there would throw away the
user's prompt and attachments at the exact moment they need them to retry. So the
counter is bumped in one place only: inside `commitTurn`'s `if (finished)` block,
which runs only when a report was actually delivered.

Two details that bite:

- **The hidden `<input type="file">` keeps its own value.** Clearing the React
  `files` state is not enough — without `inputRef.current.value = ""`, picking the
  *same* file again fires no `change` event and the attachment silently never
  comes back.
- **The `deep` toggle deliberately survives the reset.** It reads as a session
  preference, not per-question input.

### The model dropdown was a bright white slab

On the hardware screen, opening the *Resident LLM* dropdown produced a glaring
white popup in an otherwise dark app — and the option text was nearly invisible.

The cause is not a missing `background` rule. The **popup list is drawn by the
OS**, not by page CSS, and the webview renders native widgets in **light** mode
unless the page declares otherwise. The closed control looked fine because CSS
styles it directly (`background: var(--panel-2); color: var(--text)`); the moment
it opened, that near-white `--text` was painted onto the OS's white popup.

The fix is one line — `color-scheme: dark` on `:root` — which tells the engine to
render *all* native UI dark. It also darkens scrollbars and the checkbox app-wide,
which were quietly light for the same reason. Explicit `select option` colours and
focus/disabled states were added alongside, so the contrast is deliberate rather
than inherited by accident.

### Takeaway

A colour that "will not apply" is often a control the page does not actually
paint. Before adding more `background` rules, ask whether the pixels belong to
the OS — `color-scheme` is the lever for that whole category, and it fixes
scrollbars, checkboxes and date pickers at the same time.

## Post-demo — part 7: the dead gutter, and a shell that owns the viewport

"The chat should be full screen, there is unused space on the right." One line
caused it:

```css
.wb-left { flex: 1; padding: 20px; overflow: auto; max-width: 860px; }
```

`.wb-left` sits in a flex row between a 232px session rail and a 320px audit
rail. Capped at 860px, the three add up to 1412px — so on a 1920px monitor about
500px had nowhere to go and rendered as a dead gutter between the transcript and
the audit panel. The cap was there for a good reason (line length), but it was
applied to the **container** rather than to the **text**.

### The shape it became

- **`.wb-left` fills the row**, and the two rails take a share of the extra width
  with `clamp()` — an ultrawide now widens all three columns instead of dumping
  everything into the middle one.
- **Prose keeps its measure in `ch`, not the panel.** `.bubble-user` is
  `min(90%, 88ch)`, `.report-summary` is `96ch`. Panels go full-bleed; lines
  stay readable. This is the distinction the original cap missed.
- **`height: 100vh` instead of `min-height`.** The shell owns the viewport and
  the panes scroll inside it. With `min-height` the whole *page* scrolled, which
  is why a long transcript could push the prompt box off-screen entirely.
- **The transcript scrolls; the prompt panel is pinned** with a rule above it.
  That single change is most of what makes it read as an app rather than a
  document.
- **The report uses the width.** Its sections flow into
  `repeat(auto-fit, minmax(320px, 1fr))` — three columns at 1920px, two at
  1366px, one on a narrow window. No media query, no JSX change.

### Following the output

Making the transcript its own scroll container created a new obligation: new
output no longer scrolls into view by itself. Naive auto-scroll is worse than
none, because it yanks you back to the bottom while you are reading an earlier
answer. The fix is a `stickToBottom` ref updated from the **scroll handler**, not
measured inside the effect — by the time the effect runs the new content is
already in the DOM, so "was the user at the bottom *before* this?" can no longer
be answered.

### Verifying a layout without running the app

The GUI needs Ollama and a multi-minute turn, which is a poor loop for CSS. So
the layout was checked against a static harness — the **real `App.css`** plus the
real DOM structure with fixture content — screenshotted with Playwright at 1920
and 1366, asserting the numbers that matter:

```
wide    gap=0px overflow=0px cols=362px 362px 362px
narrow  gap=0px overflow=0px cols=350px 350px
```

`gap` is the distance between `.wb-left`'s right edge and `.audit`'s left edge —
the dead gutter, now zero. `overflow` is page scroll height beyond the viewport —
zero means the shell really does own the viewport.

That harness also caught an edge case reasoning alone got wrong. A degraded
report has only one section, and `auto-fit` was expected to stretch it across the
whole panel. It does not: `.report-head` and `.report-summary` are
`grid-column: 1 / -1`, so every track is spanned and none is empty for `auto-fit`
to collapse. The single section stays in one track, correctly.

> One harness artifact worth knowing about: `.bubble` is `white-space: pre-wrap`,
> so HTML indentation inside the bubble renders as real whitespace and the bubble
> looks too tall. React passes a single text node and has no such problem. Do not
> "fix" that one.

### Takeaway

A max-width on a flex child in a three-column shell does not centre anything — it
leaves a hole. Constrain the **text**, let the **container** fill. And a static
harness over the real stylesheet is a cheap way to check a layout when the real
app costs minutes per look.

## Post-demo — part 8: the cross-session leak

Reported from testing: "memory data leaking from different sessions to each
other." True, and structural rather than a slip in one call site.

### The shape of the leak

`PersistentMemory` (`memory/persistent.rs`) is **one file**,
`persistent_memory.json`, encrypted with a key derived from the machine —
not from a session id, not from a user id. Nothing scopes it. Every turn, in
every chat, did this:

```rust
// pipeline.rs, run_from_plan — BEFORE this fix
let facility = PersistentMemory::load(&config.persistent_path());
...
&facility.context_blob(),   // → role C's prompt, "long-term facility memory"
```

and `persist_long_term`, called after **every** turn and on `end_session`,
folded whichever session had just run into that same single file:

```rust
let digest = if !session.rolling_summary.is_empty() {
    session.rolling_summary.clone()
} else { /* … joined turn summaries … */ };
memory.history_digest = digest;   // overwrites the ONE global digest
```

So: ask something in chat A, its digest lands in `persistent_memory.json`. Open
an unrelated chat B and ask anything — role C loads that same file and receives
chat A's digest as `HISTORY: …` in its own prompt. Two conversations that never
should have met.

This is a **different mechanism** from `SessionContext`
(`sessions/<id>.json`) — that one *is* keyed by session id and was correctly
scoped from Stage 4 onward (see part 4 / docs 11). It is worth being precise
about which "memory" leaked, because the fix must not touch the one that
wasn't broken: a follow-up question inside one chat still needs its own
history, and does not lose it here.

### The temporary fix

A single `const GLOBAL_MEMORY_ENABLED: bool = false;` in `pipeline.rs`, gating
both the read and the write:

- the read site now passes `facility_context = String::new()` to `compiler::compile`
  when disabled — `ollama_engine.rs`'s prompt already renders an empty string as
  `(none)`, so no prompt-side special-casing was needed;
- `persist_long_term` returns immediately when disabled, at the top of the
  function — the **one** function both `pipeline.rs`'s own call and
  `src-tauri`'s `end_session` command go through, so neither call site can be
  re-enabled without the other by accident.

Nothing on disk is deleted. An existing `persistent_memory.json` from before
this flag simply stops being read while the flag is `false` — inert, not
wiped, in case there is ever a reason to look at what accumulated. Delete it
by hand if you want a clean slate; it will be recreated empty next time the
flag is flipped back on.

### Why this is temporary, and what the real fix is

Disabling the file stops the leak but also stops the thing it was *for* — a
plant's equipment register or a recurring theme surviving across chats, which
is a real blueprint requirement (`facility_metadata`, `recurrent_tags`; see
known problem 9 in the README, "persistent memory is still not visible"). The
actual fix is scoping: either key `PersistentMemory` by something narrower
than "the machine" (a user profile the app already has an id for), or make
cross-session memory an explicit **propose → user confirms** step before
anything crosses a session boundary, rather than an automatic digest fold.
Either is a bigger design decision than a bug-severity fix warrants on its
own — hence the flag, not a rewrite.

### Takeaway

"Which store is this?" is the first question, not "where's the bug." Two
memory tiers existed on purpose (session-scoped, user-scoped) and only one of
them was actually scoped — the fix is disabling exactly that one, and the
five extra minutes spent confirming `SessionContext` was unaffected is what
kept a memory bug from turning into a "does the chat still remember anything"
regression.

## Post-demo — part 9: making the app pitch itself

A jury walks up cold — no one from the team narrating over their shoulder. Two
gaps stood out on a component read-through, both about the running app, not the
README: an empty chat that gave no hint what a good question looks like, and a
pipeline whose only on-screen explanation (the `Stepper`) is a temporary widget
that is gone the instant a turn finishes.

### Empty state: real prompts, not an invented demo

The two example cards are not new copy — they are the exact prompts from
walkthrough steps 2 and 3, against the exact fixtures in `demo/fixtures/`.
Reusing already-verified strings instead of writing fresh ones means a card a
judge clicks is guaranteed to be a path that has actually been run, not a
plausible-looking sentence nobody tried.

Clicking a card fills the prompt **text** only. It cannot also attach the file:
the frontend has no filesystem capability (see part on conversations/memory —
`load_session` etc. all go through a Tauri command for the same reason), so
there is no way to turn a repo-relative path into a `File` object without one.
The card's second line — `attach inspection.pdf + valve.png` — says explicitly
what the one remaining manual step is, rather than leaving it to be guessed.

### A permanent "how it works," not a temporary one

The `Stepper` already narrates a turn stage by stage, but it is scoped to a
single live turn and disappears the moment `commitTurn` fires (part 6). A judge
who arrives, reads the empty state, and hasn't run anything yet — or who is
looking at the audit rail after a turn already committed — never sees it.

The new "How it works" list lives in `AuditSidebar` instead: that panel is open
by default and never disappears, so it is the one static description of the
pipeline a first-time viewer can read without running a turn or opening the
README. It mirrors `run_turn`'s own doc comment in `pipeline.rs` almost verbatim
— five stages, condensed from that comment's longer list (HITL and memory are
implementation detail, not press-facing) — so the in-app description and the
code it describes cannot quietly drift apart. The one stage worth a visual tag
is "Validate plan → Rust, deterministic," since that is the concrete answer to
"how do you keep a small model from hallucinating a bad plan," which is the
project's actual technical claim over "just prompt an LLM and hope."

### Small deliberate choices

- **A numbered list, not a horizontal strip with arrows.** The audit rail's
  width floor is 320px (`clamp(320px, 21vw, 420px)`, part 7); five chips in a
  row would wrap unpredictably at that width. A vertical list degrades to
  "still five readable rows" instead.
- **The longer explanation is a `title` tooltip**, not inline text, so the
  panel stays scannable at a glance and rewards a judge who hovers rather than
  forcing everyone to read five sentences.
- **Wired the same way `resetToken` already was** (part 6): a text value plus a
  counter `PromptPanel` watches, bumped by the parent on every click — a click on
  the *same* card twice still re-fills and refocuses, because the counter change
  is what the effect keys on, not the text.

### Takeaway

"Self-explanatory" was being solved one layer too low last time — window title
and a tagline fix the first five seconds, but a judge who then clicks around
for two minutes needs the *app*, not the README, to keep explaining itself. The
right home for that is whatever panel is already permanent, not whichever one
happens to be easiest to add text to.

## Where it ended

Stages 1–4 of the prototype→product pass done. `cargo test -p workbench-core -j 1`
and `cargo test -p tauri-app -j 1 --lib` green; `tsc` clean;
`cargo check --workspace` clean. Not yet verified end-to-end against a running
model. `feature/sovereign-workbench`.

### Takeaways for an aspiring engineer

- **Most of the time went to the environment, not the logic.** PATH, build
  locks, watcher config, crate-API guessing. Budget for that.
- **Separate "the logic" from "the app" early.** Being able to run the pipeline
  headlessly (`--example e2e`) made every later problem debuggable.
- **When a small model won't behave, constrain it, don't fight it.** Structured
  outputs solved in one edit what better prompting wouldn't have.
- **Commit in phases with passing tests.** Every commit here builds and tests
  green, so `git bisect` would actually work.
