//! `parse_pdf` — digital PDF text + tables via the pure-Rust `pdfplumber` crate.
//!
//! Scope decision (see plan): **digital PDFs only**. Scanned / image-only PDFs are
//! not supported — if a page yields no text we say so plainly instead of falling
//! back to OCR or rasterisation.

use std::path::Path;

use pdfplumber::{Pdf, Strategy, TableSettings, TextOptions};

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

        let file = match ctx.file_for(step, FileKind::Pdf) {
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

        let Parsed {
            text,
            tables,
            tables_text,
            pages,
        } = match parsed {
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
                // Structured, for anything that wants the grid.
                "tables": tables,
                // Flat pipe-delimited rendering with the header repeated on every
                // table. This is what the analysis step actually reads — the
                // structured `tables` field was being silently dropped, which is
                // how a fee in a rate table came back as "no relevant information".
                "tables_text": tables_text,
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
    Ok(parse(&path.to_string_lossy())?.text)
}

/// Everything one PDF yields.
struct Parsed {
    /// Concatenated page text (layout-aware: column whitespace is preserved).
    text: String,
    /// Structured tables: `[[[cell,…],…],…]`.
    tables: serde_json::Value,
    /// Flat rendering of the tables — pipe-delimited rows, header repeated per
    /// table, tables separated by a blank line. Empty when there are no tables.
    tables_text: String,
    pages: usize,
}

/// Open the PDF and pull text + tables from each page.
fn parse(path: &str) -> Result<Parsed> {
    let err = |e: pdfplumber::PdfError| CoreError::Tool {
        tool: "parse_pdf".into(),
        message: e.to_string(),
    };

    let pdf = Pdf::open_path(path, None).map_err(err)?;

    // `layout: true` keeps the horizontal whitespace that separates columns.
    // With the default (`false`) every row collapses to space-joined words —
    // "Credit Card 2% 1.90% Nil" — and the model cannot tell which number is
    // the MDR it was asked for.
    let text_opts = TextOptions {
        layout: true,
        ..Default::default()
    };

    // Lattice needs ruled lines. Many real rate cards are zebra-striped or plain
    // whitespace with no borders, and Lattice finds nothing on those. If a page
    // yields no tables, try again with Stream (text-alignment detection).
    let lattice = TableSettings::default();
    let stream = TableSettings {
        strategy: Strategy::Stream,
        ..Default::default()
    };

    let mut text = String::new();
    let mut all_tables: Vec<Vec<Vec<String>>> = Vec::new();
    let mut pages = 0usize;
    let mut stream_pages = 0usize;

    for page in pdf.pages() {
        let page = page.map_err(err)?;
        pages += 1;

        let page_text = page.extract_text(&text_opts);
        if !page_text.trim().is_empty() {
            text.push_str(&page_text);
            text.push_str("\n\n");
        }

        let mut found = collect_tables(&page, &lattice);
        if found.is_empty() {
            let s = collect_tables(&page, &stream);
            if !s.is_empty() {
                stream_pages += 1;
                found = s;
            }
        }
        all_tables.extend(found);
    }

    let tables_text = render_tables(&all_tables);

    // The single most useful line when a document question comes back "no
    // relevant information": it separates "no text was ever extracted" (a
    // scanned/vector file pdfplumber cannot read) from "extracted, but retrieval
    // or the context window lost it".
    tracing::info!(
        path,
        pages,
        text_chars = text.len(),
        tables = all_tables.len(),
        stream_fallback_pages = stream_pages,
        tables_text_chars = tables_text.len(),
        "parse_pdf: extracted"
    );
    if text.trim().is_empty() {
        tracing::warn!(
            path,
            pages,
            "parse_pdf: no extractable text — likely a scanned or image-only PDF, which is out of scope"
        );
    }

    Ok(Parsed {
        text,
        tables: serde_json::to_value(&all_tables)?,
        tables_text,
        pages,
    })
}

/// Pull tables from one page under a given strategy, dropping fully-empty ones.
fn collect_tables(page: &pdfplumber::Page, settings: &TableSettings) -> Vec<Vec<Vec<String>>> {
    page.find_tables(settings)
        .into_iter()
        .map(|table| {
            table
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.text.as_deref().unwrap_or("").trim().to_string())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .filter(|rows: &Vec<Vec<String>>| rows.iter().any(|r| r.iter().any(|c| !c.is_empty())))
        .collect()
}

/// Flatten tables to text a small model can read: `col | col | col`, with the
/// first row (assumed the header) repeated above every data row so a chunk that
/// contains only the middle of a long table still names its columns.
fn render_tables(tables: &[Vec<Vec<String>>]) -> String {
    let mut out = String::new();
    for (t, rows) in tables.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        if t > 0 {
            out.push('\n');
        }
        out.push_str(&format!("[table {}]\n", t + 1));
        let header = rows[0].join(" | ");
        for (r, row) in rows.iter().enumerate() {
            let line = row.join(" | ");
            if line.trim().is_empty() {
                continue;
            }
            if r == 0 {
                out.push_str(&header);
                out.push('\n');
            } else {
                // header ▸ row, so a mid-table chunk keeps the column names
                out.push_str(&format!("{header}  \u{25b8}  {line}\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render_tables;

    #[test]
    fn render_tables_repeats_the_header_on_every_data_row() {
        let tables = vec![vec![
            vec!["Mode".into(), "MDR".into(), "GST".into()],
            vec!["Credit card".into(), "2.00%".into(), "18%".into()],
            vec!["UPI".into(), "0.00%".into(), "NA".into()],
        ]];
        let out = render_tables(&tables);
        // The figure a fee question needs is present…
        assert!(out.contains("2.00%"), "{out}");
        // …and the row that carries it still names its columns, so a chunk that
        // contains only the middle of a long table is still self-describing.
        let mdr_row = out
            .lines()
            .find(|l| l.contains("Credit card") && l.contains("2.00%"))
            .expect("credit-card row present");
        assert!(mdr_row.contains("Mode") && mdr_row.contains("MDR"), "{mdr_row}");
    }

    #[test]
    fn render_tables_is_empty_for_no_tables() {
        assert_eq!(render_tables(&[]), "");
        assert_eq!(render_tables(&[vec![]]), "");
    }
}
