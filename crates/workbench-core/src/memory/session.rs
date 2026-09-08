//! Per-session chat memory — one JSON file per session under `sessions/`.
//!
//! Two things live here, with different lifetimes:
//!
//! * **`exchanges`** — the full transcript (prompt + report + attachment names +
//!   timestamp) for every turn. This is what the UI redraws when you click a past
//!   chat in the sidebar, so nothing is dropped from it.
//! * **`turns` + `rolling_summary`** — the *bounded* context handed to the model.
//!   Once there are more than [`MAX_VERBATIM_TURNS`] recent turns the oldest are
//!   compressed into `rolling_summary`, so the prompt never grows without limit.
//!
//! The file is this session's own scratch — not secret — so it is plain JSON,
//! written atomically (temp + rename) so a crash mid-write cannot corrupt it.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::schemas::FinalReport;
use crate::engine::OllamaEngine;
use crate::Result;

/// Once more than this many turns are stored verbatim, the oldest are folded into
/// `rolling_summary` by the compressor. This bounds *model context*, not the
/// transcript — `exchanges` keeps every turn.
const MAX_VERBATIM_TURNS: usize = 6;

/// Seconds since the Unix epoch, or 0 if the clock is unreadable.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One completed turn, reduced to short strings — the model-context view.
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

/// One turn as the user should see it again — the full transcript view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exchange {
    pub prompt: String,
    /// `original_name` of each attachment on this turn.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// The report produced. `None` if the turn errored or was cancelled before
    /// synthesis.
    #[serde(default)]
    pub report: Option<FinalReport>,
    /// Unix seconds when the turn finished.
    pub ts: i64,
}

/// A session's file on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: String,
    /// Shown in the sidebar. Defaults to the first prompt, truncated; the user
    /// can rename it.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// Full transcript, oldest first.
    #[serde(default)]
    pub exchanges: Vec<Exchange>,
    /// Recent turns kept verbatim for the model.
    #[serde(default)]
    pub turns: Vec<TurnSummary>,
    /// Compressed digest of turns that have aged out of `turns`.
    #[serde(default)]
    pub rolling_summary: String,
}

impl Default for SessionContext {
    fn default() -> Self {
        let now = now_secs();
        Self {
            session_id: String::new(),
            title: String::new(),
            created_at: now,
            updated_at: now,
            exchanges: Vec::new(),
            turns: Vec::new(),
            rolling_summary: String::new(),
        }
    }
}

/// A one-line summary of a session, for the sidebar list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub title: String,
    pub updated_at: i64,
    pub exchange_count: usize,
}

impl SessionContext {
    /// Load the session stored at `path`, or a fresh one bound to `session_id`.
    ///
    /// Unlike the old single-file scheme, the *path* identifies the session, so
    /// there is no id filter — whatever is in the file is this session.
    pub fn load_or_new(path: &Path, session_id: &str) -> Self {
        Self::load(path).unwrap_or_else(|| SessionContext {
            session_id: session_id.to_string(),
            ..Default::default()
        })
    }

    /// Read a session file, or `None` if it is missing / unparseable.
    pub fn load(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<SessionContext>(&raw).ok()
    }

    /// Every session in `dir`, newest-updated first. Corrupt files are skipped.
    pub fn list(dir: &Path) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(s) = Self::load(&p) {
                out.push(SessionMeta {
                    session_id: s.session_id,
                    title: if s.title.is_empty() {
                        "(untitled)".to_string()
                    } else {
                        s.title
                    },
                    updated_at: s.updated_at,
                    exchange_count: s.exchanges.len(),
                });
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// The block handed to role A/B/C as "earlier in this session".
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

    /// True once at least one turn has been recorded — a follow-up question can
    /// be answered from conversation alone.
    pub fn has_history(&self) -> bool {
        !self.turns.is_empty() || !self.rolling_summary.is_empty()
    }

    /// Record a completed turn: append the transcript entry *and* the bounded
    /// model-context summary, compress if over budget, refresh the title/clock,
    /// then persist.
    pub async fn record_turn(
        &mut self,
        exchange: Exchange,
        turn: TurnSummary,
        engine: &OllamaEngine,
        path: &Path,
    ) -> Result<()> {
        if self.title.is_empty() {
            self.title = title_from_prompt(&exchange.prompt);
        }
        self.updated_at = exchange.ts.max(now_secs());
        self.exchanges.push(exchange);

        self.turns.push(turn);
        if self.turns.len() > MAX_VERBATIM_TURNS {
            self.compress(engine).await;
        }
        self.save(path)
    }

    /// Back-compat shim for callers that only have a `TurnSummary`.
    pub async fn append_turn(
        &mut self,
        turn: TurnSummary,
        engine: &OllamaEngine,
        path: &Path,
    ) -> Result<()> {
        let exchange = Exchange {
            prompt: turn.user_prompt.clone(),
            attachments: Vec::new(),
            report: None,
            ts: now_secs(),
        };
        self.record_turn(exchange, turn, engine, path).await
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
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// First line of the prompt, trimmed to a sidebar-friendly length.
fn title_from_prompt(prompt: &str) -> String {
    let first = prompt.trim().lines().next().unwrap_or("").trim();
    let mut t: String = first.chars().take(60).collect();
    if first.chars().count() > 60 {
        t.push('…');
    }
    if t.is_empty() {
        "(untitled)".to_string()
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> OllamaEngine {
        OllamaEngine::new("http://127.0.0.1:11434", "a", "b", "c")
    }

    #[tokio::test]
    async fn per_session_files_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("s-aaa.json");
        let p2 = dir.path().join("s-bbb.json");

        let mut a = SessionContext::load_or_new(&p1, "s-aaa");
        a.record_turn(
            Exchange { prompt: "first chat".into(), attachments: vec![], report: None, ts: 100 },
            TurnSummary {
                user_prompt: "first chat".into(),
                plan_summary: "summarize".into(),
                tool_summary: "0 tools".into(),
                report_summary: "done".into(),
            },
            &engine(),
            &p1,
        )
        .await
        .unwrap();

        // A different session must not have touched the first file.
        let _b = SessionContext::load_or_new(&p2, "s-bbb");
        let reloaded = SessionContext::load(&p1).unwrap();
        assert_eq!(reloaded.session_id, "s-aaa");
        assert_eq!(reloaded.exchanges.len(), 1);
        assert_eq!(reloaded.title, "first chat");
    }

    #[tokio::test]
    async fn list_is_newest_first_and_counts_exchanges() {
        let dir = tempfile::tempdir().unwrap();
        for (id, ts, n) in [("s-old", 100, 1), ("s-new", 500, 3)] {
            let p = dir.path().join(format!("{id}.json"));
            let mut s = SessionContext::load_or_new(&p, id);
            for i in 0..n {
                s.record_turn(
                    Exchange { prompt: format!("q{i}"), attachments: vec![], report: None, ts },
                    TurnSummary {
                        user_prompt: format!("q{i}"),
                        plan_summary: "x".into(),
                        tool_summary: "x".into(),
                        report_summary: "x".into(),
                    },
                    &engine(),
                    &p,
                )
                .await
                .unwrap();
            }
        }
        let list = SessionContext::list(dir.path());
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "s-new", "newest first");
        assert_eq!(list[0].exchange_count, 3);
        assert_eq!(list[1].exchange_count, 1);
    }

    #[tokio::test]
    async fn transcript_keeps_every_turn_but_model_context_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let mut s = SessionContext::load_or_new(&p, "s");
        for i in 0..10 {
            s.record_turn(
                Exchange { prompt: format!("q{i}"), attachments: vec![], report: None, ts: i },
                TurnSummary {
                    user_prompt: format!("q{i}"),
                    plan_summary: "x".into(),
                    tool_summary: "x".into(),
                    report_summary: format!("a{i}"),
                },
                &engine(),
                &p,
            )
            .await
            .unwrap();
        }
        // Compression needs a live model; when it fails it keeps a raw digest, so
        // `turns` is trimmed either way.
        assert_eq!(s.exchanges.len(), 10, "transcript is complete");
        assert!(s.turns.len() <= MAX_VERBATIM_TURNS + 1, "model context is bounded");
    }

    #[test]
    fn title_truncates_long_prompts() {
        let long = "a".repeat(200);
        let t = title_from_prompt(&long);
        assert!(t.chars().count() <= 61);
        assert!(t.ends_with('…'));
    }
}
