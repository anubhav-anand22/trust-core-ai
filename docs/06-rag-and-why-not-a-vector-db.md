# 06 · RAG, embeddings, and the vector store

## What RAG is, in one picture

An LLM only knows what was in its training data. It has never seen *your*
refinery's SOPs. **Retrieval-Augmented Generation** fixes that:

```
                        ┌─────────────────────────┐
 user question ───────► │ 1. turn it into a vector│
                        └───────────┬─────────────┘
                                    ▼
        ┌───────────────────────────────────────────────┐
        │ 2. find the few KB passages whose vectors are  │
        │    closest to the question's vector            │
        └───────────────────────┬───────────────────────┘
                                ▼
        ┌───────────────────────────────────────────────┐
        │ 3. give the model:  question + those passages  │
        │    "answer using only this"                    │
        └───────────────────────────────────────────────┘
```

The model now answers *from* your documents, and because you know which passages
you fed it, the answer is **citable**. In this project that's the
`search_knowledge` tool feeding chunks to `compare_to_sop` and the role-C
compiler; the report's `citations` are the `source` filenames of the chunks that
were used.

## Embeddings

An **embedding** is a list of numbers (768 of them, for `nomic-embed-text`) that
represents the *meaning* of a piece of text. The model is trained so that texts
with similar meaning land close together in that 768-dimensional space.

```
"seal is leaking badly"        → [0.12, -0.04, 0.88, ... ]  ┐ close together
"gland packing weeping"        → [0.10, -0.02, 0.85, ... ]  ┘
"bearing temperature is fine"  → [-0.7,  0.3,  0.01, ... ]  ← far away
```

In `tools/rag.rs`:

```rust
async fn embed(engine: &OllamaEngine, text: &str) -> Result<Vec<f32>> {
    let request = GenerateEmbeddingsRequest::new(
        engine.embed_model().to_string(),                 // "nomic-embed-text"
        EmbeddingsInput::Single(text.to_string()),
    ).keep_alive(KeepAlive::UnloadOnCompletion);          // free it right after
    let response = engine.client().generate_embeddings(request).await?;
    response.embeddings.into_iter().next()
        .ok_or_else(|| CoreError::VectorStore("empty embedding".into()))
}
```

## Cosine similarity

To find "close" vectors you need a distance. This project uses **cosine
similarity**: the cosine of the angle between two vectors.

```
cos(θ) = (A · B) / (‖A‖ · ‖B‖)

A · B      = Σ Aᵢ·Bᵢ            (dot product)
‖A‖        = √(Σ Aᵢ²)           (length / magnitude)
```

- `1.0` → same direction → same meaning
- `0.0` → perpendicular → unrelated
- `-1.0` → opposite

It ignores magnitude and only cares about *direction*, which is what you want for
meaning ("a long paragraph about corrosion" and "a short note about corrosion"
should match). LanceDB computes it for us when the query sets
`distance_type(DistanceType::Cosine)`; it returns a `_distance` column
(`1 − similarity`), and we report `score = 1 − _distance`.

## Chunking

You don't embed a whole 3-page SOP as one vector — the meaning would be a blur.
You split it into ~800-token overlapping **chunks** so retrieval returns the one
relevant paragraph:

```rust
const CHUNK_CHARS: usize = 1_100;   // ~800 tokens of English
const CHUNK_OVERLAP: usize = 150;   // repeat 150 chars so a sentence split across
                                    // a boundary still lands whole in one chunk
```

`chunk_text` walks the document in `CHUNK_CHARS` windows, but backs each cut up
to the nearest newline or `.` so chunks end on sentence boundaries. Tested in
`rag.rs`'s `#[cfg(test)] mod tests`.

## The store: embedded LanceDB

The chunks + vectors live in an on-disk **LanceDB** table, `sop_kb`, under
`data/lancedb/`. LanceDB is an embedded vector database (no server) built on
Apache Arrow.

### Ingest (first use, or after the embedding model changes)

```rust
pub async fn ensure_ingested(engine, kb_dir, db_dir) -> Result<Option<Table>> {
    let db = lancedb::connect(&uri).execute().await?;
    // rebuild if the table is missing or a sidecar file says it was built
    // with a different embedding model
    if has_table && current_model == want_model {
        return Ok(Some(db.open_table("sop_kb").execute().await?));
    }
    let rows = build_rows(engine, kb_dir).await?;         // chunk + embed every KB file
    let batch = rows_to_batch(&rows, dim)?;               // Arrow RecordBatch
    db.create_table("sop_kb", vec![batch])               // Vec<RecordBatch>: Scannable
        .mode(CreateTableMode::Overwrite)                 // replaces a stale table in one call
        .execute().await?;
    Ok(Some(db.open_table("sop_kb").execute().await?))
}
```

The Arrow schema:

| column | Arrow type |
|---|---|
| `id` | `Utf8` |
| `text` | `Utf8` |
| `source` | `Utf8` |
| `vector` | `FixedSizeList<Float32, dim>` — `dim` read from the first embedding |

### Query

```rust
let batches: Vec<RecordBatch> = table
    .query()
    .nearest_to(query_vec)?
    .distance_type(DistanceType::Cosine)
    .limit(top_k)                       // default 5
    .execute().await?
    .try_collect().await?;
```

LanceDB streams back `RecordBatch`es with the matched rows plus a `_distance`
column. `search()` pulls `text`, `source` and `_distance` out of the Arrow arrays
and returns `Vec<(text, source, score)>`, which `RagTool::run` turns into the
`{"chunks":[…]}` JSON the rest of the pipeline reads.

## Why this was *originally* a hand-rolled cosine loop

The first implementation was ~40 lines: store `{text, source, vector}` in a JSON
file, and at query time compute cosine against every row in a `for` loop. It was
swapped to LanceDB on request. The tradeoff, so you understand both sides:

| | Brute-force cosine over JSON | Embedded LanceDB |
|---|---|---|
| dependencies added | ~0 | **~238 crates** (Arrow + DataFusion + Lance + object_store) |
| cold build time added | ~0 | **~15–25 min** the first time |
| result quality at ≤ a few thousand chunks | **exact** nearest neighbours | exact (falls back to flat scan) or approximate if an ANN index is built |
| result quality / speed at 10⁴–10⁶+ chunks | slow (linear scan) | **fast** (ANN index: IVF-PQ / HNSW) |
| on-disk format | one JSON blob | columnar Lance files, versioned, filterable with SQL predicates |
| "what's your vector database?" | "a `for` loop" | "LanceDB" |

The engineering reality: for a plant SOP corpus (hundreds–low-thousands of
chunks) the two are **functionally identical** — a linear scan of a few hundred
768-float vectors is sub-millisecond and returns the *exact* top-k. An ANN index
only starts to matter at ~10⁴+ vectors, where scanning gets slow and you trade a
little accuracy for a lot of speed.

So the brute-force version was the pragmatic choice *during development* (keep the
build loop fast while iterating on everything else). LanceDB is the right choice
for the **product**: it's what you'd actually ship, it scales if the KB grows to
every manual in the plant, it stores the data in a real columnar format you can
query and filter, and it's a credible answer when someone asks about the
architecture.

## When you genuinely need a vector DB (and when you don't)

**Use one** when: the corpus is large (10⁴+ chunks) or growing; you need metadata
filtering ("only SOPs revised after 2024"); you need the index to persist and be
shared; you want hybrid (keyword + vector) search.

**A loop is fine** when: the corpus is small and static; it's a prototype; you're
embedding on the fly anyway; simplicity and build speed matter more than the last
few milliseconds.

Next: [07 · Build, test, run](07-build-test-run.md).
