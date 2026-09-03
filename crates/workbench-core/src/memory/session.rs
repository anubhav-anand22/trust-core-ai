//! Short-term session memory (`session_context.json`).
//!
//! Blueprint: "appends turn summaries generated via an automated compression hook
//! after every interaction" and keeps "token usage within fixed limits". This file
//! is *this session's own scratch* — not secret — so it is stored as plain JSON,
//! written atomically (temp + rename) so a crash mid-write can't corrupt it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::OllamaEngine;
use crate::Result;

/// Once more than this many turns are stored verbatim, the oldest are folded into
/// `rolling_summary` by the compressor.
const MAX_VERBATIM_TURNS: usize = 6;

/// One completed turn, already reduced to short strings by the caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnSummary {
    pub user_prompt: String,
    /// e.g. "parse_pdf → ocr_image → analyze_image → search_knowledge → summarize".
    pub plan_summary: String,
    /// One line on what the tools yielded.
    pub tool_summary: String,
    /// The report's own summary line.
    pub report_summary: String,
}

/// The rolling context for the active session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: String,
    #[serde(default)]
    pub turns: Vec<TurnSummary>,
    /// Compressed digest of turns that have aged out of `turns`.
    #[serde(default)]
    pub rolling_summary: String,
}

impl SessionContext {
    /// Load the context for `session_id`, or start a fresh one. A file belonging to
    /// a *different* session id is ignored (a new session starts clean).
    pub fn load_or_new(path: &Path, session_id: &str) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<SessionContext>(&s).ok())
            .filter(|c| c.session_id == session_id)
            .unwrap_or_else(|| SessionContext {
                session_id: session_id.to_string(),
                ..Default::default()
            })
    }

    /// The block handed to role C as "earlier in this session".
    pub fn context_blob(&self) -> String {
        let mut out = String::new();
        if !self.rolling_summary.is_empty() {
            out.push_str("SUMMARY OF EARLIER TURNS:\n");
            out.push_str(&self.rolling_summary);
            out.push_str("\n\n");
        }
        for (i, t) in self.turns.iter().enumerate() {
            out.push_str(&format!(
                "TURN {}:\n  asked: {}\n  did: {}\n  found: {}\n",
                i + 1,
                t.user_prompt,
                t.plan_summary,
                t.report_summary
            ));
        }
        out
    }

    /// Append a completed turn, compress if over budget, then persist.
    ///
    /// The compressor calls [`OllamaEngine::summarize`], i.e. the *resident* model
    /// — no second model is ever loaded for memory maintenance.
    pub async fn append_turn(
        &mut self,
        turn: TurnSummary,
        engine: &OllamaEngine,
        path: &Path,
    ) -> Result<()> {
        self.turns.push(turn);
        if self.turns.len() > MAX_VERBATIM_TURNS {
            self.compress(engine).await;
        }
        self.save(path)
    }

    /// Fold everything beyond the verbatim budget into `rolling_summary`. If the
    /// summariser call fails we keep the un-summarised text rather than lose it.
    async fn compress(&mut self, engine: &OllamaEngine) {
        let overflow = self.turns.len().saturating_sub(MAX_VERBATIM_TURNS);
        if overflow == 0 {
            return;
        }
        let aged: Vec<TurnSummary> = self.turns.drain(0..overflow).collect();

        let mut blob = self.rolling_summary.clone();
        for t in &aged {
            blob.push_str(&format!(
                "\n- asked: {} | found: {}",
                t.user_prompt, t.report_summary
            ));
        }
        self.rolling_summary = match engine.summarize(&blob).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "session compression failed; keeping raw digest");
                blob
            }
        };
    }

    /// Atomic write: serialise to `<path>.tmp`, then rename over `<path>`.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
