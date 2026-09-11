# 05 · Rust for this codebase

Enough Rust to read every file here, each idea tied to where it appears. This is
not a Rust course — it's "the 15 things this project leans on."

---

## 1. Crates, modules, workspaces

- A **crate** is a compiled unit — a library or a binary. `workbench-core` and
  `tauri-app` are our crates.
- A **module** (`mod`) is a namespace *inside* a crate. `crates/workbench-core/src/
  planner/mod.rs` declares `pub mod registry;` and `pub mod planner;`. Paths use
  `::` — `crate::planner::registry::validate_plan`.
- `pub` makes an item visible outside its module. No `pub` = private to the
  module and its children.
- A **workspace** (root `Cargo.toml`, `[workspace] members = [...]`) builds
  several crates together, sharing one `Cargo.lock` and one `target/`.
  `[workspace.dependencies]` lets both crates pin the same version of, say,
  `serde`.

```toml
# crates/workbench-core/Cargo.toml
serde.workspace = true          # "use the version from the workspace root"
schemars = "1"                  # a crate-local dependency
```

---

## 2. `String` vs `&str`, and ownership in one paragraph

Every value has one **owner**. When the owner goes out of scope, the value is
freed — no garbage collector. You can lend out **references** (`&T` shared,
`&mut T` exclusive) without giving up ownership.

- `String` — an owned, growable heap string. You can keep it, mutate it, move it.
- `&str` — a *borrowed view* into a string someone else owns. Cheap to pass, but
  you can't outlive the owner.

Rule of thumb in this code: function **arguments** take `&str` (borrow, don't
consume); **struct fields** and **return values** are `String` (own it).

```rust
// planner.rs
pub async fn plan_turn(
    source: &dyn PlanSource,   // borrowed
    prompt: &str,              // borrowed
    uploads: &[InputFile],     // borrowed slice
    sink: &dyn ProgressSink,   // borrowed
) -> Result<(IntentResult, PlanOutcome)>   // owned values handed back
```

`.to_string()` / `.clone()` turn a borrow into an owned copy when you need to
keep it (e.g. `GenerationRequest::new(self.llm_model.clone(), user)`).

---

## 3. Lifetimes — the `'a` in `ToolContext<'a>`

```rust
pub struct ToolContext<'a> {
    pub config:  &'a PipelineConfig,
    pub uploads: &'a [InputFile],
    pub outputs: &'a HashMap<String, serde_json::Value>,
    pub prompt:  &'a str,
    pub engine:  &'a OllamaEngine,
}
```

`'a` is a **lifetime parameter**. It says: "every reference in this struct lives
at least as long as `'a`, and the `ToolContext` itself may not outlive `'a`." The
compiler checks it. It exists because `ToolContext` is a bundle of borrows — the
executor builds one per step, hands it to `tool.run()`, and drops it before the
next step. Nothing in it is copied; it's a temporary window onto data the
executor owns.

You rarely *write* lifetimes; you read them as "this borrows from something".

---

## 4. `Option` and `Result` — no nulls, no exceptions

- `Option<T>` = `Some(T)` or `None`. Replaces null.
  ```rust
  match ctx.file_for(step, FileKind::Pdf) {
      Some(file) => { /* use it */ }
      None => return Ok(ToolResult::failure(&step.id, self.name(), "no PDF attached", …)),
  }
  // `file_for` reads `step.args["file"]` (the planner names the attachment),
  // falling back to the first file of that kind. `first_file_of` still exists;
  // use `file_for` in a tool so multi-file turns work.
  ```
- `Result<T, E>` = `Ok(T)` or `Err(E)`. Replaces exceptions. **Errors are values**
  you must handle or pass on.

### The `?` operator

`expr?` means: if `expr` is `Ok(v)` / `Some(v)`, unwrap to `v`; if it's `Err(e)`
/ `None`, **return it from the current function**. It's early-return sugar.

```rust
let response = self.client.generate(request).await
    .map_err(|e| CoreError::Ollama(e.to_string()))?;   // on error, return CoreError::Ollama(...)
```

For `?` to work across error types, `From<SourceError> for MyError` must exist.
`thiserror`'s `#[from]` generates those.

<a id="errors"></a>
### The one error enum (`lib.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("ollama request failed: {0}")]
    Ollama(String),
    #[error("model returned malformed JSON for role `{role}`: {source}")]
    Schema { role: &'static str, #[source] source: serde_json::Error },
    #[error(transparent)]
    Io(#[from] std::io::Error),     // any std::io::Error auto-converts via `?`
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    // …
}
pub type Result<T> = std::result::Result<T, CoreError>;
```

`#[derive(thiserror::Error)]` writes the `Display` and `Error` impls from those
`#[error("…")]` strings. `{0}`, `{role}`, `{source}` interpolate the variant's
fields. Now every function returns `Result<T>` (our alias) and `?` threads
errors up.

**Convention here:** `Err` is for *bugs / infrastructure down* (Ollama
unreachable). *Expected* failures (missing file, empty OCR) are a normal
`Ok(ToolResult { ok: false, … })` so the pipeline keeps going "degraded".

---

## 5. Structs and enums — enums carry data

`StepEvent` is an enum where each variant holds different fields:

```rust
pub enum StepEvent {
    Idle,
    ExecutingTool { tool: String, index: usize, total: usize },
    Done { report_json: String },
    Error { message: String },
}
```

This is the "make illegal states unrepresentable" idea: a `Done` event *always*
has a `report_json`; an `Idle` event has no extra fields; you can't mix them up.
`PlanOutcome` (`Ready(Plan)` / `AwaitingUser { errors, last_plan }`) and
`TurnOutcome` (`Completed(FinalReport)` / `AwaitingUser { … }`) work the same
way — the *shape* of the value tells you what happened.

### `match` and `if let`

```rust
let plan = match outcome {
    PlanOutcome::Ready(p) => p,
    PlanOutcome::AwaitingUser { errors, last_plan } =>
        return Ok(TurnOutcome::AwaitingUser { errors, plan_json: to_string(&last_plan)? }),
};
```
`match` must cover every variant (the compiler enforces it — add a variant, every
`match` that forgot it won't compile). `if let Some(x) = opt { … }` is a one-arm
match when you only care about one case.

---

<a id="traits"></a>
## 6. Traits — shared behaviour, and the key design tool here

A **trait** is an interface: a set of methods a type promises to provide.

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult>;
}
```

`struct DocumentTool;` then `impl Tool for DocumentTool { … }`. Six tool structs,
one trait.

### Trait objects: `Box<dyn Tool>` and `&dyn ProgressSink`

`dyn Tool` means "some type that implements `Tool`, decided at runtime". The
executor stores `HashMap<&str, Box<dyn Tool>>` — a bag of *different* concrete
tool types behind one interface. Calling `tool.run(...)` dispatches to the right
implementation dynamically (a vtable lookup).

Why it matters: **the pipeline depends on the trait, not the concrete types.**

- `plan_turn` takes `&dyn PlanSource`. In production that's `&OllamaEngine`. In
  `tests/planner_retry.rs` it's a hand-written mock that always returns a bad
  plan — so the retry ceiling is tested with *zero* AI and in microseconds.
- `run_turn` emits through `&dyn ProgressSink`. Production: `ChannelSink` (Tauri).
  Tests: `VecSink` (collects events into a `Vec`). The e2e example: `StdoutSink`
  (prints them).

This is **dependency inversion** — high-level code (the pipeline) and low-level
code (Tauri, a mock) both depend on an abstraction (the trait) in the middle.
It's the single most important pattern in this codebase.

### `Send + Sync`

`Send` = safe to move to another thread. `Sync` = safe to share `&T` across
threads. The pipeline runs on an async runtime that may hop threads, so
everything it holds must be `Send`. You'll see `Send + Sync` bounds on the traits
for this reason.

---

## 7. `async` / `.await`

An `async fn` returns a **future** — a value representing "work that will finish
later". It does nothing until you `.await` it (or hand it to the runtime). `.await`
means "pause here, let the runtime do other things, resume when this is ready".

```rust
let response = self.client.generate(request).await?;   // network call, non-blocking wait
```

- **The runtime**: `tokio`. `#[tokio::main]` on `main` in `examples/e2e.rs` starts
  it. Tauri starts its own.
- **`async` in traits**: plain Rust traits couldn't have `async fn` until
  recently, and `dyn` still needs help — hence `#[async_trait::async_trait]` on
  `Tool`, `PlanSource`. It rewrites `async fn run(...)` into
  `fn run(...) -> Pin<Box<dyn Future<…>>>`. You don't need to understand the
  rewrite; just know that's why the macro is there.
- **`spawn_blocking`**: some libraries are *synchronous* and slow (`pdfplumber`,
  `whisper-rs`, `ocrs`). Calling them directly would block the async runtime's
  thread. So:
  ```rust
  let parsed = tokio::task::spawn_blocking(move || parse(&path)).await?;
  ```
  runs `parse` on a dedicated thread pool and `.await`s the handle. `move`
  transfers ownership of `path` (a `String`) into the closure so it's `'static`
  and `Send`.

---

## 8. `serde` — types ⇄ JSON

`#[derive(Serialize, Deserialize)]` generates code to turn a struct into JSON and
back.

```rust
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Plan { pub steps: Vec<TaskStep> }
```

- `serde_json::to_string(&plan)?` → `{"steps":[…]}`
- `serde_json::from_str::<Plan>(text)?` → a `Plan`, or `Err` if the shape is wrong

Attributes tune it:
- `#[serde(rename_all = "snake_case")]` — `ExecutingTool` field/variant →
  `executing_tool`.
- `#[serde(tag = "stage")]` on `StepEvent` — an *internally tagged* enum:
  `{"stage":"done","report_json":"…"}` instead of `{"Done":{…}}`. This is the
  shape `src/types.ts` matches on.
- `#[serde(default)]` — if the field is missing in the JSON, use `Default`
  instead of failing.
- `serde_json::Value` — an untyped JSON tree (`TaskStep.args`, `ToolResult.data`).
  Use it when the shape genuinely varies. Read with `.get("text").and_then(|v|
  v.as_str())`.

---

## 9. `schemars` — a JSON Schema *from* the type

```rust
FormatType::StructuredJson(Box::new(JsonStructure::new::<Plan>()))
```

`#[derive(JsonSchema)]` (from `schemars`) lets `JsonStructure::new::<Plan>()`
produce the JSON Schema for `Plan` at compile time. We send it to Ollama so the
model is *constrained* to emit that exact shape. This is why a 3B model stopped
emitting objects-where-strings-belong (see [doc 02](02-execution-log.md#the-real-fix--structured-outputs)).

---

## 10. Generics and bounds

```rust
async fn generate_json<T: DeserializeOwned + JsonSchema>(
    &self, system: &str, user: String, role: &'static str,
) -> Result<T>
```

`T` is a type parameter. `T: DeserializeOwned + JsonSchema` is a **bound** —
`T` must implement both traits. One function body works for `IntentResult`,
`Plan`, *and* `FinalReport` because all three satisfy the bound. The compiler
generates a specialised copy per concrete `T` (monomorphisation) — zero runtime
cost.

`impl Into<String>` in an argument position is a lightweight generic: "anything
that can become a `String`" — accepts `&str`, `String`, `&String`.

---

## 11. Closures

Anonymous functions. `|args| body`.

```rust
self.uploads.iter().find(|f| f.kind == kind)          // |f| f.kind == kind  is a closure
let err = |e: pdfplumber::PdfError| CoreError::Tool { … };   // stored in a variable, reused
```

`move |…| …` moves captured variables into the closure (needed for
`spawn_blocking`). A closure that captures nothing is `Copy` — you can pass it to
`.map_err(err)` inside a loop and it's copied each time.

---

## 12. Iterators — `.iter().map().filter().collect()`

Lazy, chainable, no index bugs.

```rust
let ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();

let hits: Vec<serde_json::Value> = index
    .search(&query_vec, top_k)
    .into_iter()
    .map(|(score, c)| serde_json::json!({ "text": c.text, "source": c.source, "score": score }))
    .collect();
```

`.iter()` borrows, `.into_iter()` consumes. `.collect()` needs a target type
(here from the `let` annotation). `.any(|x| …)` / `.all(|x| …)` /
`.filter_map(…)` / `.find(…)` show up throughout `validate_plan`.

---

## 13. `Vec`, `HashMap`, `HashSet`

- `Vec<T>` — growable array. `plan.steps: Vec<TaskStep>`.
- `HashMap<K, V>` — dictionary. `outputs: HashMap<String, serde_json::Value>`
  (step id → that step's output), `ToolRegistry`'s `HashMap<&str, Box<dyn Tool>>`.
- `HashSet<T>` — set, for membership tests. `validate_plan` builds a `HashSet` of
  step ids to check `depends_on` references in O(1).

---

## 14. `Arc`, `Mutex`, and "why can't I hold the lock across `.await`"

`src-tauri/src/lib.rs`:

```rust
pub struct AppState {
    pub ollama: Ollama,
    pub model_plan: Mutex<Option<ModelPlan>>,   // shared, mutable
}
```

- `Mutex<T>` — a lock. `.lock().unwrap()` gives a **guard**; drop the guard to
  release. Tauri hands every command an `&AppState`, so many commands can read it
  at once; `Mutex` serialises the *writes* to `model_plan`.
- **The rule**: don't hold a `std::sync::Mutex` guard across an `.await`. The
  guard isn't `Send`, and an awaiting task can move threads. The code sidesteps
  this by locking, cloning out what it needs, and dropping the guard *before* any
  `.await`:
  ```rust
  fn plan(&self) -> ModelPlan {
      self.model_plan.lock().unwrap().clone().unwrap_or_else(ModelPlan::fallback)
  }   // guard dropped here; the returned ModelPlan is owned
  ```
- `Arc<T>` — atomically reference-counted shared ownership (multiple owners, freed
  when the last drops). Used less here because Tauri manages `AppState`'s sharing,
  but it's the standard partner to `Mutex` for "shared mutable state".

---

## 15. Derive macros, recap

`#[derive(...)]` runs a macro at compile time that writes an `impl` for you:

| derive | gives you |
|---|---|
| `Debug` | `println!("{:?}", x)` |
| `Clone` | `x.clone()` |
| `Copy` | value is bit-copied instead of moved (small types only) |
| `Default` | `T::default()` |
| `PartialEq, Eq` | `==` |
| `Hash` | usable as a `HashMap`/`HashSet` key |
| `Serialize, Deserialize` | serde JSON in/out |
| `JsonSchema` | `schemars` schema |
| `thiserror::Error` | `Display` + `std::error::Error` from `#[error("…")]` |

You'll see stacks like
`#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]`
on `FileKind` — it's cheap and each one unlocks a capability the code uses.

---

Next: [06 · RAG and the vector store](06-rag-and-why-not-a-vector-db.md).
