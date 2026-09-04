//! `search_knowledge` — retrieval over the standing SOP / safety-manual KB.
//!
//! Backed by an embedded **LanceDB** table (`sop_kb`) under `data/lancedb/`.
//!
//! Flow: on first use, every `.md` / `.txt` / `.pdf` in `kb_dir` is split into
//! ~800-token overlapping chunks, each embedded via Ollama `nomic-embed-text`
//! (`keep_alive = 0`), and written to the table as `{id, text, source, vector}`.
//! A query embeds the same way and LanceDB returns the nearest rows by **cosine**
//! distance. A sidecar file records which embedding model built the table; if it
//! changes, the table is dropped and rebuilt.

use std::path::Path;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::database::CreateTableMode;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, DistanceType, Table};

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::engine::OllamaEngine;
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// ~800 tokens of English ≈ this many characters.
const CHUNK_CHARS: usize = 1_100;
const CHUNK_OVERLAP: usize = 150;
const DEFAULT_TOP_K: usize = 5;
const TABLE: &str = "sop_kb";
/// Sidecar file (next to the LanceDB dir) naming the embedding model the table
/// was built with, so a model change triggers a rebuild.
const MODEL_MARKER: &str = "sop_kb.model";

fn vs_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::VectorStore(e.to_string())
}

/// Split text into overlapping windows, preferring to break on a newline or
/// sentence end near the window edge.
fn chunk_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.replace("\r\n", "\n").chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let hard_end = (start + CHUNK_CHARS).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            if let Some(pos) = chars[start..hard_end]
                .iter()
                .rposition(|&c| c == '\n' || c == '.')
            {
                if pos > CHUNK_CHARS / 2 {
                    end = start + pos + 1;
                }
            }
        }
        let piece: String = chars[start..end].iter().collect();
        if !piece.trim().is_empty() {
            chunks.push(piece.trim().to_string());
        }
        if end >= chars.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP);
    }
    chunks
}

/// Embed a batch of strings in **one** request.
///
/// Batching matters more than it looks: the embedding model is called with
/// `keep_alive = 0` so it never co-resides with the resident LLM, which means one
/// request per chunk would load and unload the weights once per chunk. A KB of a
/// few hundred chunks turned that into minutes of pure model-loading churn.
pub(crate) async fn embed_many(engine: &OllamaEngine, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
    use ollama_rs::generation::parameters::KeepAlive;

    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let expected = texts.len();

    let request = GenerateEmbeddingsRequest::new(
        engine.embed_model().to_string(),
        EmbeddingsInput::Multiple(texts),
    )
    .keep_alive(KeepAlive::UnloadOnCompletion);

    let response = engine
        .client()
        .generate_embeddings(request)
        .await
        .map_err(|e| CoreError::VectorStore(format!("embedding request failed: {e}")))?;

    if response.embeddings.len() != expected {
        return Err(CoreError::VectorStore(format!(
            "embedding count mismatch: asked for {expected}, got {}",
            response.embeddings.len()
        )));
    }
    Ok(response.embeddings)
}

/// Embed a single string.
pub(crate) async fn embed(engine: &OllamaEngine, text: &str) -> Result<Vec<f32>> {
    embed_many(engine, vec![text.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::VectorStore("embedding response was empty".into()))
}

/// Cosine similarity, for ranking without a round trip to the vector store.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Reduce a long evidence blob to the passages that actually bear on `query`.
///
/// Attachments are frequently far larger than the context window. Passing the
/// whole thing means the tail is silently truncated — which is how a fee table on
/// page 20 becomes "no relevant information found" — and makes prefill crawl on a
/// CPU-only host. Chunk, embed once, keep the best `top_k`, restore reading order.
///
/// Falls back to a plain head-truncation if embedding is unavailable, so this can
/// never fail a turn outright.
pub(crate) async fn select_relevant(
    engine: &OllamaEngine,
    query: &str,
    text: &str,
    top_k: usize,
    budget_chars: usize,
) -> String {
    if text.chars().count() <= budget_chars {
        return text.to_string();
    }

    let chunks = chunk_text(text);
    if chunks.len() <= 1 {
        return text.chars().take(budget_chars).collect();
    }

    let mut inputs = Vec::with_capacity(chunks.len() + 1);
    inputs.push(query.to_string());
    inputs.extend(chunks.iter().cloned());

    let vectors = match embed_many(engine, inputs).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "evidence ranking unavailable; truncating instead");
            return text.chars().take(budget_chars).collect();
        }
    };

    let (query_vec, chunk_vecs) = vectors.split_at(1);
    let mut ranked: Vec<(usize, f32)> = chunk_vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (i, cosine(&query_vec[0], v)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    // Take the best chunks up to the budget, then put them back in document order
    // so the excerpt still reads coherently.
    let mut picked: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for (idx, _) in ranked.into_iter().take(top_k) {
        let len = chunks[idx].chars().count();
        if used + len > budget_chars && !picked.is_empty() {
            continue;
        }
        used += len;
        picked.push(idx);
    }
    picked.sort_unstable();

    picked
        .into_iter()
        .map(|i| chunks[i].as_str())
        .collect::<Vec<_>>()
        .join("\n…\n")
}

/// One KB chunk ready to insert.
struct Row {
    id: String,
    text: String,
    source: String,
    vector: Vec<f32>,
}

/// Read + chunk + embed every supported file in `kb_dir`.
///
/// All chunks are embedded in a single batched request — see [`embed_many`].
async fn build_rows(engine: &OllamaEngine, kb_dir: &Path) -> Result<Vec<Row>> {
    let mut pending: Vec<(String, String, usize)> = Vec::new(); // (source, text, index)
    let Ok(entries) = std::fs::read_dir(kb_dir) else {
        return Ok(Vec::new());
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let source = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("kb")
            .to_string();

        let raw = match ext.as_str() {
            "md" | "txt" => std::fs::read_to_string(&path).unwrap_or_default(),
            "pdf" => crate::tools::document::extract_pdf_plain(&path).unwrap_or_default(),
            _ => continue,
        };

        for (i, chunk) in chunk_text(&raw).into_iter().enumerate() {
            pending.push((source.clone(), chunk, i));
        }
    }

    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let vectors = embed_many(engine, pending.iter().map(|(_, t, _)| t.clone()).collect()).await?;

    Ok(pending
        .into_iter()
        .zip(vectors)
        .map(|((source, text, i), vector)| Row {
            id: format!("{source}#{i}"),
            text,
            source,
            vector,
        })
        .collect())
}

/// Turn the rows into a single Arrow `RecordBatch` with a fixed-size-list vector
/// column of width `dim`.
fn rows_to_batch(rows: &[Row], dim: i32) -> Result<RecordBatch> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            false,
        ),
    ]));

    let ids = StringArray::from(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>());
    let texts = StringArray::from(rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>());
    let sources = StringArray::from(rows.iter().map(|r| r.source.clone()).collect::<Vec<_>>());
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.vector.iter().map(|x| Some(*x)).collect::<Vec<_>>())),
        dim,
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(ids),
            Arc::new(texts),
            Arc::new(sources),
            Arc::new(vectors),
        ],
    )
    .map_err(vs_err)
}

/// Open the `sop_kb` table, (re)building it from `kb_dir` if it is missing or was
/// built with a different embedding model.
///
/// **Output:** an open [`Table`], or `Ok(None)` if the KB directory has no
/// ingestable files.
pub async fn ensure_ingested(
    engine: &OllamaEngine,
    kb_dir: &Path,
    db_dir: &Path,
) -> Result<Option<Table>> {
    std::fs::create_dir_all(db_dir).ok();
    let uri = db_dir.to_string_lossy().replace('\\', "/");
    let db: Connection = lancedb::connect(&uri).execute().await.map_err(vs_err)?;

    let marker = db_dir.join(MODEL_MARKER);
    let current_model = std::fs::read_to_string(&marker).ok();
    let want_model = engine.embed_model().to_string();

    let has_table = db
        .table_names()
        .execute()
        .await
        .map_err(vs_err)?
        .iter()
        .any(|t| t == TABLE);

    if has_table && current_model.as_deref() == Some(want_model.as_str()) {
        return Ok(Some(db.open_table(TABLE).execute().await.map_err(vs_err)?));
    }

    // Need to (re)build. `CreateTableMode::Overwrite` drops any existing table as
    // part of the same call, so a stale (wrong-embedding-model) table is replaced
    // without a separate `drop_table`.
    let rows = build_rows(engine, kb_dir).await?;
    if rows.is_empty() {
        tracing::warn!(?kb_dir, "knowledge base has no ingestable files");
        return Ok(None);
    }

    let dim = rows[0].vector.len() as i32;
    let batch = rows_to_batch(&rows, dim)?;

    db.create_table(TABLE, vec![batch])
        .mode(CreateTableMode::Overwrite)
        .execute()
        .await
        .map_err(vs_err)?;
    std::fs::write(&marker, &want_model).ok();
    tracing::info!(chunks = rows.len(), "knowledge base ingested into LanceDB");

    Ok(Some(db.open_table(TABLE).execute().await.map_err(vs_err)?))
}

/// Nearest `top_k` chunks to `query_vec` by cosine distance.
async fn search(
    table: &Table,
    query_vec: Vec<f32>,
    top_k: usize,
) -> Result<Vec<(String, String, f32)>> {
    let batches: Vec<RecordBatch> = table
        .query()
        .nearest_to(query_vec)
        .map_err(vs_err)?
        .distance_type(DistanceType::Cosine)
        .limit(top_k)
        .execute()
        .await
        .map_err(vs_err)?
        .try_collect()
        .await
        .map_err(vs_err)?;

    let mut hits = Vec::new();
    for batch in &batches {
        let text = batch
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let source = batch
            .column_by_name("source")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let dist = batch
            .column_by_name("_distance")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

        let (Some(text), Some(source)) = (text, source) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            // Cosine distance is 1 - similarity; report the similarity as score.
            let score = dist.map(|d| 1.0 - d.value(i)).unwrap_or(0.0);
            hits.push((text.value(i).to_string(), source.value(i).to_string(), score));
        }
    }
    Ok(hits)
}

/// `search_knowledge` tool.
pub struct RagTool;

#[async_trait::async_trait]
impl Tool for RagTool {
    fn name(&self) -> &'static str {
        "search_knowledge"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();
        let db_dir = ctx.config.lancedb_dir();

        let table = match ensure_ingested(ctx.engine, &ctx.config.kb_dir, &db_dir).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Ok(ToolResult {
                    step_id: step.id.clone(),
                    task: self.name().into(),
                    ok: true,
                    data: serde_json::json!({ "chunks": [], "query": ctx.prompt }),
                    error: None,
                    warning: Some("knowledge base is empty (no documents ingested)".into()),
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
            Err(e) => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    e.to_string(),
                    started.elapsed().as_millis(),
                ))
            }
        };

        let query = step
            .args
            .get("query")
            .and_then(|q| q.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(ctx.prompt)
            .to_string();

        let query_vec = match embed(ctx.engine, &query).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    e.to_string(),
                    started.elapsed().as_millis(),
                ))
            }
        };

        let top_k = step
            .args
            .get("top_k")
            .and_then(|k| k.as_u64())
            .map(|k| k as usize)
            .unwrap_or(DEFAULT_TOP_K);

        let hits = match search(&table, query_vec, top_k).await {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    e.to_string(),
                    started.elapsed().as_millis(),
                ))
            }
        };

        let chunks: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|(text, source, score)| {
                serde_json::json!({ "text": text, "source": source, "score": score })
            })
            .collect();

        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name().into(),
            ok: true,
            data: serde_json::json!({ "chunks": chunks, "query": query }),
            error: None,
            warning: None,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_covers_text_with_overlap_and_no_empty_pieces() {
        let text = "para one.\n".repeat(400);
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 3, "expected multiple chunks, got {}", chunks.len());
        assert!(chunks.iter().all(|c| !c.trim().is_empty()));
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS + 8));
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let chunks = chunk_text("just a short SOP note about flange torque.");
        assert_eq!(chunks.len(), 1);
    }
}
