# 04 · The agentic pipeline — one turn, traced

This walks a single real request through the whole system, in order, with the
actual data. The request:

> **"Assess corrosion risk on pump P-101 and check it against our SOPs."**
> attachments: `inspection.pdf`, `valve.png`

This is the run captured in the execution log — you can reproduce it with
`cargo run -p workbench-core --example e2e` (see [doc 07](07-build-test-run.md)).

---

## Stage 0 — the request arrives

The React `PromptPanel` turns the two files into base64 and calls:

```ts
submitTurn({ prompt, sessionId, files: [{name:"inspection.pdf", content_base64:"JVBER…"}, …] }, onEvent)
```

`submit_turn` in `src-tauri/src/lib.rs`:

1. decodes each file, writes it to `…/uploads/<session>/inspection.pdf`, and
   classifies it by extension:
   ```rust
   InputFile { path: "…/inspection.pdf", kind: FileKind::Pdf, original_name: "inspection.pdf" }
   ```
2. builds `PipelineConfig` (paths + `llm_model: "llama3.2:3b"`, `vision_model:
   "moondream"`, `embed_model: "nomic-embed-text"`),
3. builds `OllamaEngine`, `default_registry()`, and a `ChannelSink` wrapping the
   `Channel` the UI is listening on,
4. calls `workbench_core::run_turn(...)`.

From here we're inside `crates/workbench-core/src/pipeline.rs`.

`sink.emit(StepEvent::Idle)` → the UI stepper shows "Idle".

---

## Stage 1 — understand the request (role A)

`plan_turn()` (in `planner/planner.rs`) first emits
`ParsingContext { attempt: 1 }` and calls `engine.parse_intent(prompt, uploads)`.

That builds a request to the resident model with the **role-A system prompt**:

> *You are the Intent Parser… return STRICT JSON:
> `{"intents":[…], "file_kinds":["pdf"|"image"|"audio"], "needs_knowledge":bool}`*

and the user block:

> `USER REQUEST: Assess corrosion risk on pump P-101 and check it against our SOPs.`
> `ATTACHED FILE KINDS: [pdf, image]`

Ollama is told to constrain output to the `IntentResult` JSON schema
(`FormatType::StructuredJson`). The model returns something like:

```json
{ "intents": ["assess corrosion risk on pump P-101", "check against SOPs"],
  "file_kinds": ["pdf", "image"],
  "needs_knowledge": true }
```

`serde_json::from_str::<IntentResult>(…)` parses it into a Rust struct. If it
*didn't* parse, that's `CoreError::Schema { role: "intent_parser", … }` and the
whole turn errors — but structured outputs make that rare.

---

## Stage 2 — make a plan (role B), then check it

Now the loop in `plan_turn()`:

```rust
for attempt in 1..=MAX_PLAN_ATTEMPTS {          // 3
    let plan = source.build_plan(prompt, &intent, uploads, &errors).await?;
    sink.emit(StepEvent::ValidatingPlan { attempt });
    errors = validate_plan(&plan, uploads);
    if errors.is_empty() { return Ok((intent, PlanOutcome::Ready(plan))); }
}
```

**`build_plan`** sends the resident model the **role-B system prompt**, which
includes the task registry rendered as text (so the model and the validator can
never disagree about what tasks exist):

```
- "transcribe_audio"  (stage: extract; needs an attached audio file)  — Transcribe an attached audio file…
- "parse_pdf"         (stage: extract; needs an attached pdf file)     — Extract text and tables from a digital PDF…
- "ocr_image"         (stage: extract; needs an attached image file)   — Read printed/stamped text off an image…
- "analyze_image"     (stage: extract; needs an attached image file)   — Visually inspect a photo for condition…
- "search_knowledge"  (stage: retrieve)                                — Search the SOP / safety-manual KB…
- "summarize"         (stage: analyze)                                 — Summarise the collected evidence…
- "compare_to_sop"    (stage: analyze)                                 — Compare observations against SOPs…
Rules: `task` must be one of the quoted names verbatim — no stage label or brackets.
Extraction/retrieval steps MUST come before analysis steps. List every step after its dependencies.
```

The task name is the only quoted token on each line, so a small model has an
unambiguous string to copy. `normalize_task_name()` is still a safety net: if the
model writes `"parse_pdf [extract]"` anyway, the stage suffix is stripped before
the registry lookup rather than wasting a retry.

Output is constrained to the `Plan` schema: `{"steps":[{"id","task","args","depends_on":[]}]}`.

### Attempt 1 → rejected

In the captured run the model's first plan had a stage-order or dependency
mistake. `validate_plan` returned e.g.:

```
["analysis step `s3` (`compare_to_sop`) has no preceding extraction/retrieval step"]
```

`errors` is now non-empty, so the loop goes again — and **feeds those exact
strings back** into `build_plan`:

> *YOUR PREVIOUS PLAN WAS REJECTED BY THE VALIDATOR.
> Fix ALL of these problems: - analysis step `s3` has no preceding extraction…*

### Attempts 2, 3 → still tweaking, then valid

The run needed all three attempts. On attempt 3 the plan passed every check:

```json
{ "steps": [
  { "id": "s1", "task": "search_knowledge", "args": {"query": "pump P-101 corrosion SOP"}, "depends_on": [] },
  { "id": "s2", "task": "parse_pdf",        "args": {},                                     "depends_on": [] },
  { "id": "s3", "task": "analyze_image",    "args": {},                                     "depends_on": [] },
  { "id": "s4", "task": "compare_to_sop",   "args": {}, "depends_on": ["s1", "s2", "s3"] }
]}
```

`validate_plan` runs its six checks and returns `[]`:

| check | result |
|---|---|
| every task in `TASK_REGISTRY` | ✓ all four |
| ids unique, `depends_on` resolves | ✓ s1–s4 unique, s4's deps exist |
| deps listed before dependents | ✓ s4 is last |
| stage order | ✓ s4 (`Analyze`) is preceded by s1 (`Retrieve`) and s2/s3 (`Extract`) |
| required files present | ✓ `parse_pdf` has a Pdf, `analyze_image` has an Image |
| files exist on disk | ✓ both uploads written in stage 0 |

`plan_turn` returns `PlanOutcome::Ready(plan)`.

> **If all 3 attempts had failed**, it would instead emit
> `AwaitingUser { errors, plan_json }` and `run_turn` returns
> `TurnOutcome::AwaitingUser`. The UI shows `HitlModal` with the complaints and
> the last plan. No infinite loop, ever. The user edits the plan and clicks
> **Re-run with this plan** — that calls `resume_turn`, which re-runs
> `validate_plan` (unless *Run anyway*), then jumps straight to stage 3 below via
> the shared `run_from_plan` tail. The model is not consulted again.

---

## Stage 3 — run the tools, in order

`execute_plan()` (in `executor/executor.rs`) — one `for` loop, `outputs` starts
empty:

### s1 · `search_knowledge` — 9.9 s

`RagTool::run`:
1. `ensure_ingested()` — the `sop_kb` LanceDB table doesn't exist yet, so it
   reads every file in `crates/workbench-core/kb/` (the two seed SOPs), splits
   each into ~1100-char overlapping chunks, calls `nomic-embed-text` once per
   chunk to get a 768-number vector, and writes them to
   `…/lancedb/sop_kb/` as an Arrow table `{id, text, source, vector}`.
   *(This one-time cost is why s1 took ~10 s; later turns just open the table.)*
2. embeds the query `"pump P-101 corrosion SOP"`,
3. `table.query().nearest_to(vec)?.distance_type(Cosine).limit(5)` — LanceDB
   returns the 5 nearest chunks,
4. returns `ToolResult { ok: true, data: {"chunks": [{text, source, score}, …]} }`.

`ExecutingTool{tool:"search_knowledge",index:0,total:4}` then
`ToolFinished{ok:true,elapsed_ms:9952}` are emitted. `outputs["s1"]` now holds
the chunks.

### s2 · `parse_pdf` — 45 ms

`DocumentTool::run` finds the Pdf `InputFile`, hands it to `spawn_blocking`, and
`pdfplumber` extracts the text:

```
MRPL Unit-2 - Pump P-101 External Inspection Sheet
Date: 2026-09-04  Inspector: T. Rao
1. Mechanical seal: steady clear drip, approx 12 drops/min.
2. Casing drain small-bore line: moderate scaling, pitting about 1.5 mm deep.
…
```

`data: {"text": "…", "tables": [], "pages": 1}`. `outputs["s2"]` set.

### s3 · `analyze_image` — 52 s

`VisionTool::run` base64-encodes `valve.png` and calls
`engine.analyze_image(b64, INSPECTION_QUESTION)`. That hits Ollama with
`model: "moondream"`, the image, and `keep_alive: 0`. moondream (running on CPU
here — hence 52 s) returns a description of visible rust/staining/damage.
`data: {"observation": "…"}`.

### s4 · `compare_to_sop` — 33 s

`AnalysisTool::compare_to_sop().run` gathers the `data` of its `depends_on`
(`s1`, `s2`, `s3`) from `outputs`, renders them to a text block, and sends it to
the **resident model** (not a new model) with:

> *Compare the observed conditions against the SOP passages. List each point as
> COMPLIANT or DEVIATION with a one-line reason and the SOP source name.*

`data: {"text": "DEVIATION: seal drip 12/min exceeds SOP-ROT-007 S1 threshold… "}`.

---

## Stage 4 — quality check (rules, no AI)

`assert_sane(&plan, &results, None)` in `executor/quality.rs`:

- all 4 steps produced a result ✓
- no tool returned `ok: false` ✓
- `search_knowledge` was planned and returned 5 chunks (>0) ✓
- `parse_pdf` produced non-empty text ✓

`QualityReport { passed: true, checks: [...] }`.
`sink.emit(StepEvent::QualityCheck { passed: true })`.

---

## Stage 5 — write the report (role C)

`compiler::compile(...)` emits `Synthesizing` and calls
`engine.compile_report(prompt, results, session_blob, facility_blob)`:

> **role-C system prompt:** *You are the Output Compiler. Synthesise into STRICT
> JSON `{summary, findings[], citations[], safety_notes[], degraded}`. Never
> invent measurements or SOP clauses not in the tool outputs.*

The user block is the prompt + every `ToolResult` serialised + the session
summary (empty, first turn) + facility memory (empty). Output constrained to the
`FinalReport` schema. Result:

```json
{
  "summary": "Corrosion risk on pump P-101 is moderate due to coating loss and scaling. Seal replacement is recommended…",
  "findings": [
    "moderate scaling on the casing drain small-bore line",
    "coating loss over a 90 mm patch near the discharge flange"
  ],
  "citations": [
    "SOP-ROT-007_centrifugal_pump_condition_monitoring.md",
    "SOP-COR-014_external_corrosion_inspection.md"
  ],
  "safety_notes": [
    "Product-wetted external corrosion … is a loss-of-containment precursor - stop, secure the area, and notify the shift supervisor…"
  ],
  "degraded": false
}
```

The citations come from the `source` field of the RAG chunks the model actually
used; the safety note is lifted verbatim from SOP-COR-014's text. `degraded`
stays `false` because no tool failed and the quality check passed.

If role C had returned unparseable JSON twice, `compile()` would fall back to a
deterministic report built straight from the `ToolResult`s, with `degraded: true`.

---

## Stage 6 — memory

```rust
let turn = TurnSummary {
    user_prompt:    "Assess corrosion risk on pump P-101 …",
    plan_summary:   "search_knowledge → parse_pdf → analyze_image → compare_to_sop",
    tool_summary:   "4 tool(s), 4 ok",
    report_summary: "Corrosion risk on pump P-101 is moderate …",
};
session.append_turn(turn, engine, &config.session_path()).await;   // → session_context.json
persist_long_term(config, &session);                               // → persistent_memory.json (AES-256-GCM)
```

`session_context.json` is now one turn long. `persistent_memory.json` on disk is
base64 gibberish; decrypting it yields `history_digest: "Corrosion risk on pump
P-101 is moderate …"`. Next turn, role A/B/C get this as context, and role C can
say "as noted earlier this session…".

`sink.emit(StepEvent::Done { report_json })`. The React `App` parses it into a
`FinalReport` and renders `ReportView`.

---

## The event tape the UI saw

```
idle
parsing_context   attempt=1
validating_plan   attempt=1
validating_plan   attempt=2
validating_plan   attempt=3
executing_tool    search_knowledge  0/4
tool_finished     search_knowledge  ok  9952 ms
executing_tool    parse_pdf         1/4
tool_finished     parse_pdf         ok  45 ms
executing_tool    analyze_image     2/4
tool_finished     analyze_image     ok  52497 ms
executing_tool    compare_to_sop    3/4
tool_finished     compare_to_sop    ok  32920 ms
quality_check     passed=true
synthesizing
done              <FinalReport JSON>
```

`Stepper.tsx` walks the strip as these arrive; `AuditSidebar.tsx` builds the
timing table from the `tool_finished` events.

Next: [05 · Rust for this codebase](05-rust-for-this-codebase.md).
