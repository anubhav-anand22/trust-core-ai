# 08 · Extending it

Common changes and exactly which files they touch. The architecture is built so
each of these is small and local.

---

## Add a new tool (e.g. `detect_gauge_reading`)

**1. Register the task** — `crates/workbench-core/src/planner/registry.rs`, one
row in `TASK_REGISTRY`:

```rust
TaskSpec {
    name: "detect_gauge_reading",
    stage: Stage::Extract,
    requires_file: Some(FileKind::Image),
    description: "Read the numeric value from an attached pressure/temperature gauge photo.",
},
```

The `description` is injected into role B's prompt automatically, so the planner
learns the new capability with no other change.

**2. Implement `Tool`** — new file `src/tools/gauge.rs`:

```rust
use crate::engine::schemas::{FileKind, TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::Result;

pub struct GaugeTool;

#[async_trait::async_trait]
impl Tool for GaugeTool {
    fn name(&self) -> &'static str { "detect_gauge_reading" }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();
        let Some(file) = ctx.first_file_of(FileKind::Image) else {
            return Ok(ToolResult::failure(&step.id, self.name(), "no image attached",
                started.elapsed().as_millis()));
        };
        // ... do the work (call ctx.engine, spawn_blocking, etc.) ...
        Ok(ToolResult {
            step_id: step.id.clone(), task: self.name().into(), ok: true,
            data: serde_json::json!({ "reading": 24.6, "unit": "bar" }),
            error: None, warning: None, elapsed_ms: started.elapsed().as_millis(),
        })
    }
}
```

**3. Wire it** — `src/tools/mod.rs`: add `pub mod gauge;` and one line in
`default_registry()`:

```rust
registry.register(Box::new(gauge::GaugeTool));
```

That's it. The validator, executor, quality check, compiler, UI and tests need
**no change** — they all work off the trait and the registry.

**4. (optional) Test it** — `tests/tools_smoke.rs` already iterates
`TASK_REGISTRY` and asserts every task has a registered tool, so your new task is
covered by that loop automatically. Add a specific test if the logic warrants it.

---

## Add a new validation rule

`src/planner/registry.rs`, inside `validate_plan`. Push a human-readable string
onto `errors` when the plan violates your rule. Example — cap plan length:

```rust
if plan.steps.len() > 12 {
    errors.push(format!("plan has {} steps; the maximum is 12", plan.steps.len()));
}
```

Add a test in `tests/registry.rs` that builds a bad plan and asserts your message
appears. The planner's retry loop will feed the message back to the model
automatically.

---

## Add a new pipeline stage / `StepEvent`

**1.** `src/events.rs` — a new variant:

```rust
RedactingPii { count: usize },
```

**2.** `src/pipeline.rs` — emit it at the right point in `run_turn`:

```rust
sink.emit(StepEvent::RedactingPii { count: n });
```

**3.** `src/types.ts` — add it to the `StepEvent` union so the frontend
type-checks:

```ts
| { stage: "redacting_pii"; count: number }
```

**4.** `src/components/Stepper.tsx` — decide where it sits on the strip
(`positionFor`) and, if it's a visible step, add it to `STAGES`.

---

## Change the model tier logic

`src-tauri/src/bootstrap/models.rs`, `recommend_models(hw: &HardwareInfo)`. It's
a plain `if`/`else if` over `hw.vram_gb` and `hw.total_ram_gb` returning a
`ModelPlan`. The `BootstrapGate` dropdown options are `LLM_CHOICES` in
`src/components/BootstrapGate.tsx`.

---

## Add a UI panel

New file in `src/components/`, import it in `src/App.tsx`, give it the state it
needs (`events`, `report`, `plan`, …). Styling: add classes to `src/App.css`
(it's one flat stylesheet with CSS variables at the top).

---

## Swap an implementation behind a seam

Some parts are deliberately behind a small interface so the implementation can
change without touching callers:

| Seam | Where | Swap in… |
|---|---|---|
| `ProgressSink` | `src/events.rs` | a different transport (websocket, log file) |
| `PlanSource` | `src/planner/planner.rs` | a different model, a canned plan, a mock |
| `Tool` | `src/tools/mod.rs` | any capability |
| the vector store | `src/tools/rag.rs` — `ensure_ingested` / `search` are the only two functions that touch LanceDB | a different vector DB, or back to a plain cosine loop |

---

## Wire the HITL resume path (currently a stub)

Today, when the planner exhausts its retries the run returns
`TurnOutcome::AwaitingUser { errors, plan_json }` and the UI shows `HitlModal`
with an editable plan — but there's no command to *run* the edited plan.

To finish it:

1. `src-tauri/src/lib.rs` — a `resume_turn(session_id, plan_json, on_event)`
   command that deserialises the edited `Plan`, runs `validate_plan` once more,
   and if it passes, calls a new `workbench_core::run_plan(engine, registry,
   config, session_id, prompt, uploads, plan, sink)` that skips stages 1–2 and
   starts at `execute_plan`.
2. `src/lib/pipeline.ts` — a `resumeTurn(...)` wrapper.
3. `src/components/HitlModal.tsx` — an "Approve & run" button that calls it.

The pieces (`validate_plan`, `execute_plan`, the `Channel` plumbing) already
exist; this is wiring, not new machinery.
