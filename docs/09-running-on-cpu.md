# 09 · Running on a CPU-only machine

This project is meant to run air-gapped on ordinary office hardware — no GPU,
often 8 GB of RAM. That constraint shapes three things: which model gets
recommended, how many threads anything is allowed to use, and what happens when a
turn takes too long. This doc explains all three and the reasoning behind them.

## The core fact: on a CPU, model size *is* the latency

With a GPU, the weights sit in VRAM and the GPU streams through them fast; a 7B
model feels about the same as a 3B one. Without a GPU, every single token is
matrix-multiplied on the CPU, and that work scales with the number of parameters.
A 7B model on 4 cores is not "a bit slower" than a 1.5B — it is *minutes per
step* versus *seconds*.

So the rule the recommender follows: **RAM decides whether a model fits; cores
decide whether it is usable.** A machine with 32 GB of RAM and no GPU still gets a
small model, because the RAM only means the weights load, not that they run at a
tolerable speed.

## The model catalogue

`src-tauri/src/bootstrap/models.rs` has a `LLM_CATALOG`: each entry carries the
model's Ollama tag, approximate resident RAM, download size, and the context
window to run it with.

```rust
pub const LLM_FLOOR: LlmSpec = LlmSpec {
    name: "qwen2.5:1.5b-instruct",
    ram_gb: 1.8,
    download_gb: 1.0,
    num_ctx: 8192,
};
```

`recommend_models(&HardwareInfo)` walks a ladder:

| Tier | Condition (no GPU) | Model | Why |
|---|---|---|---|
| standard | ≥6 physical cores **and** ≥10 GB RAM free | `qwen2.5:3b-instruct` | enough cores to keep 3B responsive |
| **floor** | ≥4 physical cores **and** ≥5 GB free | `qwen2.5:1.5b-instruct` | the target machine; slow-ish but always usable |
| minimal | ≥2 physical cores **and** ≥3 GB free | `qwen2.5:0.5b-instruct` | rough answers, but it runs |
| unusable | anything less | (minimal, with a refusal in the tier label) | be honest |

GPU machines take a separate branch keyed on VRAM, but it *also* checks system RAM
now — a big VRAM card in an 8 GB box was previously handed a 7B it could not host.

### Why every entry is an "instruct" model

The pipeline constrains the model's output to a JSON schema
(`FormatType::StructuredJson`; see [04 · The agentic pipeline](04-agentic-pipeline.md)).
Instruct-tuned models in the qwen2.5 family follow a schema reliably from 0.5B
upward. A **"thinking" model** such as `qwen3:4b` writes its chain of reasoning
first and then — under the schema constraint — returns an *empty* body, which
surfaces as `EOF while parsing a value at line 1 column 0`. It is an
incompatibility, not a quality gap, so those models are kept out of the catalogue
entirely.

## Threads: count the *physical* cores

`HardwareInfo` now has both:

```rust
pub cpu_cores: usize,       // logical — hyperthreads, shown for info
pub physical_cores: usize,  // the number that matters
```

`std::thread::available_parallelism()` returns the *logical* count. On a
4-core/8-thread laptop that is `8`. Sizing a thread pool off `8` on 4 real cores
means 7 compute threads fighting over 4 execution units, and the OS scheduler —
and your desktop — suffers. Every thread budget in the app now derives from
`sysinfo::physical_core_count()`:

- `HardwareInfo::worker_threads()` → `physical_cores - 1` (leave one for the OS)
- Ollama's `num_thread` (via `ResourceLimits`)
- whisper.cpp's `n_threads`
- the `rten`/rayon pool used by OCR (`RAYON_NUM_THREADS`)

## Priority: Ollama is a separate process

This is the subtle one. `num_thread` limits how many workers Ollama *spawns*, but
not how the OS *schedules* them. At normal priority on a 4-core box, a full-core
inference run competes with the window manager and the shell loses — a frozen
volume overlay, a dead Start button, an unusable tray, lasting well past the end
of the run.

`bootstrap::ollama::lower_ollama_priority()` drops every `ollama*` process to
below-normal scheduling priority. It runs at startup and again right after the
model is warmed (the per-model *runner* is a lazily-spawned child process, so the
first call misses it). Inference stays exactly as fast — below-normal only yields
a core when something else genuinely needs it — but the desktop can no longer be
starved.

## When a turn takes too long

Two mechanisms, both in `crates/workbench-core`.

**Per-call timeout.** Every Ollama request goes through `OllamaEngine::guarded`,
which races the request future against:

- a wall-clock deadline — `ResourceLimits::call_timeout`, **240 s** by default.
  Deliberately generous: CPU inference of a long prompt is slow, not stuck, and a
  tight timeout would abandon calls that were about to succeed.
- a poll of the cancel flag (below), on a 200 ms tick.

A call past the deadline returns `CoreError::Timeout`. The executor turns that
into a failed step plus a `StepEvent::Warning` — the turn continues without that
step's output rather than hanging forever.

**Cancellation.** `CancelFlag` is a shared `AtomicBool` — the only way to reach
into a pipeline that is otherwise one uninterruptible `await`. The Tauri host
creates one per turn, stores it in `AppState`, hands a clone to the engine, and
exposes a `cancel_turn` command; the **Stop** button calls it. The flag is
checked before every model call and between every tool step, so Stop ends a turn
*within* a step. Tools that block a thread (`whisper`, `ocrs`, `pdfplumber` all
run under `spawn_blocking`) still finish their current call — nothing can safely
kill a blocking task from outside — but no further step starts.

## Telling the user before it hurts

- **`assess_llm` / the `assess_model` command.** When the bootstrap screen's
  model dropdown changes, the frontend asks the backend to judge the pick against
  the live hardware. The result drives a strip on the bootstrap card:
  *caution* ("~2× slower than the recommendation") or *blocked* ("needs 5.6 GB
  free, you have 3.1") — and *blocked* disables the Download button. Previously an
  override produced only a log line nobody saw.
- **Disk pre-flight.** `ensure_models` sums the download estimate for the models
  it is about to pull and refuses up front if the target drive lacks that plus
  2 GB of unpack headroom.
- **`StepEvent::Warning`.** The pipeline had no channel for "something is wrong
  but the turn goes on". Now it does, and the UI renders it as a dismissible
  amber strip above the report.

## Where to look when it is still slow

Read the log (`%APPDATA%\com.anubhav_anand.tauri-app\logs\workbench.log.<date>`,
or the **Open log folder** button in the audit sidebar):

| Log line | Meaning |
|---|---|
| `hardware probe … physical_cores=4 worker_threads=3` | the thread budget in force |
| `turn engine limits … num_ctx=8192 timeout_s=240` | the ceilings for this turn |
| `lowered Ollama process priority … count=2` | priority fix took effect |
| `llm call (structured) … elapsed_ms=…` | how long each model call actually took |
| `model call exceeded its time budget` | a call hit the 240 s timeout |
| `run cancelled by user before this step` | Stop was pressed |

See also [07 · Build, test, run](07-build-test-run.md#reading-the-logs).
