# Sovereign AI Workbench — Documentation

These docs are written for someone learning to program who wants to understand
**how this codebase works and why it's shaped the way it is** — not just API
reference. Read them in order the first time.

| # | Doc | What you'll get |
|---|---|---|
| 01 | [Overview](01-overview.md) | The problem, the one-paragraph mental model, the architecture diagram |
| 02 | [Execution log](02-execution-log.md) | How the project was actually built — every decision, every wall hit, how it was solved |
| 03 | [Codebase tour](03-codebase-tour.md) | A guided walk through every folder and file, with the key types |
| 04 | [The agentic pipeline](04-agentic-pipeline.md) | Deep dive on the core loop: intent → plan → validate → execute → synthesise. Traces one real turn end to end |
| 05 | [Rust for this codebase](05-rust-for-this-codebase.md) | The language features you need to read the code: traits, `async`, `Result`/`?`, `serde`, workspaces, lifetimes — each tied to where it appears here |
| 06 | [RAG and the vector store](06-rag-and-why-not-a-vector-db.md) | What retrieval-augmented generation is, the embedding + cosine-similarity maths, and the embedded-LanceDB store (with the tradeoff vs a plain cosine loop) |
| 07 | [Build, test, run](07-build-test-run.md) | Toolchain, environment variables, every command, where files live, troubleshooting |
| 08 | [Extending it](08-extending.md) | How to add a tool, a task, a pipeline stage, a UI panel |
| 09 | [Running on a CPU-only machine](09-running-on-cpu.md) | Model tiers, thread budgets, process priority, timeouts and Stop — why the desktop used to freeze and what fixed it |
| 10 | [Documents and tables](10-documents-and-tables.md) | How a PDF becomes evidence, the three ways a fact gets lost on that path, and Fast vs Deep read |
| 11 | [Conversations and memory](11-conversations-and-memory.md) | Per-session chat storage, the sidebar, how follow-up questions are answered, and the two memory tiers |

## Glossary (quick reference)

| Term | Meaning in this project |
|---|---|
| **Agent / agentic** | A program that decides *which steps to take* to answer a request, then takes them. Here the "deciding" is one LLM call that emits a plan; the "taking" is a fixed executor. No open-ended loop. |
| **LLM** | Large Language Model. We run one locally via **Ollama** (`llama3.2:3b`, `qwen3:4b`, …). |
| **Ollama** | A local server (`http://127.0.0.1:11434`) that loads and runs LLMs on your machine. No cloud. |
| **Resident model** | The one text LLM we keep loaded in RAM the whole session (`keep_alive: -1`). Roles A/B/C are the *same* model with different system prompts. |
| **Role A / B / C** | Intent Parser / Task Planner / Output Compiler — three jobs, one model, three system prompts. |
| **System prompt** | Instructions given to the model *before* the user's text that set its behaviour ("You are the Task Planner. Return JSON…"). |
| **Structured output** | Asking Ollama to constrain the model's response to a JSON **schema**, so it can't emit the wrong shape. We derive the schema from Rust types with `schemars`. |
| **Embedding** | A list of ~768 numbers that represents the *meaning* of a piece of text. Similar meanings → similar vectors. Produced by `nomic-embed-text`. |
| **Cosine similarity** | A number from -1 to 1 measuring how aligned two vectors are. We use it to rank knowledge-base chunks against a query. |
| **RAG** | Retrieval-Augmented Generation: fetch relevant reference text first, then let the model answer *using* it, so answers are grounded and citable. |
| **VLM** | Vision-Language Model. `moondream` — takes an image + a question, returns a description. |
| **Chunk** | A ~800-token slice of a document. We split SOPs into chunks so retrieval returns just the relevant passage, not the whole file. |
| **LanceDB** | An *embedded* vector database (a library, not a server) built on Apache Arrow. Stores the KB chunks + vectors on disk and answers nearest-neighbour queries. |
| **ANN** | Approximate Nearest Neighbour — an index (HNSW, IVF-PQ) that finds *almost* the closest vectors very fast. Worth it at 10⁴+ vectors; below that a plain scan is exact and just as quick. |
| **DAG** | Directed Acyclic Graph. The plan's steps form one: each step may depend on earlier steps, no cycles. |
| **Deterministic validation** | Checking the plan with plain `if`/`for` code (no AI), so the result is reproducible and explainable. |
| **HITL** | Human-in-the-loop. Reached only if even the deterministic fallback plan fails validation — normally the planner's retries end in a fallback plan, not a pause. |
| **`file_for`** | `ToolContext` method: resolve which attachment a step operates on from `args["file"]`, falling back to the first file of that kind. Makes multi-file turns work. |
| **Fallback plan** | A plan built with no model call — one extraction step per attachment, then analysis. Used when the LLM planner can't produce a valid one. |
| **Session** | One chat, stored as `sessions/<id>.json`: a full transcript (`exchanges`) plus a bounded model-context summary (`turns` + `rolling_summary`). Listed in the sidebar. |
| **`answer_followup`** | A task for a conversational follow-up ("and debit cards?") — runs the transcript through the resident model, no new files or KB. Valid as a lone step. |
| **Transcript vs. model context** | The transcript keeps every turn for redisplay; the model only ever sees the last N turns plus a rolling summary of the rest. |
| **Stepper** | The UI strip showing which pipeline stage is active. Driven by `StepEvent`s streamed from Rust. |
| **`keep_alive`** | Ollama parameter: how long to keep a model in RAM after a request. `-1` = forever, `0` = unload immediately. |
| **Physical vs logical cores** | Physical = real execution units. Logical = those times the hyperthread count. Thread pools are sized off *physical* here; logical flatters the number and starves the scheduler. |
| **`CancelFlag`** | A shared `AtomicBool` the Stop button flips. The pipeline polls it before every model call and between every step — the one way to interrupt a long `await`. |
| **`StepEvent::Warning`** | A non-fatal notice from the pipeline (skipped file, timed-out step). Renders as an amber strip; the turn still finishes. |
| **`guarded` call** | Every Ollama request is raced against a wall-clock timeout and the cancel flag, so a call can't hang the turn. |
| **`tables_text`** | A PDF's tables flattened to `col \| col \| col` lines with the header repeated per row. What the model actually reads — the structured `tables` field was being dropped. |
| **Fast / Deep read** | Fast retrieves the passages nearest the question; Deep maps the analysis over every window of the whole document and reduces. Deep is a checkbox, slower, misses nothing. |
| **Embedding prefix** | `nomic-embed-text` wants `search_query:` on queries and `search_document:` on passages; it is measurably worse without them. |
| **GGUF / quantised** | A compressed on-disk format for LLM weights. `ggml-base.en.bin` is a quantised Whisper model. |
| **Tauri** | A framework for desktop apps: a Rust backend + a web-tech frontend (here React) in a native window. |
| **Crate** | A Rust package/library. `workbench-core` and `tauri-app` are our two crates; everything in `Cargo.toml` `[dependencies]` is a third-party crate. |
| **Workspace** | A set of crates built together from one root `Cargo.toml`. |
