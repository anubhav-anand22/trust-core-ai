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
use ollama_rs::generation::parameters::{FormatType, KeepAlive};
use ollama_rs::Ollama;
use serde::de::DeserializeOwned;

use crate::engine::schemas::{FinalReport, InputFile, IntentResult, Plan, ToolResult};
use crate::planner::registry::registry_prompt_block;
use crate::{CoreError, Result};

/// Role A — turn the raw ask + attachment kinds into structured intent.
const ROLE_A_SYSTEM: &str = "\
You are the Intent Parser of an offline industrial inspection assistant.
Read the user's request and the list of attached file kinds, then return STRICT JSON:
{\"intents\":[string,...],\"file_kinds\":[\"audio\"|\"pdf\"|\"image\"],\"needs_knowledge\":boolean}
- `intents`: one short phrase per distinct thing the user wants.
- `file_kinds`: echo only kinds that were actually attached.
- `needs_knowledge`: true if answering requires plant SOPs, safety procedures or standards.
Return JSON only. No prose, no markdown, no code fences.";

/// Role C — synthesise everything the tools produced into the user-facing report.
const ROLE_C_SYSTEM: &str = "\
You are the Output Compiler of an offline industrial inspection assistant.
You are given the user's request, the raw outputs of the tools that ran, a summary of
earlier turns, and long-term facility memory. Synthesise them into STRICT JSON:
{\"summary\":string,\"findings\":[string],\"citations\":[string],\"safety_notes\":[string],\"degraded\":boolean}
- `summary`: 2-4 sentences answering the user directly.
- `findings`: concrete observations, each traceable to a tool output.
- `citations`: names of knowledge-base sources you actually used. Empty if none.
- `safety_notes`: hazards or required precautions surfaced by the SOPs.
- `degraded`: true if any tool failed or evidence was missing.
Never invent measurements, tag numbers or SOP clauses that are not in the tool outputs.
Return JSON only. No prose, no markdown, no code fences.";

/// Wraps the Ollama client and the three model names for one session.
pub struct OllamaEngine {
    client: Ollama,
    llm_model: String,
    vision_model: String,
    embed_model: String,
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
        }
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

    /// Issue one JSON-constrained generation against the resident model and
    /// deserialise it into `T`.
    ///
    /// A malformed body is surfaced as [`CoreError::Schema`] — the signal the
    /// planner's retry loop watches for.
    async fn generate_json<T: DeserializeOwned>(
        &self,
        system: &str,
        user: String,
        role: &'static str,
    ) -> Result<T> {
        let request = GenerationRequest::new(self.llm_model.clone(), user)
            .system(system.to_string())
            .format(FormatType::Json)
            .keep_alive(KeepAlive::Indefinitely);

        let response = self
            .client
            .generate(request)
            .await
            .map_err(|e| CoreError::Ollama(e.to_string()))?;

        let body = strip_code_fence(response.response.trim());
        serde_json::from_str::<T>(body).map_err(|source| CoreError::Schema { role, source })
    }

    /// **Role A.** Extract intent and confirm which attachment kinds are in play.
    pub async fn parse_intent(&self, prompt: &str, uploads: &[InputFile]) -> Result<IntentResult> {
        let kinds: Vec<String> = uploads
            .iter()
            .map(|f| format!("{:?}", f.kind).to_lowercase())
            .collect();
        let user = format!(
            "USER REQUEST:\n{prompt}\n\nATTACHED FILE KINDS: [{}]\n\nReturn the intent JSON.",
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
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        let system = format!(
            "\
You are the Task Planner of an offline industrial inspection assistant.
Choose an ordered sequence of steps using ONLY these tasks:

{}

Rules:
- Extraction and retrieval steps MUST come before analysis steps.
- List every step after the steps it depends on.
- Only schedule a task whose required file kind is actually attached.
- Give each step a short unique `id`.
Return STRICT JSON: {{\"steps\":[{{\"id\":string,\"task\":string,\"args\":object,\"depends_on\":[string]}}]}}
Return JSON only. No prose, no markdown, no code fences.",
            registry_prompt_block()
        );

        let mut user = format!(
            "USER REQUEST:\n{prompt}\n\nPARSED INTENT:\n{}\n\nATTACHED FILES:\n{}\n",
            serde_json::to_string(intent)?,
            serde_json::to_string(uploads)?,
        );
        if !previous_errors.is_empty() {
            user.push_str(&format!(
                "\nYOUR PREVIOUS PLAN WAS REJECTED BY THE VALIDATOR.\n\
                 Fix ALL of these problems and return a corrected plan:\n- {}\n",
                previous_errors.join("\n- ")
            ));
        }
        user.push_str("\nReturn the plan JSON.");

        self.generate_json(&system, user, "task_planner").await
    }

    /// **Role C.** Fold tool outputs + memory into the final report.
    pub async fn compile_report(
        &self,
        prompt: &str,
        results: &[ToolResult],
        session_summary: &str,
        facility_memory: &str,
    ) -> Result<FinalReport> {
        let user = format!(
            "USER REQUEST:\n{prompt}\n\n\
             TOOL OUTPUTS:\n{}\n\n\
             EARLIER IN THIS SESSION:\n{}\n\n\
             FACILITY MEMORY:\n{}\n\n\
             Return the report JSON.",
            serde_json::to_string_pretty(results)?,
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
            .keep_alive(KeepAlive::UnloadOnCompletion);
        let response = self
            .client
            .generate(request)
            .await
            .map_err(|e| CoreError::Ollama(e.to_string()))?;
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
        let request = GenerationRequest::new(
            self.llm_model.clone(),
            format!(
                "USER REQUEST:\n{user_prompt}\n\nTASK:\n{instruction}\n\nEVIDENCE:\n{evidence}\n\n\
                 Answer concisely and only from the evidence."
            ),
        )
        .keep_alive(KeepAlive::Indefinitely);
        let response = self
            .client
            .generate(request)
            .await
            .map_err(|e| CoreError::Ollama(e.to_string()))?;
        Ok(response.response.trim().to_string())
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
        .keep_alive(KeepAlive::Indefinitely);

        let response = self
            .client
            .generate(request)
            .await
            .map_err(|e| CoreError::Ollama(e.to_string()))?;
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
