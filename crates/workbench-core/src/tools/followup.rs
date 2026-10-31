//! `answer_followup` — answer a conversational follow-up from the session
//! transcript alone.
//!
//! Turns like *"and what about debit cards?"* or *"show that as a table"* have no
//! new attachments and need no knowledge base. Before this task existed the
//! planner had nothing valid to emit for them and the turn parked in the HITL
//! modal. Now it is a one-step plan that runs the session context through the
//! resident model.

use crate::engine::schemas::{TaskStep, ToolResult};
use crate::engine::ollama_engine::LiveOutput;
use crate::tools::{Tool, ToolContext};
use crate::Result;

pub struct AnswerFollowupTool;

#[async_trait::async_trait]
impl Tool for AnswerFollowupTool {
    fn name(&self) -> &'static str {
        "answer_followup"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        if ctx.session_blob.trim().is_empty() {
            return Ok(ToolResult::failure(
                &step.id,
                self.name(),
                "no earlier conversation to answer from",
                started.elapsed().as_millis(),
            ));
        }

        let instruction = "Answer the user's follow-up question using only the \
                           conversation so far. If the earlier turns do not \
                           contain the answer, say so plainly rather than \
                           guessing.";
        match ctx
            .engine
            .analyze(
                instruction,
                ctx.session_blob,
                ctx.prompt,
                Some(LiveOutput { sink: ctx.sink, tool: self.name() }),
            )
            .await
        {
            Ok(text) => Ok(ToolResult {
                step_id: step.id.clone(),
                task: self.name().into(),
                ok: true,
                data: serde_json::json!({ "text": text, "source": "conversation" }),
                error: None,
                warning: None,
                elapsed_ms: started.elapsed().as_millis(),
            }),
            Err(e) => Ok(ToolResult::failure(
                &step.id,
                self.name(),
                e.to_string(),
                started.elapsed().as_millis(),
            )),
        }
    }
}

/// `answer_directly` — a request with nothing to read: no files, no knowledge
/// base, no earlier turn. The sibling of [`AnswerFollowupTool`] for a fresh chat;
/// without it a plain "hi" had no valid plan at all.
pub struct AnswerDirectlyTool;

#[async_trait::async_trait]
impl Tool for AnswerDirectlyTool {
    fn name(&self) -> &'static str {
        "answer_directly"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();
        let live = LiveOutput { sink: ctx.sink, tool: self.name() };
        match ctx.engine.respond(ctx.prompt, Some(live)).await {
            Ok(text) => Ok(ToolResult {
                step_id: step.id.clone(),
                task: self.name().into(),
                ok: true,
                data: serde_json::json!({ "text": text, "source": "direct" }),
                error: None,
                warning: None,
                elapsed_ms: started.elapsed().as_millis(),
            }),
            Err(crate::CoreError::Cancelled) => Err(crate::CoreError::Cancelled),
            Err(e) => Ok(ToolResult::failure(
                &step.id,
                self.name(),
                e.to_string(),
                started.elapsed().as_millis(),
            )),
        }
    }
}
