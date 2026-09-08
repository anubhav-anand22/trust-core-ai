# 10 · Documents, tables, and where a fact gets lost

A user attached a payment-services rate card and asked what fee applies to a
₹1000 credit-card payment. The answer (2%) was in a table. The model said *"no
relevant information available."* This doc traces the whole path a document takes
and the three distinct ways a fact can vanish along it.

## The path

```
 upload  ──►  parse_pdf  ──►  ToolResult.data { text, tables, tables_text }
                                     │
                                     ▼
                          executor stores it in `outputs[step_id]`
                                     │
              ┌──────────────────────┴───────────────────────┐
              ▼                                               ▼
   analysis step (summarize /                    role C (compile_report)
   compare_to_sop)                                           │
   collect() → select_relevant() →                render each result to text,
   engine.analyze()                               narrow, engine.generate_json()
              │                                               │
              └───────────────► outputs ──────────────────────┘
                                     ▼
                              FinalReport
```

Three places on that path can each independently lose a fact.

## Loss #1 — extracted, then dropped

`parse_pdf` (`tools/document.rs`) returns:

```rust
data: json!({
    "text":        text,          // layout-aware page text
    "tables":      tables,        // [[[cell,…],…],…]  structured
    "tables_text": tables_text,   // flat: "col | col | col", header repeated
    "pages": pages,
    "source": file.original_name,
})
```

For a long time only `text` was read. `collect()` in `tools/analysis.rs` did:

```rust
if let Some(t) = v.get("text") { return t }   // ← always taken for a PDF
```

so `tables` — a `grep` of the whole repo finds *one* reference to that field, and
it is a string in a prompt — never reached the model. Every table in every PDF
was silently discarded.

**Fix:** `parse_pdf` also emits `tables_text`, a flat rendering where the header
row is repeated above every data row:

```
[table 1]
Mode | MDR | GST
Mode | MDR | GST  ▸  Credit card | 2.00% | 18%
Mode | MDR | GST  ▸  UPI | 0.00% | NA
```

The repeated header means a retrieval chunk containing only the *middle* of a
long table still names its columns. `collect()` now appends this after `text`,
and prefixes each block with `[source]` so the final report can say which file a
finding came from.

## Loss #2 — never extracted

Two `pdfplumber` defaults worked against us:

- `TableSettings::default()` is `Strategy::Lattice` — it needs **drawn ruled
  lines**. A zebra-striped or plain-whitespace rate card has none, so `find_tables`
  returns nothing. Fix: `parse()` tries Lattice, and per page where that finds
  nothing, retries with `Strategy::Stream` (detects tables from text alignment).
  The log line `parse_pdf: extracted … stream_fallback_pages=N` says how often
  the fallback fired.
- `TextOptions::default()` has `layout: false`, which concatenates words by
  spatial order with no column structure — `"Credit card 2.00 0.00 18%"`. Fix:
  `layout: true`, which preserves the whitespace grid.

Genuinely scanned PDFs (no text layer at all) are still out of scope — the log
says `parse_pdf: no extractable text` and the run degrades with a warning.

## Loss #3 — in the prompt, past the cut

Even with the table extracted and rendered, it has to *fit*. Two problems here.

**Role C dumped the whole document.** `compile_report` did
`serde_json::to_string_pretty(results)` — the entire raw text, plus base64, plus
timings — into an 8192-token window. Commit `5c0a3b6` fixed this for the analysis
step (via retrieval) but never for role C. And `serde_json` runs with
`preserve_order` in this build, so `text` (huge) serialised *before* `tables`,
putting the tables at the very end of an already-overflowing prompt — first
thing lost. Fix: role C now renders each result to readable text, caps each one,
puts tables right after their prose, and runs the whole blob through
`select_relevant` if it is still over budget.

**Chunking split decimals.** `chunk_text` broke a window on `\n` *or* `.`. The
last `.` before a window edge, in a numeric table, is usually a decimal point:
`2.00%` → `"… 2."` + `"00% …"`. Fix: break only on line boundaries. A rendered
table row is one line, so it stays whole. (`chunking_never_severs_a_decimal` in
`rag.rs` guards this.)

**Half the budget was unreachable.** `select_relevant` capped at `top_k` (8)
chunks regardless of the char budget: 8 × 1100 ≈ 8.8k against a 16k budget. Now
`top_k` is a floor and the loop fills the budget.

## Embedding prefixes

`nomic-embed-text` is trained with task-instruction prefixes and is measurably
worse without them. `rag.rs` now embeds a passage as `search_document: <text>`
and a query as `search_query: <text>`. The KB sidecar marker includes
`nomic-prefix-v1`, so an existing index rebuilds once to re-embed under the new
scheme.

## Fast vs Deep

| | Fast (default) | Deep |
|---|---|---|
| how | retrieve the passages nearest the question, answer from those | split the whole doc into gap-free `windows()`, analyse each, reduce the partials |
| cost | one analysis call | one call per window + one reduce — minutes on CPU |
| misses | anything the embedding ranked low | nothing |
| progress | one step event | one `ExecutingTool` event per window; cancellable between windows |

Deep is the checkbox in the prompt panel. Use it when a specific figure in a long
document must not be missed and you can wait.

## The prompts stopped saying "out of scope"

All three role prompts hard-coded *"offline industrial inspection assistant"*.
Given a payments fee schedule and asked for `safety_notes` and SOP clauses, *"no
relevant information"* is a natural completion. They now say "analysis assistant
… industrial *and* business documents"; role C is told to quote any rate, fee or
percentage verbatim and show the arithmetic; and `analyze()` has a real
`.system(...)` carrying that instruction instead of it being a tail sentence on
the user message competing with a wall of evidence.

## Reading the log

| Line | Says |
|---|---|
| `parse_pdf: extracted text_chars=… tables=… tables_text_chars=… stream_fallback_pages=…` | which of losses #1/#2 you are looking at: `tables=0` → not detected; `tables>0, tables_text_chars=0` → rendering bug; both non-zero → it reached the evidence |
| `evidence exceeds budget; selecting relevant passages chunks=…` | Fast-mode narrowing engaged |
| `evidence narrowed kept_chunks=… dropped_chars=…` | how much Fast mode dropped |
| `deep read: mapping instruction over the whole document windows=…` | Deep mode, and how many passes |
| `prompt likely exceeds the context window` + `role=output_compiler` | loss #3 — role C still overflowing (should not happen now) |
