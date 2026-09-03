//! Task registry and **deterministic** plan validation.
//!
//! No LLM is consulted here. The blueprint is explicit that plan feasibility must
//! be decided by "pure Python conditional checks, not an LLM self-evaluation loop"
//! — this is the Rust equivalent. Everything below is ordinary branching over the
//! plan, the registry table, and the filesystem.
//!
//! Adding a tool means: one row in [`TASK_REGISTRY`] + one match arm in the
//! executor. Nothing else needs to change.

use std::collections::{HashMap, HashSet};

use crate::engine::schemas::{FileKind, InputFile, Plan};

/// Coarse ordering bucket. Extraction and retrieval produce facts; analysis
/// consumes them. A plan that analyses before it extracts is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Extract,
    Retrieve,
    Analyze,
}

/// Static description of a task the executor knows how to run.
#[derive(Clone, Copy, Debug)]
pub struct TaskSpec {
    pub name: &'static str,
    pub stage: Stage,
    /// File kind this task consumes, if any. Validation asserts a matching file
    /// was actually attached.
    pub requires_file: Option<FileKind>,
    /// One-line description, injected into role B's prompt so the planner knows
    /// what each task is for without a second source of truth.
    pub description: &'static str,
}

/// The complete set of tasks role B may emit.
pub const TASK_REGISTRY: &[TaskSpec] = &[
    TaskSpec {
        name: "transcribe_audio",
        stage: Stage::Extract,
        requires_file: Some(FileKind::Audio),
        description: "Transcribe an attached audio file (field note, voice log) to text.",
    },
    TaskSpec {
        name: "parse_pdf",
        stage: Stage::Extract,
        requires_file: Some(FileKind::Pdf),
        description: "Extract text and tables from an attached digital PDF (inspection sheet, spec).",
    },
    TaskSpec {
        name: "ocr_image",
        stage: Stage::Extract,
        requires_file: Some(FileKind::Image),
        description: "Read printed/stamped text off an attached image (nameplate, form, label, gauge).",
    },
    TaskSpec {
        name: "analyze_image",
        stage: Stage::Extract,
        requires_file: Some(FileKind::Image),
        description: "Visually inspect an attached photo for physical condition (corrosion, leaks, damage).",
    },
    TaskSpec {
        name: "search_knowledge",
        stage: Stage::Retrieve,
        requires_file: None,
        description: "Search the standing SOP / safety-manual knowledge base for relevant passages.",
    },
    TaskSpec {
        name: "summarize",
        stage: Stage::Analyze,
        requires_file: None,
        description: "Summarise the collected evidence into findings.",
    },
    TaskSpec {
        name: "compare_to_sop",
        stage: Stage::Analyze,
        requires_file: None,
        description: "Compare observed conditions against retrieved SOP requirements and flag deviations.",
    },
];

/// Look a task up by name. `None` means the planner hallucinated it.
pub fn lookup(task: &str) -> Option<&'static TaskSpec> {
    TASK_REGISTRY.iter().find(|t| t.name == task)
}

/// Registry rendered for role B's system prompt, so the planner and the validator
/// can never drift apart.
pub fn registry_prompt_block() -> String {
    TASK_REGISTRY
        .iter()
        .map(|t| {
            let needs = match t.requires_file {
                Some(k) => format!(" (requires an attached {k:?} file)"),
                None => String::new(),
            };
            format!("- {} [{:?}]{}: {}", t.name, t.stage, needs, t.description)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Validate a plan against the registry, the DAG rules and the filesystem.
///
/// **Input:** the plan role B produced, plus the files actually attached to this
/// turn. **Output:** a list of human-readable problems; an empty list means the
/// plan is safe to execute.
///
/// The returned strings are fed back verbatim into the planner's retry prompt and,
/// if retries run out, shown to the user in the HITL modal — so they are written
/// to be actionable, not just diagnostic.
pub fn validate_plan(plan: &Plan, uploads: &[InputFile]) -> Vec<String> {
    let mut errors = Vec::new();

    if plan.steps.is_empty() {
        errors.push("plan contains no steps".into());
        return errors;
    }

    // 1. Every task must exist in the registry.
    for step in &plan.steps {
        if lookup(&step.task).is_none() {
            errors.push(format!(
                "step `{}`: unknown task `{}` (not in the task registry)",
                step.id, step.task
            ));
        }
    }

    // 2. Step ids must be unique, and every `depends_on` must resolve.
    let ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    if ids.len() != plan.steps.len() {
        errors.push("plan contains duplicate step ids".into());
    }
    for step in &plan.steps {
        for dep in &step.depends_on {
            if !ids.contains(dep.as_str()) {
                errors.push(format!(
                    "step `{}`: depends_on references unknown step `{}`",
                    step.id, dep
                ));
            }
        }
    }

    // 3. The list must already be in topological order: the executor runs it
    //    strictly top-to-bottom, so a dependency listed later can never be ready.
    //    (This also rules out cycles.)
    let position: HashMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    for (i, step) in plan.steps.iter().enumerate() {
        for dep in &step.depends_on {
            if let Some(&dep_pos) = position.get(dep.as_str()) {
                if dep_pos >= i {
                    errors.push(format!(
                        "step `{}` is listed before its dependency `{}`; reorder so dependencies come first",
                        step.id, dep
                    ));
                }
            }
        }
    }

    // 4. Stage ordering — the blueprint's "parsing/extraction tasks must precede
    //    analytical tasks".
    let stage_of = |task: &str| lookup(task).map(|t| t.stage);
    let has_files = !uploads.is_empty();
    let has_evidence_step = plan.steps.iter().any(|s| {
        matches!(
            stage_of(&s.task),
            Some(Stage::Extract) | Some(Stage::Retrieve)
        )
    });

    if has_files && !has_evidence_step {
        errors.push(
            "files are attached but the plan has no extraction or retrieval step to read them"
                .into(),
        );
    }

    for (i, step) in plan.steps.iter().enumerate() {
        if stage_of(&step.task) == Some(Stage::Analyze) {
            let preceded_by_evidence = plan.steps[..i].iter().any(|s| {
                matches!(
                    stage_of(&s.task),
                    Some(Stage::Extract) | Some(Stage::Retrieve)
                )
            });
            if !preceded_by_evidence {
                errors.push(format!(
                    "analysis step `{}` (`{}`) has no preceding extraction/retrieval step to analyse",
                    step.id, step.task
                ));
            }
        }
    }

    // 5. Tasks that consume a file kind need one attached.
    let kinds_present: HashSet<FileKind> = uploads.iter().map(|f| f.kind).collect();
    for step in &plan.steps {
        if let Some(spec) = lookup(&step.task) {
            if let Some(required) = spec.requires_file {
                if !kinds_present.contains(&required) {
                    errors.push(format!(
                        "step `{}` (`{}`) needs an attached {:?} file but none was provided",
                        step.id, step.task, required
                    ));
                }
            }
        }
    }

    // 6. Every attached file must really be on disk before we invoke anything.
    for file in uploads {
        if !std::path::Path::new(&file.path).is_file() {
            errors.push(format!(
                "attached file `{}` was not found at {}",
                file.original_name, file.path
            ));
        }
        if file.kind == FileKind::Unknown {
            errors.push(format!(
                "attached file `{}` has an unsupported type",
                file.original_name
            ));
        }
    }

    errors
}
