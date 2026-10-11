//! The plan / validate / retry loop with a hard ceiling and a human off-ramp.
//!
//! Blueprint rule: **at most 2 automated re-prompts**, then stop and ask the user.
//! That is [`MAX_PLAN_ATTEMPTS`] = 3 (one initial attempt + two retries). There is
//! no unbounded "reflect and try again" loop anywhere in this pipeline.
//!
//! Planning talks to the model through the [`PlanSource`] trait rather than a
//! concrete client, so the retry ceiling and the HITL hand-off can be unit-tested
//! with a mock and no running Ollama.

use crate::engine::schemas::{FileKind, InputFile, IntentResult, Plan, TaskStep};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::planner::registry::validate_plan;
use crate::Result;

/// 1 initial planner call + 2 retries. Exceeding this parks the run for the user.
pub const MAX_PLAN_ATTEMPTS: u32 = 3;

/// The two model calls planning needs. Implemented by [`OllamaEngine`] for real
/// runs and by mocks in tests.
#[async_trait::async_trait]
pub trait PlanSource: Send + Sync {
    /// Role A. `history` is the session's `context_blob` — empty on the first turn.
    async fn parse_intent(
        &self,
        prompt: &str,
        history: &str,
        uploads: &[InputFile],
    ) -> Result<IntentResult>;

    /// Role B. `previous_errors` carries the validator's messages on a retry.
    async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan>;
}

#[async_trait::async_trait]
impl PlanSource for OllamaEngine {
    async fn parse_intent(
        &self,
        prompt: &str,
        history: &str,
        uploads: &[InputFile],
    ) -> Result<IntentResult> {
        OllamaEngine::parse_intent(self, prompt, history, uploads).await
    }

    async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        OllamaEngine::build_plan(self, prompt, history, intent, uploads, previous_errors).await
    }
}

/// What planning produced.
#[derive(Debug)]
pub enum PlanOutcome {
    /// Passed every deterministic check; safe to execute.
    Ready(Plan),
    /// Retries exhausted. `errors` and `last_plan` go to the HITL modal so the
    /// user can correct the plan or approve it as-is.
    AwaitingUser { errors: Vec<String>, last_plan: Plan },
}

/// Parse intent, then plan-and-validate until the plan is clean or attempts run
/// out.
///
/// **Inputs:** a plan source, the raw prompt, the files attached this turn, and a
/// progress sink. **Output:** the parsed intent plus a [`PlanOutcome`].
///
/// On each retry the *validator's own error strings* are fed back to
/// [`PlanSource::build_plan`], so the model repairs the specific defect instead of
/// re-rolling blindly.
pub async fn plan_turn(
    source: &dyn PlanSource,
    prompt: &str,
    uploads: &[InputFile],
    history: &str,
    has_history: bool,
    sink: &dyn ProgressSink,
) -> Result<(IntentResult, PlanOutcome)> {
    sink.emit(StepEvent::ParsingContext { attempt: 1 });
    let intent = source.parse_intent(prompt, history, uploads).await?;

    let mut last_plan = Plan::default();
    let mut errors: Vec<String> = Vec::new();

    for attempt in 1..=MAX_PLAN_ATTEMPTS {
        let plan = source
            .build_plan(prompt, history, &intent, uploads, &errors)
            .await?;

        sink.emit(StepEvent::ValidatingPlan { attempt });
        errors = validate_plan(&plan, uploads);

        if errors.is_empty() {
            tracing::info!(attempt, steps = plan.steps.len(), "plan validated");
            return Ok((intent, PlanOutcome::Ready(plan)));
        }

        tracing::warn!(attempt, ?errors, "plan rejected by validator");
        last_plan = plan;
    }

    // Retries exhausted. Rather than dead-ending in the HITL modal, fall back to
    // a deterministic plan built straight from the attachments — one extraction
    // step per file, then analysis. The stepper still showed the model planning;
    // it just can never leave the user with nothing. Only if *that* somehow fails
    // validation (it should not) do we actually park.
    let fb = fallback_plan(&intent, uploads, has_history);
    let fb_errors = validate_plan(&fb, uploads);
    if fb_errors.is_empty() {
        tracing::warn!(
            attempts = MAX_PLAN_ATTEMPTS,
            steps = fb.steps.len(),
            "planner exhausted retries; using a deterministic fallback plan"
        );
        crate::events::warn(
            sink,
            "The model could not produce a valid plan, so a standard \
             one-step-per-file plan was used.",
            Some(format!("planner errors: {}", errors.join("; "))),
        );
        return Ok((intent, PlanOutcome::Ready(fb)));
    }

    tracing::warn!(
        attempts = MAX_PLAN_ATTEMPTS,
        ?fb_errors,
        "planner exhausted retries and the fallback plan also failed; handing over to user"
    );
    sink.emit(StepEvent::AwaitingUser {
        errors: errors.clone(),
        plan_json: serde_json::to_string(&last_plan).unwrap_or_else(|_| "{}".into()),
    });

    Ok((intent, PlanOutcome::AwaitingUser { errors, last_plan }))
}

/// A plan built with no model call: extract every attached file, then analyse.
///
/// Deterministic and always valid for the given uploads — the safety net when the
/// resident model cannot write a usable plan (common on a 0.5–1.5B CPU model).
pub(crate) fn fallback_plan(
    intent: &IntentResult,
    uploads: &[InputFile],
    has_history: bool,
) -> Plan {
    let step = |id: &str, task: &str, args: serde_json::Value, deps: Vec<String>| TaskStep {
        id: id.to_string(),
        task: task.to_string(),
        args,
        depends_on: deps,
    };

    // No new files and nothing to retrieve, but there IS a conversation: this is
    // a follow-up question. Answer it from the transcript.
    if uploads.is_empty() && !intent.needs_knowledge && has_history {
        return Plan {
            steps: vec![step(
                "fu",
                "answer_followup",
                serde_json::Value::Object(Default::default()),
                vec![],
            )],
        };
    }

    let mut steps: Vec<TaskStep> = Vec::new();
    let mut extract_ids: Vec<String> = Vec::new();

    for (i, f) in uploads.iter().enumerate() {
        let task = match f.kind {
            FileKind::Pdf => "parse_pdf",
            FileKind::Audio => "transcribe_audio",
            FileKind::Image => "analyze_image",
            FileKind::Unknown => continue, // screened out upstream; belt and braces
        };
        let id = format!("x{}", i + 1);
        steps.push(step(
            &id,
            task,
            serde_json::json!({ "file": f.original_name }),
            vec![],
        ));
        extract_ids.push(id);
    }

    if intent.needs_knowledge {
        steps.push(step(
            "kb",
            "search_knowledge",
            serde_json::json!({ "query": intent.intents.join("; ") }),
            vec![],
        ));
    }

    // Analysis depends on everything above it.
    let mut analysis_deps = extract_ids.clone();
    if intent.needs_knowledge {
        analysis_deps.push("kb".to_string());
    }

    if !analysis_deps.is_empty() {
        steps.push(step(
            "sum",
            "summarize",
            serde_json::Value::Object(Default::default()),
            analysis_deps.clone(),
        ));
        if intent.needs_knowledge {
            steps.push(step(
                "sop",
                "compare_to_sop",
                serde_json::Value::Object(Default::default()),
                analysis_deps,
            ));
        }
    }

    // Nothing to extract, nothing to retrieve, no conversation to follow up on —
    // a greeting or a general question on a fresh chat. Without this the plan
    // came back empty, `validate_plan` rejected it ("plan contains no steps"),
    // and the one case this safety net exists for parked in the HITL modal.
    if steps.is_empty() {
        steps.push(step(
            "ans",
            "answer_directly",
            serde_json::Value::Object(Default::default()),
            vec![],
        ));
    }

    Plan { steps }
}

/// Analysis tasks that read the same evidence and can share one model call.
const FOLDABLE: [&str; 2] = ["summarize", "compare_to_sop"];

/// Replace a plan's `summarize` + `compare_to_sop` steps with one `assess` step.
///
/// Both steps rank the same evidence (an embedding-model reload each) and
/// prefill ~half the context window with it, so on a CPU the pair cost two
/// near-identical minutes. One call does both jobs. Runs on the *validated*
/// plan — `assess` is not in `TASK_REGISTRY`, so the planner prompt and the
/// validator never see it.
///
/// Deterministic, and a no-op unless the plan has at least one of *each*. The
/// merged step keeps the id of the last folded step, sits where that step sat,
/// and depends on the union of the folded steps' dependencies — every one of
/// which already appears before that position. A plan where some *other* step
/// depends on a folded one is left untouched: repointing it could break the
/// top-to-bottom order the executor relies on, and no real plan does it. So is
/// a plan with duplicate ids (possible only on a force-resume, which skips
/// validation). Skipping the fold just means two calls, as before.
pub fn fold_analysis(plan: Plan) -> Plan {
    let is_foldable = |s: &TaskStep| FOLDABLE.contains(&s.task.as_str());
    let has = |task: &str| plan.steps.iter().any(|s| s.task == task);
    if !FOLDABLE.iter().all(|t| has(t)) {
        return plan;
    }

    let folded_ids: Vec<&str> = plan
        .steps
        .iter()
        .filter(|s| is_foldable(s))
        .map(|s| s.id.as_str())
        .collect();
    let depended_on_by_others = plan.steps.iter().any(|s| {
        !is_foldable(s) && s.depends_on.iter().any(|d| folded_ids.contains(&d.as_str()))
    });
    // A force-resumed plan skips validation, so ids are not guaranteed unique
    // there. Folding by id on such a plan could keep two `assess` steps.
    let all_ids_unique = plan
        .steps
        .iter()
        .enumerate()
        .all(|(i, s)| plan.steps[..i].iter().all(|p| p.id != s.id));
    if depended_on_by_others || !all_ids_unique {
        tracing::info!(
            ?folded_ids,
            depended_on_by_others,
            all_ids_unique,
            "analysis fold skipped; running the steps separately"
        );
        return plan;
    }

    let mut depends_on: Vec<String> = Vec::new();
    for s in plan.steps.iter().filter(|s| is_foldable(s)) {
        for d in &s.depends_on {
            if !folded_ids.contains(&d.as_str()) && !depends_on.contains(d) {
                depends_on.push(d.clone());
            }
        }
    }
    let last_id = folded_ids.last().copied().unwrap_or_default().to_string();
    tracing::info!(?folded_ids, into = %last_id, "folded analysis steps into one assess call");

    let steps = plan
        .steps
        .into_iter()
        .filter_map(|s| {
            if !is_foldable(&s) {
                Some(s)
            } else if s.id == last_id {
                Some(TaskStep {
                    id: s.id,
                    task: "assess".to_string(),
                    args: serde_json::Value::Object(Default::default()),
                    depends_on: depends_on.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    Plan { steps }
}

/// Does some step of `plan` read attachment `f`?
///
/// Yes if a step names it in `args.file`, or if a step that consumes its kind
/// names no file and `f` is the first upload of that kind (what
/// `ToolContext::file_for` falls back to). Any tool of the kind counts: a photo
/// the planner sent to `ocr_image` is covered, not missing an `analyze_image`.
pub fn is_covered(plan: &Plan, uploads: &[InputFile], f: &InputFile) -> bool {
    use crate::planner::registry::lookup;

    let named = plan
        .steps
        .iter()
        .any(|s| s.args.get("file").and_then(|v| v.as_str()) == Some(f.original_name.as_str()));
    let is_first_of_kind =
        uploads.iter().find(|u| u.kind == f.kind).map(|u| &u.original_name) == Some(&f.original_name);
    let covered_by_fallback = is_first_of_kind
        && plan.steps.iter().any(|s| {
            s.args.get("file").is_none()
                && lookup(&s.task).and_then(|spec| spec.requires_file) == Some(f.kind)
        });
    named || covered_by_fallback
}

/// Should this turn search the SOP knowledge base?
///
/// Role A's `needs_knowledge`, OR the request plainly asks for it. Role A is a
/// small model too: on "check it against our SOPs" it said `false` in two of
/// three runs. Whole words only ("sop" must not fire on "isopropyl"), and no
/// bare "standard" — "what's the standard rate on this card?" is a fee
/// question, not a compliance one.
pub fn wants_knowledge(intent: &IntentResult, prompt: &str) -> bool {
    const WORDS: &[&str] = &[
        "sop", "sops", "procedure", "procedures", "compliance", "compliant", "regulation",
        "regulations", "standards",
    ];
    const PHRASES: &[&str] = &["check against", "checked against", "per standard", "against the standard"];

    let p = prompt.to_lowercase();
    intent.needs_knowledge
        || p.split(|c: char| !c.is_alphanumeric()).any(|w| WORDS.contains(&w))
        || PHRASES.iter().any(|ph| p.contains(ph))
}

/// Which tools can actually succeed on this machine. Completion never adds a
/// step that is certain to fail — that would only buy a "degraded" badge.
#[derive(Clone, Copy, Debug)]
pub struct Available {
    /// A vision model is configured (the profile can switch photos off).
    pub vision: bool,
    /// Both OCR model files are on disk.
    pub ocr: bool,
    /// The whisper model file is on disk.
    pub audio: bool,
}

/// Add what a validated plan left out of the request.
///
/// A 1.5B planner draws a different plan almost every run: across six
/// identical turns it skipped the photo in three, the SOP search in three and
/// any analysis in two (one plan was `parse_pdf` alone). The validator only
/// checks that a plan is *runnable*; this makes it *complete*:
/// 1. every attachment gets an extraction step (if its tool can run);
/// 2. `search_knowledge` when `wants_kb` and there is none;
/// 3. existing analysis steps also depend on what was added — `AnalysisTool`
///    reads only its declared dependencies, so otherwise it never sees it;
/// 4. evidence but no analysis → `summarize` (+ `compare_to_sop` with the KB),
///    which [`fold_analysis`] later turns into one `assess`.
///
/// Only for fresh plans: never a resumed one, whose shape the user chose in the
/// HITL modal. A conversational plan (`answer_followup` / `answer_directly`) is
/// left alone unless files were attached that it ignores; then the files are
/// covered and analysed alongside the answer step, but no KB search is added.
/// The result is re-validated; if that ever fails the original is returned.
///
/// With no vision model but OCR installed, `analyze_image` steps are rerouted
/// to `ocr_image` first: they could only fail otherwise.
///
/// Returns the plan and a short label per added or rerouted step, for the log.
pub fn complete_plan(
    mut plan: Plan,
    uploads: &[InputFile],
    prompt: &str,
    wants_kb: bool,
    available: Available,
) -> (Plan, Vec<String>) {
    use crate::planner::registry::{lookup, Stage};

    let conversational = plan
        .steps
        .iter()
        .any(|s| matches!(s.task.as_str(), "answer_followup" | "answer_directly"));

    let mut used: std::collections::HashSet<String> =
        plan.steps.iter().map(|s| s.id.clone()).collect();
    let mut fresh = |prefix: &str| {
        let id = (1..)
            .map(|n| format!("{prefix}{n}"))
            .find(|id| !used.contains(id))
            .expect("unbounded range");
        used.insert(id.clone());
        id
    };
    let step = |id: String, task: &str, args: serde_json::Value, deps: Vec<String>| TaskStep {
        id,
        task: task.to_string(),
        args,
        depends_on: deps,
    };

    let mut front: Vec<TaskStep> = Vec::new();
    let mut added: Vec<String> = Vec::new();

    // No vision model (a profile without photos) but OCR installed: an
    // `analyze_image` step can only fail ("image analysis is off"), and both the
    // model and `fallback_plan` emit it for every image. Read the image with OCR
    // instead. This is how an attached invoice came back "missing image analysis".
    if !available.vision && available.ocr {
        for s in plan.steps.iter_mut().filter(|s| s.task == "analyze_image") {
            s.task = "ocr_image".into();
            let file = s.args.get("file").and_then(|f| f.as_str()).unwrap_or("image");
            added.push(format!("analyze_image→ocr_image({file})"));
        }
    }

    for f in uploads.iter().filter(|f| !is_covered(&plan, uploads, f)) {
        let task = match f.kind {
            FileKind::Pdf => "parse_pdf",
            FileKind::Image if available.vision => "analyze_image",
            // No vision model (a profile without photos) but OCR is installed:
            // an image is still read. Without this, an attached invoice photo
            // went unread and the report said "no data".
            FileKind::Image if available.ocr => "ocr_image",
            FileKind::Audio if available.audio => "transcribe_audio",
            // Tool switched off, or unsupported: leave it uncovered and let
            // `warn_uncovered_files` say so.
            _ => continue,
        };
        front.push(step(fresh("c"), task, serde_json::json!({ "file": f.original_name }), vec![]));
        added.push(format!("{task}({})", f.original_name));
    }

    if wants_kb && !conversational && !plan.steps.iter().any(|s| s.task == "search_knowledge") {
        // The planner's own query would have come from the intents; the raw
        // request is the closest deterministic stand-in.
        front.push(step(
            fresh("kb"),
            "search_knowledge",
            serde_json::json!({ "query": prompt }),
            vec![],
        ));
        added.push("search_knowledge".into());
    }

    let is_analysis = |s: &TaskStep| lookup(&s.task).map(|t| t.stage) == Some(Stage::Analyze);
    let has_analysis = plan.steps.iter().any(is_analysis);
    // "Check it against our SOPs" with a plan that summarises but never
    // compares: the KB search alone doesn't run the check. (Found in review.)
    let kb_present = plan.steps.iter().chain(&front).any(|s| s.task == "search_knowledge");
    let needs_compare = wants_kb
        && !conversational
        && kb_present
        && has_analysis
        && !plan.steps.iter().any(|s| s.task == "compare_to_sop");
    if front.is_empty() && !needs_compare && (has_analysis || conversational) {
        return (plan, added);
    }

    let new_ids: Vec<String> = front.iter().map(|s| s.id.clone()).collect();
    let mut steps = front;
    for mut s in plan.steps.iter().cloned() {
        if is_analysis(&s) {
            s.depends_on.extend(new_ids.iter().cloned());
        }
        steps.push(s);
    }

    if !has_analysis {
        // Everything gathered so far, including a conversational answer step.
        let evidence: Vec<String> = steps
            .iter()
            .filter(|s| lookup(&s.task).is_some_and(|t| t.stage != Stage::Analyze))
            .map(|s| s.id.clone())
            .collect();
        if !evidence.is_empty() {
            let kb = steps.iter().any(|s| s.task == "search_knowledge");
            steps.push(step(fresh("sum"), "summarize", serde_json::json!({}), evidence.clone()));
            added.push("summarize".into());
            if kb {
                steps.push(step(fresh("sop"), "compare_to_sop", serde_json::json!({}), evidence));
                added.push("compare_to_sop".into());
            }
        }
    } else if needs_compare {
        let evidence: Vec<String> = steps
            .iter()
            .filter(|s| lookup(&s.task).is_some_and(|t| t.stage != Stage::Analyze))
            .map(|s| s.id.clone())
            .collect();
        steps.push(step(fresh("sop"), "compare_to_sop", serde_json::json!({}), evidence));
        added.push("compare_to_sop".into());
    }

    let completed = Plan { steps };
    let errors = validate_plan(&completed, uploads);
    if !errors.is_empty() {
        tracing::warn!(?errors, ?added, "plan completion produced an invalid plan; keeping the original");
        return (plan, Vec::new());
    }
    (completed, added)
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::engine::schemas::FileKind;
    use crate::planner::registry::validate_plan;

    fn file(name: &str, kind: FileKind) -> InputFile {
        InputFile {
            path: format!("/uploads/{name}"),
            kind,
            original_name: name.into(),
        }
    }

    fn intent(needs_knowledge: bool) -> IntentResult {
        IntentResult {
            intents: vec!["assess risk".into()],
            file_kinds: vec![],
            needs_knowledge,
        }
    }

    #[test]
    fn fallback_makes_one_step_per_file_and_names_it() {
        let uploads = vec![
            file("north.jpg", FileKind::Image),
            file("south.jpg", FileKind::Image),
            file("note.m4a", FileKind::Audio),
            file("sheet.pdf", FileKind::Pdf),
        ];
        let plan = fallback_plan(&intent(false), &uploads, false);

        let extract: Vec<_> = plan
            .steps
            .iter()
            .filter(|s| s.task != "summarize" && s.task != "compare_to_sop")
            .collect();
        assert_eq!(extract.len(), 4, "one extraction step per file");
        for s in &extract {
            let named = s.args.get("file").and_then(|v| v.as_str());
            assert!(
                named.map(|n| uploads.iter().any(|u| u.original_name == n)).unwrap_or(false),
                "step {} must name a real attachment, got {:?}",
                s.id,
                s.args
            );
        }
        // Two images → two analyze_image steps, not one.
        assert_eq!(
            plan.steps.iter().filter(|s| s.task == "analyze_image").count(),
            2
        );
    }

    #[test]
    fn fallback_plan_always_validates() {
        // validate_plan asserts every upload is really on disk, so use temp files.
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, kind: FileKind| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            InputFile {
                path: p.to_string_lossy().into_owned(),
                kind,
                original_name: name.into(),
            }
        };
        for nk in [false, true] {
            let uploads = vec![mk("a.pdf", FileKind::Pdf), mk("b.png", FileKind::Image)];
            let plan = fallback_plan(&intent(nk), &uploads, false);
            let errs = validate_plan(&plan, &uploads);
            assert!(errs.is_empty(), "needs_knowledge={nk}: {errs:?}");
        }
    }

    /// The hole in the safety net: a first message with no files that needs no
    /// knowledge base ("hi") used to fall back to an EMPTY plan, which
    /// `validate_plan` rejects — so the turn parked instead of answering.
    #[test]
    fn fallback_never_returns_an_empty_plan_for_a_bare_first_message() {
        let plan = fallback_plan(&intent(false), &[], false);
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.steps);
        assert_eq!(plan.steps[0].task, "answer_directly");
        let errs = validate_plan(&plan, &[]);
        assert!(errs.is_empty(), "fallback must always validate: {errs:?}");
    }

    #[test]
    fn fallback_adds_sop_comparison_only_when_knowledge_is_needed() {
        let uploads = vec![file("a.pdf", FileKind::Pdf)];
        assert!(!fallback_plan(&intent(false), &uploads, false)
            .steps
            .iter()
            .any(|s| s.task == "compare_to_sop"));
        assert!(fallback_plan(&intent(true), &uploads, false)
            .steps
            .iter()
            .any(|s| s.task == "compare_to_sop"));
    }
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    fn step(id: &str, task: &str, deps: &[&str]) -> TaskStep {
        TaskStep {
            id: id.into(),
            task: task.into(),
            args: serde_json::Value::Object(Default::default()),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn tasks(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.task.as_str()).collect()
    }

    /// The shape `fallback_plan` and most planners produce: one model call, not two.
    #[test]
    fn folds_the_pair_into_one_assess_with_the_union_of_dependencies() {
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", &[]),
                step("x2", "analyze_image", &[]),
                step("kb", "search_knowledge", &[]),
                step("sum", "summarize", &["x1", "x2"]),
                step("sop", "compare_to_sop", &["sum", "kb"]),
            ],
        };
        let folded = fold_analysis(plan);
        assert_eq!(tasks(&folded), ["parse_pdf", "analyze_image", "search_knowledge", "assess"]);
        let assess = folded.steps.last().unwrap();
        assert_eq!(assess.id, "sop", "keeps the last folded id");
        // `sum` was folded away, so it must not linger as a dependency.
        assert_eq!(assess.depends_on, ["x1", "x2", "kb"]);
    }

    #[test]
    fn folded_plan_still_lists_every_dependency_first() {
        let plan = fold_analysis(fallback_plan(
            &IntentResult {
                intents: vec!["check pump".into()],
                file_kinds: vec![],
                needs_knowledge: true,
            },
            &[InputFile {
                path: "/uploads/a.pdf".into(),
                kind: FileKind::Pdf,
                original_name: "a.pdf".into(),
            }],
            false,
        ));
        assert_eq!(tasks(&plan).iter().filter(|t| **t == "assess").count(), 1, "{plan:?}");
        for (i, s) in plan.steps.iter().enumerate() {
            for d in &s.depends_on {
                let at = plan.steps.iter().position(|p| &p.id == d);
                assert!(at.is_some_and(|at| at < i), "{} depends on {d}, not listed before it", s.id);
            }
        }
    }

    #[test]
    fn leaves_a_plan_without_both_halves_alone() {
        let plan = Plan {
            steps: vec![step("x1", "parse_pdf", &[]), step("sum", "summarize", &["x1"])],
        };
        assert_eq!(tasks(&fold_analysis(plan)), ["parse_pdf", "summarize"]);
    }

    #[test]
    fn leaves_a_force_resumed_plan_with_duplicate_ids_alone() {
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", &[]),
                step("a", "summarize", &["x1"]),
                step("a", "compare_to_sop", &["x1"]),
            ],
        };
        assert_eq!(tasks(&fold_analysis(plan)), ["parse_pdf", "summarize", "compare_to_sop"]);
    }

    #[test]
    fn leaves_the_plan_alone_when_another_step_depends_on_an_analysis_step() {
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", &[]),
                step("sum", "summarize", &["x1"]),
                step("kb", "search_knowledge", &["sum"]),
                step("sop", "compare_to_sop", &["kb"]),
            ],
        };
        assert_eq!(
            tasks(&fold_analysis(plan)),
            ["parse_pdf", "summarize", "search_knowledge", "compare_to_sop"]
        );
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    const ON: Available = Available { vision: true, ocr: true, audio: true };

    fn step(id: &str, task: &str, file: Option<&str>, deps: &[&str]) -> TaskStep {
        TaskStep {
            id: id.into(),
            task: task.into(),
            args: match file {
                Some(f) => serde_json::json!({ "file": f }),
                None => serde_json::json!({}),
            },
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn tasks(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.task.as_str()).collect()
    }

    /// Real files: completion re-validates, and the validator checks the disk.
    fn uploads(dir: &std::path::Path) -> Vec<InputFile> {
        [("inspection.pdf", FileKind::Pdf), ("valve.png", FileKind::Image)]
            .into_iter()
            .map(|(name, kind)| {
                let path = dir.join(name);
                std::fs::write(&path, b"x").unwrap();
                InputFile { path: path.to_string_lossy().into_owned(), kind, original_name: name.into() }
            })
            .collect()
    }

    /// The worst warm run: the planner read the PDF and stopped. Completion
    /// adds the photo, the SOP search and analysis, and the fold makes the
    /// analysis one `assess`.
    #[test]
    fn a_pdf_only_plan_gets_the_photo_the_sops_and_one_assess() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan { steps: vec![step("x1", "parse_pdf", Some("inspection.pdf"), &[])] };

        let (done, added) = complete_plan(plan, &up, "check it against our SOPs", true, ON);
        assert_eq!(added, ["analyze_image(valve.png)", "search_knowledge", "summarize", "compare_to_sop"]);
        assert!(validate_plan(&done, &up).is_empty());
        assert_eq!(
            tasks(&fold_analysis(done)),
            ["analyze_image", "search_knowledge", "parse_pdf", "assess"]
        );
    }

    /// `AnalysisTool` reads only its declared dependencies; an added step that
    /// isn't one of them would be extracted and then ignored.
    #[test]
    fn existing_analysis_also_depends_on_what_was_added() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("sum", "summarize", None, &["x1"]),
            ],
        };
        let (done, added) = complete_plan(plan, &up, "assess the pump", false, ON);
        assert_eq!(added, ["analyze_image(valve.png)"]);
        let sum = done.steps.iter().find(|s| s.id == "sum").unwrap();
        assert_eq!(sum.depends_on, ["x1", "c1"]);
    }

    #[test]
    fn a_photo_the_planner_sent_to_ocr_is_covered() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("x2", "ocr_image", Some("valve.png"), &[]),
                step("sum", "summarize", None, &["x1", "x2"]),
            ],
        };
        let (_, added) = complete_plan(plan, &up, "read the nameplate", false, ON);
        assert!(added.is_empty(), "{added:?}");
    }

    /// Profile says no photos: an `analyze_image` step could only fail.
    #[test]
    fn never_adds_a_step_whose_tool_is_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("sum", "summarize", None, &["x1"]),
            ],
        };
        let off = Available { vision: false, ocr: false, audio: true };
        let (done, added) = complete_plan(plan, &up, "assess", false, off);
        assert!(added.is_empty());
        assert!(!is_covered(&done, &up, &up[1]), "left for warn_uncovered_files to report");
    }

    /// The teammate's second report: the planner failed, the fallback plan sent
    /// the invoice to `analyze_image`, and with no vision model that step can
    /// only fail. OCR reads it instead.
    #[test]
    fn without_vision_the_fallback_plans_image_step_is_rerouted_to_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let intent = IntentResult { intents: vec!["describe this invoice".into()], file_kinds: vec![], needs_knowledge: false };
        let fb = fallback_plan(&intent, &up[1..], false);
        assert!(tasks(&fb).contains(&"analyze_image"));

        let ocr_only = Available { vision: false, ocr: true, audio: false };
        let (done, added) = complete_plan(fb, &up[1..], "describe this invoice", false, ocr_only);
        assert_eq!(added, ["analyze_image→ocr_image(valve.png)"]);
        assert!(tasks(&done).contains(&"ocr_image") && !tasks(&done).contains(&"analyze_image"));
        assert!(validate_plan(&done, &up[1..]).is_empty());
    }

    /// The teammate's bug: a profile without photos has no vision model, and
    /// an attached invoice image went unread. OCR reads it instead.
    #[test]
    fn without_vision_an_image_is_read_by_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan { steps: vec![step("kb", "search_knowledge", None, &[])] };
        let ocr_only = Available { vision: false, ocr: true, audio: false };
        let (done, added) = complete_plan(plan, &up, "what is the cost of labour for 10 hours", false, ocr_only);
        assert_eq!(added, ["parse_pdf(inspection.pdf)", "ocr_image(valve.png)", "summarize", "compare_to_sop"]);
        assert!(validate_plan(&done, &up).is_empty());
    }

    #[test]
    fn a_follow_up_without_files_is_left_alone_even_when_sops_are_mentioned() {
        let plan = Plan { steps: vec![step("fu", "answer_followup", None, &[])] };
        let (done, added) = complete_plan(plan, &[], "and per our SOPs?", true, ON);
        assert!(added.is_empty());
        assert_eq!(tasks(&done), ["answer_followup"]);
    }

    /// New files on a "follow-up" mean it isn't a pure follow-up. Read them and
    /// analyse them together with the conversational answer, but don't bolt a
    /// KB search onto a conversation.
    #[test]
    fn a_follow_up_that_ignores_attached_files_gets_them_read() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan { steps: vec![step("fu", "answer_followup", None, &[])] };
        let (done, added) = complete_plan(plan, &up, "and against our SOPs?", true, ON);
        assert_eq!(added, ["parse_pdf(inspection.pdf)", "analyze_image(valve.png)", "summarize"]);
        let sum = done.steps.last().unwrap();
        assert_eq!(sum.depends_on, ["c1", "c2", "fu"]);
        assert!(validate_plan(&done, &up).is_empty());
    }

    /// The likeliest under-plan for "check it against our SOPs": everything
    /// read and summarised, the SOP check itself missing.
    #[test]
    fn a_summary_without_the_sop_check_gets_it_and_folds_to_assess() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("x2", "analyze_image", Some("valve.png"), &[]),
                step("sum", "summarize", None, &["x1", "x2"]),
            ],
        };
        let (done, added) = complete_plan(plan, &up, "check it against our SOPs", true, ON);
        assert_eq!(added, ["search_knowledge", "compare_to_sop"]);
        assert!(validate_plan(&done, &up).is_empty());
        assert_eq!(
            tasks(&fold_analysis(done)),
            ["search_knowledge", "parse_pdf", "analyze_image", "assess"]
        );
    }

    /// Nothing to add at the front, so the early return must not swallow it.
    #[test]
    fn a_kb_search_with_no_comparison_gets_the_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("x1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("x2", "analyze_image", Some("valve.png"), &[]),
                step("kb", "search_knowledge", None, &[]),
                step("sum", "summarize", None, &["x1", "x2", "kb"]),
            ],
        };
        let (done, added) = complete_plan(plan, &up, "assess against our SOPs", true, ON);
        assert_eq!(added, ["compare_to_sop"]);
        assert_eq!(done.steps.last().unwrap().depends_on, ["x1", "x2", "kb"]);
    }

    #[test]
    fn added_ids_never_collide_with_the_planners() {
        let dir = tempfile::tempdir().unwrap();
        let up = uploads(dir.path());
        let plan = Plan {
            steps: vec![
                step("c1", "parse_pdf", Some("inspection.pdf"), &[]),
                step("sum1", "summarize", None, &["c1"]),
            ],
        };
        let (done, _) = complete_plan(plan, &up, "assess", false, ON);
        let ids: Vec<&str> = done.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["c2", "c1", "sum1"]);
    }

    #[test]
    fn knowledge_is_wanted_on_whole_words_only() {
        let no = IntentResult { intents: vec![], file_kinds: vec![], needs_knowledge: false };
        assert!(wants_knowledge(&no, "Assess P-101 and check it against our SOPs."));
        assert!(wants_knowledge(&no, "is this compliant with procedure?"));
        assert!(!wants_knowledge(&no, "isopropyl alcohol leak near the pump"));
        assert!(!wants_knowledge(&no, "what's the standard rate on this card?"));
        let yes = IntentResult { needs_knowledge: true, ..no };
        assert!(wants_knowledge(&yes, "anything"));
    }
}
