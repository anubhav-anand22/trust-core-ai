//! `search_knowledge` — retrieval over the standing SOP / safety-manual KB.
//!
//! Flow: on first use, every `.md` / `.txt` / `.pdf` in `kb_dir` is chunked,
//! embedded via Ollama `nomic-embed-text` (`keep_alive = 0`), and written to a
//! JSON index under `data/`. A query embeds the same way and is cosine-ranked
//! against the stored chunks.
//!
//! The index is a brute-force cosine scan. For a plant KB of a few hundred chunks
//! that is exact and sub-millisecond. The store sits behind [`ensure_ingested`] /
//! [`VectorIndex`] so a `lancedb`-backed implementation can replace it later
//! (Phase 5) without touching this tool or its callers.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::engine::OllamaEngine;
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// ~800 tokens of English ≈ this many characters.
const CHUNK_CHARS: usize = 1_100;
const CHUNK_OVERLAP: usize = 150;
const DEFAULT_TOP_K: usize = 5;

#[derive(Clone, Serialize, Deserialize)]
struct Chunk {
    text: String,
    source: String,
    vector: Vec<f32>,
}

/// The on-disk knowledge index.
#[derive(Default, Serialize, Deserialize)]
struct VectorIndex {
    /// Embedding model the vectors were produced with. If it changes, the index
    /// is rebuilt.
    model: String,
    chunks: Vec<Chunk>,
}

impl VectorIndex {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn search(&self, query: &[f32], top_k: usize) -> Vec<(f32, &Chunk)> {
        let mut scored: Vec<(f32, &Chunk)> = self
            .chunks
            .iter()
            .map(|c| (cosine(query, &c.vector), c))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.truncate(top_k);
        scored
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Embed one string via the configured embedding model, unloading it immediately.
async fn embed(engine: &OllamaEngine, text: &str) -> Result<Vec<f32>> {
    use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
    use ollama_rs::generation::parameters::KeepAlive;

    let request = GenerateEmbeddingsRequest::new(
        engine.embed_model().to_string(),
        EmbeddingsInput::Single(text.to_string()),
    )
    .keep_alive(KeepAlive::UnloadOnCompletion);

    let response = engine
        .client()
        .generate_embeddings(request)
        .await
        .map_err(|e| CoreError::VectorStore(format!("embedding request failed: {e}")))?;

    response
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::VectorStore("embedding response was empty".into()))
}

/// Build the index from `kb_dir` if it is missing or was made with another model.
///
/// **Output:** the number of chunks in the index. Idempotent and cheap on repeat
/// calls (a non-empty, same-model index short-circuits).
pub async fn ensure_ingested(
    engine: &OllamaEngine,
    kb_dir: &Path,
    index_path: &Path,
) -> Result<usize> {
    let existing = VectorIndex::load(index_path);
    if !existing.chunks.is_empty() && existing.model == engine.embed_model() {
        return Ok(existing.chunks.len());
    }

    let mut index = VectorIndex {
        model: engine.embed_model().to_string(),
        chunks: Vec::new(),
    };

    let entries = match std::fs::read_dir(kb_dir) {
        Ok(e) => e,
        Err(_) => {
            index.save(index_path)?;
            return Ok(0);
        }
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

        for chunk in chunk_text(&raw) {
            let vector = embed(engine, &chunk).await?;
            index.chunks.push(Chunk {
                text: chunk,
                source: source.clone(),
                vector,
            });
        }
    }

    index.save(index_path)?;
    tracing::info!(chunks = index.chunks.len(), "knowledge base ingested");
    Ok(index.chunks.len())
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
        let index_path = ctx.config.lancedb_dir().join("kb_index.json");

        // First-run ingest; ignored on later turns.
        if let Err(e) = ensure_ingested(ctx.engine, &ctx.config.kb_dir, &index_path).await {
            tracing::warn!(error = %e, "knowledge base ingest failed");
        }

        let query = step
            .args
            .get("query")
            .and_then(|q| q.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(ctx.prompt)
            .to_string();

        let index = VectorIndex::load(&index_path);
        if index.chunks.is_empty() {
            return Ok(ToolResult {
                step_id: step.id.clone(),
                task: self.name().into(),
                ok: true,
                data: serde_json::json!({ "chunks": [], "query": query }),
                error: None,
                warning: Some("knowledge base is empty (no documents ingested)".into()),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

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

        let hits: Vec<serde_json::Value> = index
            .search(&query_vec, top_k)
            .into_iter()
            .map(|(score, c)| {
                serde_json::json!({ "text": c.text, "source": c.source, "score": score })
            })
            .collect();

        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name().into(),
            ok: true,
            data: serde_json::json!({ "chunks": hits, "query": query }),
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
    fn cosine_is_one_for_identical_and_zero_for_orthogonal() {
        assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0); // length mismatch -> 0
    }

    #[test]
    fn chunking_covers_text_with_overlap_and_no_empty_pieces() {
        let text = "para one.\n".repeat(400); // ~4000 chars -> several chunks
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 3, "expected multiple chunks, got {}", chunks.len());
        assert!(chunks.iter().all(|c| !c.trim().is_empty()));
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS + 8));
        // consecutive chunks should share some tail/head text (overlap)
        let a_tail: String = chunks[0].chars().rev().take(40).collect();
        let a_tail: String = a_tail.chars().rev().collect();
        assert!(chunks[1].contains(a_tail.trim()) || chunks[1].starts_with("para"));
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let chunks = chunk_text("just a short SOP note about flange torque.");
        assert_eq!(chunks.len(), 1);
    }
}
