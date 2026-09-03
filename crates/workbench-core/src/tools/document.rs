//! `parse_pdf` — digital PDF text + tables via the pure-Rust `pdfplumber` crate.
//!
//! Scope decision (see plan): **digital PDFs only**. Scanned / image-only PDFs are
//! not supported — if a page yields no text we say so plainly instead of falling
//! back to OCR or rasterisation.

use std::path::Path;

use pdfplumber::{Pdf, TableSettings, TextOptions};

use crate::engine::schemas::{FileKind, TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// `parse_pdf` tool.
pub struct DocumentTool;

#[async_trait::async_trait]
impl Tool for DocumentTool {
    fn name(&self) -> &'static str {
        "parse_pdf"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        let file = match ctx.first_file_of(FileKind::Pdf) {
            Some(f) => f,
            None => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    "no PDF file attached",
                    started.elapsed().as_millis(),
                ))
            }
        };

        let path = file.path.clone();
        // pdfplumber is synchronous and can be slow on big documents; keep it off
        // the async executor thread.
        let parsed = tokio::task::spawn_blocking(move || parse(&path))
            .await
            .map_err(|e| CoreError::Tool {
                tool: "parse_pdf".into(),
                message: e.to_string(),
            })?;

        let (text, tables, pages) = match parsed {
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

        let warning = if text.trim().is_empty() {
            Some(
                "no extractable text — this looks like a scanned PDF, which is out of scope"
                    .to_string(),
            )
        } else {
            None
        };

        Ok(ToolResult {
            step_id: step.id.clone(),
            task: self.name().into(),
            ok: true,
            data: serde_json::json!({
                "text": text,
                "tables": tables,
                "pages": pages,
                "source": file.original_name,
            }),
            error: None,
            warning,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}

/// Concatenated plain text of every page — used by the RAG ingester for PDF KB
/// documents so it can reuse the same extractor.
pub fn extract_pdf_plain(path: &Path) -> Result<String> {
    let (text, _tables, _pages) = parse(&path.to_string_lossy())?;
    Ok(text)
}

/// Open the PDF and pull text + tables from each page.
///
/// **Output:** `(all_text, tables_as_json, page_count)` where `tables_as_json` is
/// `[[[cell,…],…],…]` — a list of tables, each a list of rows, each a list of
/// cell strings.
fn parse(path: &str) -> Result<(String, serde_json::Value, usize)> {
    let err = |e: pdfplumber::PdfError| CoreError::Tool {
        tool: "parse_pdf".into(),
        message: e.to_string(),
    };

    let pdf = Pdf::open_path(path, None).map_err(err)?;

    let mut text = String::new();
    let mut all_tables: Vec<Vec<Vec<String>>> = Vec::new();
    let mut pages = 0usize;

    for page in pdf.pages() {
        let page = page.map_err(err)?;
        pages += 1;

        let page_text = page.extract_text(&TextOptions::default());
        if !page_text.trim().is_empty() {
            text.push_str(&page_text);
            text.push_str("\n\n");
        }

        for table in page.find_tables(&TableSettings::default()) {
            let rows: Vec<Vec<String>> = table
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.text.as_deref().unwrap_or("").trim().to_string())
                        .collect()
                })
                .collect();
            if rows.iter().any(|r| r.iter().any(|c| !c.is_empty())) {
                all_tables.push(rows);
            }
        }
    }

    Ok((text, serde_json::to_value(all_tables)?, pages))
}
