# 01 · Overview

## The problem

A refinery inspector comes back from the field with a phone full of photos, a
voice memo, and a scanned inspection sheet. They want a written assessment that
also checks what they saw against the plant's Standard Operating Procedures
(SOPs). The catch: the refinery is **air-gapped** — no cloud AI, no internet.
Everything has to run on a laptop.

So we need a program that:

1. takes a plain-English request plus attached files (audio / PDF / image),
2. figures out what tools it needs to run and in what order,
3. runs them one at a time (a constrained laptop can't run five AI models at once),
4. writes a grounded report that cites the SOPs,
5. remembers context across a session, encrypted on disk,
6. shows its work at every step so an engineer can trust it.

## The one-paragraph mental model

> A single local LLM is asked to do three small jobs with three different
> instruction sheets: **understand** the request, **plan** a sequence of tool
> calls, and **write up** the results. Between "plan" and "run", ordinary Rust
> code checks the plan is sane (every task is real, files exist, analysis comes
> after extraction). A fixed executor runs the planned tools one by one. If the
> planner can't produce a valid plan in three tries, the program stops and asks
> the human. Everything the program does is emitted as an event so the UI can
> draw a progress bar and an audit log.

That's the whole system. The rest is detail.

## Why this shape (and not a chat-style "agent")

Popular "AI agents" run an open-ended loop: *think → act → observe → think again*,
until the model decides it's done. That's flexible but:

- it can loop forever or wander,
- it's non-deterministic — hard to test, hard to explain to a plant manager,
- it needs a big, reliable model.

This project does the opposite. The model plans **once**. A plan is just data
(`{"steps": [...]}`). Plain code — not the model — decides whether the plan is
allowed to run. The number of model calls per turn is bounded and known. You can
unit-test the validator with zero AI involved (we do — see
`tests/registry.rs`). This is the blueprint's explicit requirement:
*"deterministic plan validation without runtime loops or runaway LLM
evaluations."*

## Architecture

```
┌─────────────────────────── Tauri desktop app (one .exe) ───────────────────────────┐
│                                                                                    │
│  React frontend (src/)                    Rust host (src-tauri/src/)                │
│  ────────────────────                     ─────────────────────────                 │
│  BootstrapGate   ── invoke() ──────────►  bootstrap/ollama.rs   install / detect    │
│  PromptPanel      (files as base64)       bootstrap/hardware.rs  RAM + GPU probe     │
│  Stepper                                  bootstrap/models.rs    tier pick + pulls   │
│  HitlModal                                lib.rs   submit_turn, end_session, …       │
│  ReportView       ◄── Channel<StepEvent>  events.rs  StepEvent → Tauri Channel       │
│  AuditSidebar                                    │                                   │
│                                                 ▼                                   │
│                          crates/workbench-core/  (plain Rust library, no Tauri)     │
│                          ────────────────────────────────────────────────────       │
│   engine/     one Ollama model; roles A/B/C = swap the system prompt                 │
│   planner/    TASK_REGISTRY + validate_plan()  ·  plan → validate → retry ×2 → HITL  │
│   executor/   run steps in order  ·  rule-based quality check  ·  role-C compiler    │
│   tools/      document · ocr · audio · vision · rag · analysis                       │
│   memory/     session_context.json (rolling)  ·  persistent_memory.json (AES-256)   │
│   pipeline.rs run_turn(): the whole sequence                                        │
│                                                 │                                   │
└─────────────────────────────────────────────────┼───────────────────────────────────┘
                                                  ▼
                                    Ollama server  (127.0.0.1:11434)
                                    resident LLM · moondream · nomic-embed-text
```

### Two crates, on purpose

- **`workbench-core`** is where the thinking lives. It has *no* dependency on
  Tauri, on a window, or on a UI. That means you can run the entire pipeline from
  a command-line program (`cargo run --example e2e`) and unit-test it fast.
- **`src-tauri`** is the shell: it owns the window, installs Ollama, detects
  hardware, and translates between the browser (JSON over `invoke`) and the
  library (Rust function calls).

Keeping "the logic" and "the app" in separate crates is one of the most useful
habits in this codebase. When something breaks, you almost always know which
half to look in.

### Data flows one direction per turn

```
prompt + files
   → intent (role A)
   → plan (role B)                    ─┐
   → validate_plan()  ──► errors ──────┤ retry ≤2, then HITL
   → [tool, tool, tool, …] in order   ─┘
   → ToolResult[]
   → quality check (rules)
   → FinalReport (role C)
   → append session memory
   → persist encrypted long-term memory
```

Each arrow also emits a `StepEvent` (`parsing_context`, `validating_plan`,
`executing_tool`, `synthesizing`, `done`, …) that the UI turns into the stepper
and the audit sidebar.

Next: [02 · Execution log](02-execution-log.md) — how this got built.
