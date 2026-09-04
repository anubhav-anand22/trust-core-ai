# 03 · Codebase tour

A guided walk through every folder. Open the files alongside this. Paths are from
the repo root.

```
local agentic workbench/
├── Cargo.toml                  workspace root — lists the two member crates
├── package.json                frontend deps + the `tauri` script
├── vite.config.ts              dev-server config (the target/ ignore fix lives here)
├── index.html                  the single HTML page React mounts into
├── src/                        React frontend  (TypeScript)
├── src-tauri/                  the desktop host  (Rust)
├── crates/workbench-core/      the pipeline library  (Rust, no Tauri)
├── demo/                       fixture generator + a README for running things
└── docs/                       you are here
```

---

## `crates/workbench-core/` — the brain

No Tauri, no UI, no window. Just types and functions. This is what the tests and
the `--example e2e` binary exercise.

### `src/lib.rs` — the crate's front door

Declares every module and defines two things used everywhere:

- **`PipelineConfig`** — a plain struct of paths and model names. Built once per
  turn by the host and passed down. Has helper methods like
  `config.session_path()` that join sub-paths onto `data_dir`.
- **`CoreError`** — one `enum` covering every failure mode
  (`Ollama`, `Schema { role }`, `Tool { tool, message }`, `Ocr`, `Audio`,
  `Crypto`, `Io`, `Json`, …). `pub type Result<T> = std::result::Result<T,
  CoreError>;` so every function in the crate returns `Result<Something>`.

Built with `thiserror` — see [doc 05](05-rust-for-this-codebase.md#errors).

### `src/events.rs` — the progress stream

```rust
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum StepEvent {
    Idle,
    ParsingContext { attempt: u32 },
    ValidatingPlan { attempt: u32 },
    AwaitingUser  { errors: Vec<String>, plan_json: String },
    ExecutingTool { tool: String, index: usize, total: usize },
    ToolFinished  { tool: String, ok: bool, elapsed_ms: u128 },
    QualityCheck  { passed: bool },
    Synthesizing,
    Done  { report_json: String },
    Error { message: String },
}
```

Every pipeline stage emits one of these. `#[serde(tag = "stage")]` means it
serialises to `{"stage": "executing_tool", "tool": "parse_pdf", …}` — a shape
the TypeScript side pattern-matches on (`src/types.ts` has the mirror).

**`ProgressSink`** is the trait the pipeline emits *through*:
```rust
pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: StepEvent);
}
```
The Tauri host implements it over a `Channel` (`src-tauri/src/events.rs`); tests
implement it as `VecSink` that just collects events into a `Vec` so assertions
can check the sequence. **The pipeline never knows which one it's talking to.**
This is dependency inversion — [doc 05](05-rust-for-this-codebase.md#traits).

### `src/engine/` — the one LLM, three roles

- **`schemas.rs`** — the typed contract. Every structure the model must produce
  (`IntentResult`, `Plan`, `TaskStep`, `FinalReport`) and every structure passed
  around (`ToolResult`, `QualityReport`, `InputFile`, `FileKind`). The
  model-output types also `#[derive(JsonSchema)]` so their schema can be sent to
  Ollama.

- **`ollama_engine.rs`** — `OllamaEngine` wraps the Ollama HTTP client plus the
  three model names (`llm_model`, `vision_model`, `embed_model`). Its private
  `generate_json::<T>()` is the workhorse:
  ```rust
  async fn generate_json<T: DeserializeOwned + JsonSchema>(
      &self, system: &str, user: String, role: &'static str,
  ) -> Result<T> {
      let request = GenerationRequest::new(self.llm_model.clone(), user)
          .system(system.to_string())
          .format(FormatType::StructuredJson(Box::new(JsonStructure::new::<T>())))
          .keep_alive(KeepAlive::Indefinitely);
      let response = self.client.generate(request).await
          .map_err(|e| CoreError::Ollama(e.to_string()))?;
      serde_json::from_str::<T>(strip_code_fence(response.response.trim()))
          .map_err(|source| CoreError::Schema { role, source })
  }
  ```
  Then `parse_intent()` (role A), `build_plan()` (role B), `compile_report()`
  (role C) each call it with a different `system` prompt. Plus `analyze_image()`
  and `analyze()` for the vision and analysis tools, and `summarize()` for memory
  compression.

  The "one resident model" rule lives here: text calls use
  `KeepAlive::Indefinitely`; `analyze_image()` uses
  `KeepAlive::UnloadOnCompletion` so `moondream` is freed the instant it answers.

### `src/planner/` — decide and check

- **`registry.rs`** — the heart of the "not a chat agent" design.
  ```rust
  pub const TASK_REGISTRY: &[TaskSpec] = &[
      TaskSpec { name: "transcribe_audio", stage: Extract,  requires_file: Some(Audio), … },
      TaskSpec { name: "parse_pdf",        stage: Extract,  requires_file: Some(Pdf),   … },
      TaskSpec { name: "ocr_image",        stage: Extract,  requires_file: Some(Image), … },
      TaskSpec { name: "analyze_image",    stage: Extract,  requires_file: Some(Image), … },
      TaskSpec { name: "search_knowledge", stage: Retrieve, requires_file: None,        … },
      TaskSpec { name: "summarize",        stage: Analyze,  requires_file: None,        … },
      TaskSpec { name: "compare_to_sop",   stage: Analyze,  requires_file: None,        … },
  ];
  ```
  Adding a capability = one row here + one `Tool` impl + one line in
  `default_registry()`. Nothing else.

  **`validate_plan(plan, uploads) -> Vec<String>`** is pure branching. Empty
  vector = the plan may run. Six checks:
  1. every `step.task` is in `TASK_REGISTRY`
  2. step ids unique; every `depends_on` names a real step
  3. dependencies are listed *before* their dependents (the executor runs
     top-to-bottom, so this also rules out cycles)
  4. stage order — an `Analyze` step must be preceded by an `Extract`/`Retrieve`
     step; a turn with files must have at least one extraction step
  5. a task that `requires_file: Some(kind)` has a file of that kind attached
  6. every attached file actually exists on disk

  It's tested in isolation in `tests/registry.rs` — no AI, instant.

- **`planner.rs`** — the retry loop.
  ```rust
  pub const MAX_PLAN_ATTEMPTS: u32 = 3;   // 1 try + 2 retries

  for attempt in 1..=MAX_PLAN_ATTEMPTS {
      let plan = source.build_plan(prompt, &intent, uploads, &errors).await?;
      sink.emit(StepEvent::ValidatingPlan { attempt });
      errors = validate_plan(&plan, uploads);
      if errors.is_empty() { return Ok((intent, PlanOutcome::Ready(plan))); }
  }
  // exhausted → PlanOutcome::AwaitingUser { errors, last_plan }
  ```
  Note `&errors` is fed *into* the next `build_plan` call — the model gets its
  own validator failures as feedback ("your previous plan was rejected: …").
  `build_plan` / `parse_intent` come from a `PlanSource` trait, not the concrete
  engine, so `tests/planner_retry.rs` can drive it with a fake that always
  returns a bad plan and assert it stops after exactly 3 tries.

### `src/executor/` — run, check, write up

- **`executor.rs`** — `ToolRegistry` (a `HashMap<&str, Box<dyn Tool>>`) and
  `execute_plan()`: a single `for` loop over `plan.steps`. For each step it looks
  up the tool, builds a `ToolContext`, calls `tool.run(step, ctx).await`, times
  it, emits `ExecutingTool` / `ToolFinished`, and stores the result's `data` in
  an `outputs` map keyed by step id so later steps can read it. **A tool that
  returns `Err` or `ok: false` does not stop the loop** — the run continues
  "degraded".

- **`quality.rs`** — `assert_sane(plan, results, report) -> QualityReport`. Plain
  predicates: every step produced a result; no tool failed; if `search_knowledge`
  was planned it returned ≥1 chunk; extraction steps produced non-empty text; the
  report summary is non-empty. Rule-based *on purpose* — the blueprint forbids
  "an LLM grading itself".

- **`compiler.rs`** — `compile()` calls role C (`engine.compile_report`), retries
  once on bad JSON, and if that still fails builds a plain-text report straight
  from the `ToolResult`s. A flaky compiler can't sink an otherwise good run.

### `src/tools/` — the capabilities

`mod.rs` defines the contract:
```rust
pub struct ToolContext<'a> {
    pub config:  &'a PipelineConfig,
    pub uploads: &'a [InputFile],
    pub outputs: &'a HashMap<String, serde_json::Value>,  // earlier steps' data
    pub prompt:  &'a str,
    pub engine:  &'a OllamaEngine,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult>;
}
```

The six implementations:

| File | Task(s) | How it works |
|---|---|---|
| `document.rs` | `parse_pdf` | `pdfplumber` crate: per page, `extract_text()` + `find_tables()`. Empty text → a warning "looks like a scanned PDF, out of scope". Runs inside `tokio::task::spawn_blocking` because pdfplumber is synchronous. |
| `ocr.rs` | `ocr_image` | `ocrs` + `rten`: load two `.rten` models, `detect_words → find_text_lines → recognize_text`. Degrades gracefully if the model files aren't present. |
| `audio.rs` | `transcribe_audio` | `symphonia` decodes any container → down-mix to mono → hand-rolled linear resample to 16 kHz → `whisper-rs`. The `WhisperContext` is dropped at the end of the function so the model leaves RAM. |
| `vision.rs` | `analyze_image` | base64-encode the image, `ctx.engine.analyze_image(b64, question)` → `moondream` via Ollama with `keep_alive: 0`. |
| `rag.rs` | `search_knowledge` | On first use, chunk + embed every KB file into an embedded **LanceDB** table (`data/lancedb/sop_kb`). On query, embed the query and let LanceDB return the nearest chunks by cosine distance. See [doc 06](06-rag-and-why-not-a-vector-db.md). |
| `analysis.rs` | `summarize`, `compare_to_sop` | One struct, two constructors. Gathers the `data` of its `depends_on` steps from `ctx.outputs`, sends it to the resident model with a task-specific instruction. |

Every tool follows the same skeleton: check the input exists (return `ok: false`
if not), do the work off the async thread if it's blocking, wrap the output in a
`ToolResult` with `elapsed_ms`.

### `src/memory/`

> The RAG store lives behind two functions in `rag.rs` — `ensure_ingested()`
> and `search()`. Everything else (`chunk_text`, the `Tool` impl, the executor,
> the tests) is storage-agnostic, so the vector backend can change without
> rippling out.

- **`session.rs`** — `SessionContext { session_id, turns, rolling_summary }`,
  stored as plain JSON (`session_context.json`). `append_turn()` adds a turn; once
  there are more than 6 turns, `compress()` folds the oldest into
  `rolling_summary` using the resident model (no second model loaded). Atomic
  write (`write tmp → rename`).
- **`persistent.rs`** — `PersistentMemory { facility_metadata, recurrent_tags,
  history_digest }`, stored **encrypted** (`persistent_memory.json`).
  AES-256-GCM; the key is `PBKDF2-HMAC-SHA256(machine_id, salt, 200 000 rounds)`.
  On-disk layout: `base64( nonce[12] || ciphertext || tag )`. A missing / corrupt
  / wrong-machine file returns a fresh default with a warning — losing long-term
  memory must never stop a session starting. Documented in the file as
  *obfuscation-grade*, not HSM-grade (the key is reconstructible on the same
  machine).

### `src/pipeline.rs` — `run_turn`

Glues it together. `TurnOutcome` is either `Completed(FinalReport)` or
`AwaitingUser { errors, plan_json }`. The body:

```rust
sink.emit(StepEvent::Idle);
let (_intent, outcome) = plan_turn(engine, prompt, uploads, sink).await?;
let plan = match outcome {
    PlanOutcome::Ready(p) => p,
    PlanOutcome::AwaitingUser { errors, last_plan } =>
        return Ok(TurnOutcome::AwaitingUser { errors, plan_json: to_string(&last_plan)? }),
};
let results  = execute_plan(&plan, registry, uploads, config, engine, prompt, sink).await?;
let quality  = assert_sane(&plan, &results, None);
sink.emit(StepEvent::QualityCheck { passed: quality.passed });
let mut report = compiler::compile(engine, prompt, &results, &session_blob, &facility_blob, sink).await;
report.degraded = report.degraded || !quality.passed;
session.append_turn(turn, engine, &config.session_path()).await?;   // best-effort
persist_long_term(config, &session);                                // encrypt every turn
sink.emit(StepEvent::Done { report_json: to_string(&report)? });
```

### `examples/e2e.rs`

A command-line program that builds a `PipelineConfig`, an `OllamaEngine`, the
`default_registry()`, and calls `run_turn` — printing each `StepEvent` and the
final report. This is how the pipeline is verified against real models without
the GUI. Run it: see [doc 07](07-build-test-run.md).

### `tests/`

- `registry.rs` — `validate_plan` rejects each bad-plan class, accepts a good one.
- `planner_retry.rs` — the retry ceiling and the recovery-on-last-attempt case,
  using a mock `PlanSource`.
- `tools_smoke.rs` — `default_registry()` has every task; extraction tools fail
  cleanly with no file; analysis fails cleanly with no evidence.
- `memory/persistent.rs` (inline `#[cfg(test)]`) — encrypt/decrypt round-trip,
  plaintext is *not* on disk, corrupt/missing files tolerated.
- `tools/rag.rs` (inline) — cosine and chunking maths.

---

## `src-tauri/` — the desktop host

### `src/lib.rs`

- **`AppState`** — `{ ollama: Ollama, model_plan: Mutex<Option<ModelPlan>> }`,
  Tauri-managed, shared across commands.
- **Commands** (`#[tauri::command]` — callable from JS via `invoke`):
  `generate_response` (debug), `is_ollama_installed`, `ensure_ollama_installed`,
  `detect_hardware`, `probe_system`, `ensure_models`, `set_model_plan`,
  `warm_model`, `audit_paths`, `submit_turn`, `end_session`.
- **`submit_turn`** — decodes the base64 file uploads, writes them under
  `uploads/<session>/`, builds the `PipelineConfig` + `OllamaEngine` +
  `default_registry()`, wraps the incoming `Channel<StepEvent>` in a
  `ChannelSink`, and calls `workbench_core::run_turn`.

### `src/bootstrap/`

- `ollama.rs` — `is_ollama_installed` (runs `where ollama`) and
  `ensure_ollama_installed` (per-OS install script, streams progress). Moved
  verbatim from the original scaffold.
- `hardware.rs` — `detect_hardware()`: RAM via `sysinfo`, GPU via `nvidia-smi`
  then a PowerShell `Win32_VideoController` query.
- `models.rs` — `recommend_models(hw)` (the tier table: ≥8 GB VRAM or ≥16 GB RAM
  → `qwen2.5:7b-instruct`, etc.), `probe_system()` (hardware + recommendation in
  one call), and `ensure_models(list)` (pull anything missing, streaming
  download %).

### `src/events.rs`

```rust
pub struct ChannelSink(pub Channel<StepEvent>);
impl ProgressSink for ChannelSink {
    fn emit(&self, event: StepEvent) { let _ = self.0.send(event); }
}
```
Eight lines. The only glue between "the pipeline emits events" and "Tauri streams
them to the browser".

---

## `src/` — the React frontend

- **`types.ts`** — the TypeScript mirror of `StepEvent`, `ModelPlan`,
  `FinalReport`, etc. A discriminated union: `{ stage: "executing_tool"; tool:
  string; … } | { stage: "done"; report_json: string } | …`.
- **`lib/pipeline.ts`** — one typed function per Tauri command. `submitTurn()`
  creates a `Channel`, sets `channel.onmessage`, and passes it to `invoke`.
  `fileToUpload()` reads a `File` into `{ name, content_base64 }`.
- **`components/`**
  - `BootstrapGate.tsx` — the first-run wizard: check Ollama → install →
    `probe_system` → show specs + an LLM override dropdown → `ensure_models`
    (streamed) → `warm_model` → hand off.
  - `PromptPanel.tsx` — textarea + drag-and-drop file zone.
  - `Stepper.tsx` — maps the latest `StepEvent` onto a fixed strip of stages.
  - `HitlModal.tsx` — shown on `awaiting_user`; the validator's complaints + an
    editable plan JSON box.
  - `ReportView.tsx` — summary / findings / safety notes / citations + a
    `degraded` badge.
  - `AuditSidebar.tsx` — per-step timings (from `ToolFinished` events), the
    active model names, and the resolved on-disk paths (from `audit_paths`).
- **`App.tsx`** — holds the state (`events`, `report`, `hitl`), renders
  `BootstrapGate` until a `ModelPlan` is chosen, then the workbench.

Next: [04 · The agentic pipeline](04-agentic-pipeline.md) — one turn, traced.
