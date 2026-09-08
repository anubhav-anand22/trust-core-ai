//! Rule-based output sanity assertions.
//!
//! The blueprint calls for a quality check that is *rule-based*, explicitly not an
//! LLM grading itself. Every assertion below is a plain predicate over the plan and
//! the tool results, so its verdict is reproducible and explainable in the audit
//! sidebar.

use crate::engine::schemas::{FinalReport, Plan, QualityCheck, QualityReport, ToolResult};

/// Check the collected tool output against the plan that produced it.
///
/// **Inputs:** the executed plan, its results, and the compiled report (if one was
/// produced yet). **Output:** a [`QualityReport`] whose `passed` is the AND of all
/// checks. Failures are informational — they mark the run degraded rather than
/// discarding it, because a partial answer still beats nothing in the field.
pub fn assert_sane(
    plan: &Plan,
    results: &[ToolResult],
    report: Option<&FinalReport>,
) -> QualityReport {
    let mut checks = Vec::new();

    // 1. Every planned step produced a result.
    let missing: Vec<&str> = plan
        .steps
        .iter()
        .filter(|s| !results.iter().any(|r| r.step_id == s.id))
        .map(|s| s.id.as_str())
        .collect();
    checks.push(QualityCheck {
        name: "all_steps_reported".into(),
        passed: missing.is_empty(),
        detail: if missing.is_empty() {
            format!("{} step(s) all reported", plan.steps.len())
        } else {
            format!("no result for step(s): {}", missing.join(", "))
        },
    });

    // 2. No tool failed.
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| r.task.as_str())
        .collect();
    checks.push(QualityCheck {
        name: "no_tool_failures".into(),
        passed: failed.is_empty(),
        detail: if failed.is_empty() {
            "all tools succeeded".into()
        } else {
            format!("failed tool(s): {}", failed.join(", "))
        },
    });

    // 3. If knowledge retrieval was planned, it must have returned something —
    //    an empty KB search means citations in the report would be fabricated.
    if plan.steps.iter().any(|s| s.task == "search_knowledge") {
        let hit = results.iter().any(|r| {
            r.task == "search_knowledge"
                && r.ok
                && r.data
                    .get("chunks")
                    .and_then(|c| c.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        });
        checks.push(QualityCheck {
            name: "knowledge_search_returned_results".into(),
            passed: hit,
            detail: if hit {
                "knowledge base returned at least one passage".into()
            } else {
                "knowledge search returned no passages; citations may be unsupported".into()
            },
        });
    }

    // 4. Extraction steps should not come back empty-handed.
    for r in results.iter().filter(|r| r.ok) {
        if matches!(r.task.as_str(), "parse_pdf" | "ocr_image" | "transcribe_audio") {
            let text_len = r
                .data
                .get("text")
                .and_then(|t| t.as_str())
                .map(str::len)
                .unwrap_or(0);
            if text_len == 0 {
                checks.push(QualityCheck {
                    name: format!("{}_produced_text", r.task),
                    passed: false,
                    detail: r
                        .warning
                        .clone()
                        .unwrap_or_else(|| format!("`{}` extracted no text", r.task)),
                });
            }
        }
    }

    // 4b. If a PDF yielded tables, their flat rendering must have come through.
    //     A regression in `render_tables` would otherwise silently drop every
    //     figure in a rate card — exactly the bug this whole stage was about.
    for r in results.iter().filter(|r| r.ok && r.task == "parse_pdf") {
        let table_count = r
            .data
            .get("tables")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if table_count > 0 {
            let rendered = r
                .data
                .get("tables_text")
                .and_then(|t| t.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            checks.push(QualityCheck {
                name: "pdf_tables_rendered".into(),
                passed: rendered,
                detail: if rendered {
                    format!("{table_count} table(s) extracted and rendered to text")
                } else {
                    format!("{table_count} table(s) extracted but not rendered — the model will not see them")
                },
            });
        }
    }

    // 5. The report must actually say something.
    if let Some(rep) = report {
        checks.push(QualityCheck {
            name: "report_non_empty".into(),
            passed: !rep.summary.trim().is_empty(),
            detail: if rep.summary.trim().is_empty() {
                "compiler returned an empty summary".into()
            } else {
                format!("summary is {} chars", rep.summary.trim().len())
            },
        });
    }

    let passed = checks.iter().all(|c| c.passed);
    QualityReport { passed, checks }
}
