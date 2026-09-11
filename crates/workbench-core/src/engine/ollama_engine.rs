//! The single resident LLM.
//!
//! The blueprint's memory rule: **one** text model stays loaded, and the three
//! "roles" are just different system prompts — never a second set of weights.
//! That is enforced here:
//!
//! * role A (intent), role B (planner) and role C (compiler) all call
//!   [`OllamaEngine::llm_model`] with `keep_alive = Indefinitely`;
//! * the specialist models (vision, embeddings) are called with `keep_alive = 0`
//!   so Ollama unloads them the moment they return and they never co-reside with
//!   the resident model.
//!
//! Because the executor is strictly sequential, no two of these calls overlap.

use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::generation::parameters::{FormatType, JsonStructure, KeepAlive};
use ollama_rs::models::ModelOptions;
use ollama_rs::Ollama;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::engine::schemas::{FinalReport, InputFile, IntentResult, Plan, ToolResult};
use crate::planner::registry::registry_prompt_block;
use std::time::Duration;

use crate::{CancelFlag, CoreError, Result, TurnMode};

/// Role A — turn the raw ask + attachment kinds into structured intent.
const ROLE_A_SYSTEM: &str = "\
You are the Intent Parser of an offline analysis assistant. The user works
with industrial *and* business documents — inspection reports, SOPs, invoices,
payment-services rate cards, contracts — treat every domain as in scope.
Read the user's request and the list of attached file kinds, then return STRICT JSON:
{\"intents\":[string,...],\"file_kinds\":[\"audio\"|\"pdf\"|\"image\"],\"needs_knowledge\":boolean}
- `intents`: one short phrase per distinct thing the user wants.
- `file_kinds`: echo only kinds that were actually attached.
- `needs_knowledge`: true only if answering needs reference material that would
  live in a knowledge base (plant SOPs, safety procedures, standards). A
  question answerable from the attached documents alone is `false`.
Return JSON only. No prose, no markdown, no code fences.";

/// Role C — synthesise everything the tools produced into the user-facing report.
const ROLE_C_SYSTEM: &str = "\
You are the Output Compiler of an offline analysis assistant. The request may
concern an inspection, an SOP, an invoice, a fee schedule or a contract — all
are in scope; do not treat a non-inspection question as out of scope.
You are given the user's request, the raw outputs of the tools that ran, a summary of
earlier turns, and long-term facility memory. Synthesise them into STRICT JSON:
{\"summary\":string,\"findings\":[string],\"citations\":[string],\"safety_notes\":[string],\"degraded\":boolean}
- `summary`: 2-4 sentences answering the user directly.
- `findings`: concrete observations, each traceable to a tool output.
- `citations`: names of knowledge-base sources you actually used. Empty if none.
- `safety_notes`: hazards or required precautions surfaced by the evidence. Empty if none.
- `degraded`: true if any tool failed or evidence was missing.
When the evidence contains a rate, fee, percentage or amount the user asked about,
quote it verbatim in `summary` and show the arithmetic for their figures.
Never invent measurements, tag numbers, rates or clauses that are not in the tool outputs.
Return JSON only. No prose, no markdown, no code fences.";

/// How much context, generation and CPU one session may use.
///
/// Ollama's per-model defaults are not safe here: a 4096-token window silently
/// truncates a long attachment (the model then reports "no relevant information"
/// about text that was simply cut off), and an uncapped thread count pins every
/// core, starving the desktop while a turn runs.
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    /// Context window. Must fit the evidence blob plus the answer.
    pub num_ctx: u64,
    /// Threads Ollama may use. Leave at least one core for the OS.
    pub num_thread: u32,
    /// Ceiling on generated tokens, so a rambling small model cannot burn
    /// minutes of CPU on one step.
    pub num_predict: i32,
    /// Wall-clock ceiling on a single model call. A call that blows past this is
    /// abandoned with [`CoreError::Timeout`] rather than hanging the turn (and,
    /// on a small box, the desktop) indefinitely. Deliberately generous: CPU
    /// inference of a long prompt is slow, not stuck.
    pub call_timeout: Duration,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            num_ctx: 8192,
            num_thread: cores.saturating_sub(1).max(1) as u32,
            num_predict: 640,
            call_timeout: Duration::from_secs(240),
        }
    }
}

/// Wraps the Ollama client and the three model names for one session.
pub struct OllamaEngine {
    client: Ollama,
    llm_model: String,
    vision_model: String,
    embed_model: String,
    limits: ResourceLimits,
    /// Set by the host for the duration of one turn; polled inside every model
    /// call so a user pressing Stop is honoured mid-call, not just between steps.
    cancel: Option<CancelFlag>,
    /// Fast (retrieve then answer) vs Deep (map-reduce the whole document).
    mode: TurnMode,
}

impl OllamaEngine {
    /// Build an engine against a running Ollama server.
    ///
    /// `base_url` is split into host + port because that is the shape `ollama-rs`
    /// wants; a malformed URL falls back to the default `127.0.0.1:11434`.
    pub fn new(
        base_url: &str,
        llm_model: impl Into<String>,
        vision_model: impl Into<String>,
        embed_model: impl Into<String>,
    ) -> Self {
        let (host, port) = split_base_url(base_url);
        let client = Ollama::builder().host(host).port(port).build();
        Self {
            client,
            llm_model: llm_model.into(),
            vision_model: vision_model.into(),
            embed_model: embed_model.into(),
            limits: ResourceLimits::default(),
            cancel: None,
            mode: TurnMode::Fast,
        }
    }

    /// Override the default context / thread / output ceilings.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    /// Attach a per-turn cancellation flag. See [`CancelFlag`].
    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// Set the read mode for this turn. See [`TurnMode`].
    pub fn with_mode(mut self, mode: TurnMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn mode(&self) -> TurnMode {
        self.mode
    }

    /// `Err(CoreError::Cancelled)` if the current turn was cancelled. Cheap;
    /// the executor calls it between steps so a stop during a blocking tool is
    /// caught before the next step starts.
    pub fn cancel_check(&self) -> Result<()> {
        match &self.cancel {
            Some(c) => c.check(),
            None => Ok(()),
        }
    }

    /// Drive one Ollama call to completion, but never past two limits: the
    /// wall-clock `call_timeout`, and a Stop from the user. Polls the cancel flag
    /// on a 200 ms tick so neither the timeout nor the flag can be missed while
    /// the request future is parked.
    async fn guarded<F, T>(&self, what: &str, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, ollama_rs::error::OllamaError>>,
    {
        tokio::pin!(fut);
        let deadline = tokio::time::sleep(self.limits.call_timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                biased;
                r = &mut fut => {
                    return r.map_err(|e| {
                        tracing::error!(what, error = %e, "ollama request failed");
                        CoreError::Ollama(e.to_string())
                    });
                }
                _ = &mut deadline => {
                    let secs = self.limits.call_timeout.as_secs();
                    tracing::error!(what, secs, "model call exceeded its time budget; abandoning it");
                    return Err(CoreError::Timeout { what: what.to_string(), secs });
                }
                _ = tokio::time::sleep(Duration::from_millis(200)), if self.cancel.is_some() => {
                    if self.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                        tracing::info!(what, "model call cancelled by user");
                        return Err(CoreError::Cancelled);
                    }
                }
            }
        }
    }

    /// The options every request carries: bounded context, bounded threads,
    /// bounded output.
    ///
    /// `pub(crate)` because the RAG embedder builds its own request type and must
    /// carry the same ceilings — an embedding call that skips these runs with
    /// Ollama's default *unbounded* thread count, which is enough on its own to
    /// starve the desktop for the length of a turn.
    pub(crate) fn opts(&self) -> ModelOptions {
        ModelOptions::default()
            .num_ctx(self.limits.num_ctx)
            .num_thread(self.limits.num_thread)
            .num_predict(self.limits.num_predict)
    }

    pub fn llm_model(&self) -> &str {
        &self.llm_model
    }
    pub fn vision_model(&self) -> &str {
        &self.vision_model
    }
    pub fn embed_model(&self) -> &str {
        &self.embed_model
    }
    /// Escape hatch for tools that talk to Ollama directly (vision, embeddings).
    pub fn client(&self) -> &Ollama {
        &self.client
    }

    /// Run an embeddings request under the same timeout + cancellation guard as
    /// every other model call. The RAG embedder builds its own request type, so
    /// it hands it here rather than reaching for [`Self::client`] directly — an
    /// unguarded embedding of a long document was one hot path that could still
    /// hang a turn.
    pub(crate) async fn embed_request(
        &self,
        request: ollama_rs::generation::embeddings::request::GenerateEmbeddingsRequest,
    ) -> Result<ollama_rs::generation::embeddings::GenerateEmbeddingsResponse> {
        self.guarded("embeddings", self.client.generate_embeddings(request))
            .await
    }

    /// Issue one schema-constrained generation against the resident model and
    /// deserialise it into `T`.
    ///
    /// Ollama is given `T`'s JSON Schema (structured outputs), so even a small
    /// model is forced to emit the right shape — arrays stay arrays, `depends_on`
    /// entries stay strings, etc. A body that still fails to deserialise is
    /// surfaced as [`CoreError::Schema`], the signal the planner's retry loop
    /// watches for.
    async fn generate_json<T: DeserializeOwned + JsonSchema>(
        &self,
        system: &str,
        user: String,
        role: &'static str,
    ) -> Result<T> {
        let started = std::time::Instant::now();
        let prompt_chars = user.len() + system.len();

        let request = GenerationRequest::new(self.llm_model.clone(), user)
            .system(system.to_string())
            .format(FormatType::StructuredJson(Box::new(JsonStructure::new::<T>())))
            .options(self.opts())
            .keep_alive(KeepAlive::Indefinitely);

        let response = self.guarded(role, self.client.generate(request)).await?;

        // Prompt size against the context window is the first thing to check when
        // a model claims it cannot find something that is in the document.
        tracing::debug!(
            role,
            model = %self.llm_model,
            prompt_chars,
            response_chars = response.response.len(),
            elapsed_ms = started.elapsed().as_millis(),
            ctx_budget_chars = self.limits.num_ctx * 4,
            "llm call (structured)"
        );
        if prompt_chars as u64 > self.limits.num_ctx * 4 {
            tracing::warn!(
                role,
                prompt_chars,
                ctx_budget_chars = self.limits.num_ctx * 4,
                "prompt likely exceeds the context window; the tail will be truncated"
            );
        }

        let body = strip_code_fence(response.response.trim());
        serde_json::from_str::<T>(body).map_err(|source| {
            tracing::warn!(role, body = %body.chars().take(400).collect::<String>(), "malformed JSON from model");
            CoreError::Schema { role, source }
        })
    }

    /// **Role A.** Extract intent and confirm which attachment kinds are in play.
    pub async fn parse_intent(
        &self,
        prompt: &str,
        history: &str,
        uploads: &[InputFile],
    ) -> Result<IntentResult> {
        let kinds: Vec<String> = uploads
            .iter()
            .map(|f| format!("{:?}", f.kind).to_lowercase())
            .collect();
        let history_block = if history.trim().is_empty() {
            String::new()
        } else {
            format!("EARLIER IN THIS CONVERSATION:\n{history}\n\n")
        };
        let user = format!(
            "{history_block}USER REQUEST:\n{prompt}\n\nATTACHED FILE KINDS: [{}]\n\n\
             Return the intent JSON.",
            kinds.join(", ")
        );
        self.generate_json(ROLE_A_SYSTEM, user, "intent_parser").await
    }

    /// **Role B.** Produce an ordered plan drawn only from the task registry.
    ///
    /// `previous_errors` is non-empty on a retry: the exact validator messages are
    /// fed back so the model repairs the specific problem rather than guessing.
    pub async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        let system = format!(
            "\
You are the Task Planner of an offline analysis assistant (industrial and
business documents alike).
Choose an ordered sequence of steps using ONLY these tasks:

{}

Rules:
- `task` MUST be exactly one of the quoted names above — copy it verbatim, with
  no stage label, brackets or other suffix (write `parse_pdf`, never `parse_pdf [Extract]`).
- Extraction and retrieval steps MUST come before analysis steps.
- List every step after the steps it depends on, using their `id`s in `depends_on`.
- ONE extraction step per attached file. If three images are attached, emit three
  `ocr_image` (or `analyze_image`) steps, one per image — never one step for all.
- Name the file each extraction step operates on in `args` as
  {{\"file\": \"<exact name from ATTACHED FILES>\"}}. Copy the name verbatim.
- Never invent a file. Only name files that appear in ATTACHED FILES.
- `search_knowledge` takes {{\"query\": \"...\"}} and no `file`. Analysis steps take
  no `args`.
- If the request is a FOLLOW-UP to EARLIER IN THIS CONVERSATION and needs no new
  files or knowledge (e.g. \"and what about debit cards?\", \"show that as a
  table\"), the whole plan is a single `answer_followup` step with no `args`.
- Give each step a short unique `id`.
Return STRICT JSON: {{\"steps\":[{{\"id\":string,\"task\":string,\"args\":object,\"depends_on\":[string]}}]}}
Return JSON only. No prose, no markdown, no code fences.",
            registry_prompt_block()
        );

        let file_list = if uploads.is_empty() {
            "(none)".to_string()
        } else {
            uploads
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    format!(
                        "{}. name=\"{}\"  kind={}",
                        i + 1,
                        f.original_name,
                        format!("{:?}", f.kind).to_lowercase()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let history_block = if history.trim().is_empty() {
            String::new()
        } else {
            format!("EARLIER IN THIS CONVERSATION:\n{history}\n\n")
        };
        let mut user = format!(
            "{history_block}USER REQUEST:\n{prompt}\n\nPARSED INTENT:\n{}\n\n\
             ATTACHED FILES:\n{file_list}\n",
            serde_json::to_string(intent)?,
        );
        if !previous_errors.is_empty() {
            user.push_str(&format!(
                "\nYOUR PREVIOUS PLAN WAS REJECTED BY THE VALIDATOR.\n\
                 Fix ALL of these problems and return a corrected plan:\n- {}\n",
                previous_errors.join("\n- ")
            ));
        }
        user.push_str("\nReturn the plan JSON.");

        let mut plan: Plan = self.generate_json(&system, user, "task_planner").await?;
        // Belt-and-braces: fold `"parse_pdf [Extract]"` etc. back to the bare
        // registry name so validation and the executor both resolve it.
        for step in &mut plan.steps {
            let clean = crate::planner::registry::normalize_task_name(&step.task).to_string();
            step.task = clean;
        }
        Ok(plan)
    }

    /// **Role C.** Fold tool outputs + memory into the final report.
    ///
    /// The tool outputs are rendered to readable text (not raw JSON — that dumped
    /// base64, timings and, because of field order, put a huge `text` field ahead
    /// of the `tables` a fee question needs) and, if still oversized, narrowed to
    /// the passages that bear on the request. Commit `5c0a3b6` did this for the
    /// analysis step but never here, so this path still overflowed the window.
    pub async fn compile_report(
        &self,
        prompt: &str,
        results: &[ToolResult],
        session_summary: &str,
        facility_memory: &str,
    ) -> Result<FinalReport> {
        /// Per-result cap so one verbose extraction cannot crowd out the rest.
        const PER_RESULT_CHARS: usize = 6_000;

        let rendered: String = results
            .iter()
            .map(|r| {
                let head = match r.data.get("source").and_then(|s| s.as_str()) {
                    Some(src) if !src.is_empty() => format!("## {} (from {src})", r.task),
                    _ => format!("## {}", r.task),
                };
                let mut body = if !r.ok {
                    format!("(this step failed: {})", r.error.as_deref().unwrap_or("unknown"))
                } else if let Some(t) = r.data.get("text").and_then(|t| t.as_str()) {
                    let mut b = t.to_string();
                    if let Some(tbl) = r.data.get("tables_text").and_then(|t| t.as_str()) {
                        if !tbl.trim().is_empty() {
                            b.push_str("\n\nTABLES:\n");
                            b.push_str(tbl);
                        }
                    }
                    b
                } else if let Some(o) = r.data.get("observation").and_then(|o| o.as_str()) {
                    o.to_string()
                } else if let Some(chunks) = r.data.get("chunks").and_then(|c| c.as_array()) {
                    chunks
                        .iter()
                        .filter_map(|c| {
                            let text = c.get("text")?.as_str()?;
                            let s = c.get("source").and_then(|s| s.as_str()).unwrap_or("kb");
                            Some(format!("[{s}] {text}"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    serde_json::to_string(&r.data).unwrap_or_default()
                };
                if body.chars().count() > PER_RESULT_CHARS {
                    body = body.chars().take(PER_RESULT_CHARS).collect::<String>() + " …[truncated]";
                }
                if let Some(w) = &r.warning {
                    body.push_str(&format!("\n(note: {w})"));
                }
                format!("{head}\n{body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        // Leave ~40% of the window for the system prompt, the question, the
        // memory blocks and the answer.
        let budget = ((self.limits.num_ctx as usize) * 4 * 3) / 5;
        let tool_outputs = if rendered.chars().count() > budget {
            crate::tools::rag::select_relevant(self, prompt, &rendered, 12, budget).await
        } else {
            rendered
        };

        let user = format!(
            "USER REQUEST:\n{prompt}\n\n\
             TOOL OUTPUTS:\n{tool_outputs}\n\n\
             EARLIER IN THIS SESSION:\n{}\n\n\
             FACILITY MEMORY:\n{}\n\n\
             Return the report JSON.",
            if session_summary.is_empty() { "(none)" } else { session_summary },
            if facility_memory.is_empty() { "(none)" } else { facility_memory },
        );
        self.generate_json(ROLE_C_SYSTEM, user, "output_compiler").await
    }

    /// Vision call against the compact VLM (e.g. `moondream`).
    ///
    /// `keep_alive = UnloadOnCompletion` so Ollama frees the vision model the
    /// instant it answers — it must never co-reside with the resident LLM.
    pub async fn analyze_image(&self, image_base64: &str, question: &str) -> Result<String> {
        use ollama_rs::generation::images::Image;
        let request = GenerationRequest::new(self.vision_model.clone(), question.to_string())
            .add_image(Image::from_base64(image_base64.to_string()))
            .options(self.opts())
            .keep_alive(KeepAlive::UnloadOnCompletion);

        let started = std::time::Instant::now();
        let response = self.guarded("vision", self.client.generate(request)).await?;

        tracing::debug!(
            model = %self.vision_model,
            image_b64_chars = image_base64.len(),
            response_chars = response.response.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "vlm call"
        );
        Ok(response.response.trim().to_string())
    }

    /// Analysis step (`summarize` / `compare_to_sop`) on the resident model:
    /// `instruction` + accumulated `evidence` + the user's ask → prose.
    pub async fn analyze(
        &self,
        instruction: &str,
        evidence: &str,
        user_prompt: &str,
    ) -> Result<String> {
        // A real system prompt: the fee/arithmetic instruction was previously a
        // tail sentence on the user message, competing with a wall of evidence
        // for a 1.5B model's attention.
        let system = "\
You analyse a supplied EVIDENCE block and answer strictly from it. The evidence
may be an inspection report, an SOP, an invoice, or a payment-services fee
schedule — every domain is in scope. Rules:
- Use only what is in EVIDENCE. If it does not contain the answer, say so plainly.
- When EVIDENCE gives a rate, fee, percentage or amount the request asks about,
  quote it verbatim and show the arithmetic for the user's numbers.
- A row in a TABLES section is evidence like any prose line.
- Be concise. No preamble.";

        let request = GenerationRequest::new(
            self.llm_model.clone(),
            format!("USER REQUEST:\n{user_prompt}\n\nTASK:\n{instruction}\n\nEVIDENCE:\n{evidence}"),
        )
        .system(system.to_string())
        .options(self.opts())
        .keep_alive(KeepAlive::Indefinitely);

        let started = std::time::Instant::now();
        let response = self.guarded("analysis", self.client.generate(request)).await?;

        tracing::debug!(
            model = %self.llm_model,
            evidence_chars = evidence.len(),
            response_chars = response.response.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "llm call (analysis)"
        );
        Ok(response.response.trim().to_string())
    }

    /// Load the resident model into RAM ahead of the first real turn.
    ///
    /// An empty prompt is enough to make Ollama read the weights off disk; the
    /// point is to pay that cost during bootstrap rather than inside the user's
    /// first question. Goes through the engine (rather than a bare client) so the
    /// very first model load is bounded by the same thread ceiling as every other
    /// call — this is the load most likely to thrash a small machine.
    pub async fn warm(&self) -> Result<()> {
        let request = GenerationRequest::new(self.llm_model.clone(), String::new())
            .options(self.opts())
            .keep_alive(KeepAlive::Indefinitely);

        let started = std::time::Instant::now();
        self.guarded("warm-up", self.client.generate(request)).await?;

        tracing::info!(
            model = %self.llm_model,
            elapsed_ms = started.elapsed().as_millis(),
            limits = ?self.limits,
            "resident model warmed"
        );
        Ok(())
    }

    /// Plain-text summarisation used by the rolling session-memory compressor.
    ///
    /// Deliberately not JSON-constrained: we want prose here, and a schema error
    /// would needlessly fail a memory write.
    pub async fn summarize(&self, text: &str) -> Result<String> {
        let request = GenerationRequest::new(
            self.llm_model.clone(),
            format!(
                "Summarise the following conversation history in at most 4 sentences. \
                 Preserve equipment tags, measurements and dates verbatim.\n\n{text}"
            ),
        )
        .options(self.opts())
        .keep_alive(KeepAlive::Indefinitely);

        let response = self
            .guarded("session-summary", self.client.generate(request))
            .await?;
        Ok(response.response.trim().to_string())
    }
}

/// Split `http://host:port` into the pieces `ollama-rs` expects.
/// Falls back to the Ollama default on anything unparseable.
fn split_base_url(base_url: &str) -> (String, u16) {
    let trimmed = base_url.trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", trimmed),
    };
    match rest.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (format!("{scheme}://{host}"), p),
            Err(_) => (format!("{scheme}://{rest}"), 11434),
        },
        None => (format!("{scheme}://{rest}"), 11434),
    }
}

/// Small models sometimes wrap JSON in ```json fences despite instructions.
/// Strip them rather than burning a retry on formatting.
fn strip_code_fence(body: &str) -> &str {
    let b = body.trim();
    if let Some(rest) = b.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        let rest = rest.trim_start_matches(['\n', '\r']);
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_base_url("http://127.0.0.1:11434"),
            ("http://127.0.0.1".to_string(), 11434)
        );
        assert_eq!(
            split_base_url("http://localhost"),
            ("http://localhost".to_string(), 11434)
        );
    }

    #[test]
    fn strips_fences() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
    }
}
