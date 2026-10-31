//! Mesh execution: run one validated plan across the Host and paired Workers.
//!
//! The Host keeps every piece of state (session, memory, knowledge base, plan,
//! results). A Worker is handed one [`StepJob`] at a time, like a function call:
//! the step, the outputs it depends on, the one attachment it reads, and the
//! session blob only for the tool that answers from it. It returns a
//! [`ToolResult`] and keeps nothing.
//!
//! This module is network-free. The Host's link layer (`src-tauri/src/mesh/`)
//! implements [`Dispatcher`]; tests use an in-process fake.
//!
//! With the mesh off, none of this runs: [`crate::pipeline::run_turn`] still calls
//! the sequential [`execute_plan`](crate::executor::execute_plan), unchanged.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

use crate::engine::schemas::{InputFile, IntentResult, Plan, TaskStep, ToolResult};
use crate::engine::OllamaEngine;
use crate::events::{ProgressSink, StepEvent};
use crate::executor::executor::{finish_step, run_step};
use crate::executor::ToolRegistry;
use crate::planner::{lookup, PlanSource, Stage};
use crate::tools::{resolve_file, ToolContext};
use crate::{CoreError, PipelineConfig, Result, TurnMode};

/// Tools that read [`ToolContext::session_blob`]. Every other job is sent with an
/// empty blob, so the conversation only leaves the Host when the step needs it.
/// Keep in sync with the tools: today only `followup.rs` reads it.
const NEEDS_SESSION_BLOB: &[&str] = &["answer_followup"];

/// How often the executor looks at the Stop flag while every job is still running.
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// Where one step runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Node {
    /// This device.
    Local,
    /// A paired Worker, by device id.
    Worker(String),
}

/// Everything a Worker needs to run one step, and nothing more.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepJob {
    pub step: TaskStep,
    /// Outputs of the steps this one reads, keyed by step id.
    pub dep_outputs: HashMap<String, serde_json::Value>,
    /// The attachment this step reads, if any. Paths are the Host's: the link
    /// layer ships the bytes and rewrites the path to the Worker's temp dir.
    pub files: Vec<InputFile>,
    /// The user's ask, verbatim (vision question, RAG query fallback, analysis).
    pub prompt: String,
    /// Empty unless the tool is in [`NEEDS_SESSION_BLOB`].
    pub session_blob: String,
    /// The Worker builds its engine with this mode, so Deep reads stay deep.
    pub mode: TurnMode,
    /// The profile line the Host's engine prepends to its system prompts
    /// (`OllamaEngine::with_persona`). The Worker's engine needs the same one, or
    /// its answer is framed differently.
    #[serde(default)]
    pub persona: String,
}

/// Places steps and runs jobs on Workers. Implemented by the Host's link layer.
#[async_trait::async_trait]
pub trait Dispatcher: Send + Sync {
    /// Nodes that may run `step`, best first. Asked again before every attempt,
    /// so Workers joining or leaving mid-turn are seen. Empty means "the Host".
    fn candidates(&self, step: &TaskStep) -> Vec<Node>;

    /// Run `job` on one Worker.
    ///
    /// `Ok` means the tool ran there, even if the result is `ok: false`.
    /// `Err(Cancelled)` ends the turn. Any other `Err` means the Worker could not
    /// run it (dropped, timed out, refused): the executor tries the next
    /// candidate, then the Host.
    async fn run_on_worker(
        &self,
        worker: &str,
        job: StepJob,
        sink: &dyn ProgressSink,
    ) -> Result<ToolResult>;

    /// Stop was pressed: tell the Workers to abandon their jobs.
    async fn cancel_all(&self) {}

    /// The name the audit panel shows for a Worker ("gpu-box"). `None` = its id.
    fn device_name(&self, worker: &str) -> Option<String> {
        let _ = worker;
        None
    }

    /// The Worker that should plan this turn, or `None` for the Host (MESH-10).
    fn delegator(&self) -> Option<String> {
        None
    }

    /// Run one planner model call on `worker`. Errors mean the same as for
    /// [`Dispatcher::run_on_worker`]: [`RemotePlanner`] falls back to the Host.
    async fn plan_on_worker(&self, worker: &str, job: PlanJob) -> Result<PlanReply> {
        let _ = job;
        Err(CoreError::Tool { tool: format!("mesh:{worker}"), message: "this mesh can't plan remotely".into() })
    }
}

/// One planner model call for a delegator Worker (MESH-9). Only the *model call*
/// moves: the Host still validates, retries, completes the plan and parks it for
/// the user, because validation checks attachments that exist only on the Host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanJob {
    pub call: PlanCall,
    /// As [`StepJob::persona`]: the planner's system prompts carry it too.
    #[serde(default)]
    pub persona: String,
}

/// The two calls of [`PlanSource`]. `uploads` carry names and kinds only: the
/// path is replaced with the name, so the Host's folders never leave it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCall {
    ParseIntent { prompt: String, history: String, uploads: Vec<InputFile> },
    BuildPlan {
        prompt: String,
        history: String,
        intent: IntentResult,
        uploads: Vec<InputFile>,
        previous_errors: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReply {
    Intent(IntentResult),
    Plan(Plan),
}

/// Run one planner call on this device. A delegator Worker calls this.
pub async fn run_plan_job(job: &PlanJob, source: &dyn PlanSource) -> Result<PlanReply> {
    match &job.call {
        PlanCall::ParseIntent { prompt, history, uploads } => {
            source.parse_intent(prompt, history, uploads).await.map(PlanReply::Intent)
        }
        PlanCall::BuildPlan { prompt, history, intent, uploads, previous_errors } => source
            .build_plan(prompt, history, intent, uploads, previous_errors)
            .await
            .map(PlanReply::Plan),
    }
}

/// A [`PlanSource`] that asks the delegator Worker, and falls back to the Host's
/// own model for the rest of the turn the first time the Worker can't answer.
/// `plan_turn` runs on the Host unchanged around it.
pub struct RemotePlanner<'a> {
    dispatcher: &'a dyn Dispatcher,
    worker: String,
    local: &'a dyn PlanSource,
    /// Polled while waiting on the Worker, so Stop works during planning too.
    engine: &'a OllamaEngine,
    sink: &'a dyn ProgressSink,
    gave_up: std::sync::atomic::AtomicBool,
}

impl<'a> RemotePlanner<'a> {
    pub fn new(
        dispatcher: &'a dyn Dispatcher,
        worker: String,
        local: &'a dyn PlanSource,
        engine: &'a OllamaEngine,
        sink: &'a dyn ProgressSink,
    ) -> Self {
        Self { dispatcher, worker, local, engine, sink, gave_up: Default::default() }
    }

    /// `Ok(None)` = the Worker couldn't answer; plan locally.
    async fn ask(&self, call: PlanCall) -> Result<Option<PlanReply>> {
        use std::sync::atomic::Ordering;
        if self.gave_up.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.engine.cancel_check()?;
        let job = PlanJob { call, persona: self.engine.persona().to_string() };
        let remote = self.dispatcher.plan_on_worker(&self.worker, job);
        tokio::pin!(remote);
        let outcome = loop {
            match tokio::time::timeout(CANCEL_POLL, &mut remote).await {
                Ok(outcome) => break outcome,
                Err(_) if self.engine.cancel_check().is_err() => {
                    self.dispatcher.cancel_all().await;
                    return Err(CoreError::Cancelled);
                }
                Err(_) => continue,
            }
        };
        match outcome {
            Ok(reply) => Ok(Some(reply)),
            Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
            Err(e) => {
                self.gave_up.store(true, Ordering::SeqCst);
                crate::events::warn(
                    self.sink,
                    "The mesh device planning this turn couldn't answer; planning on this device.",
                    Some(e.to_string()),
                );
                tracing::warn!(worker = %self.worker, error = %e, "mesh: delegated planning failed, planning locally");
                Ok(None)
            }
        }
    }
}

/// Names and kinds only (see [`PlanCall`]).
fn without_paths(uploads: &[InputFile]) -> Vec<InputFile> {
    uploads
        .iter()
        .map(|f| InputFile { path: f.original_name.clone(), kind: f.kind, original_name: f.original_name.clone() })
        .collect()
}

#[async_trait::async_trait]
impl PlanSource for RemotePlanner<'_> {
    async fn parse_intent(&self, prompt: &str, history: &str, uploads: &[InputFile]) -> Result<IntentResult> {
        let call = PlanCall::ParseIntent {
            prompt: prompt.to_string(),
            history: history.to_string(),
            uploads: without_paths(uploads),
        };
        match self.ask(call).await? {
            Some(PlanReply::Intent(intent)) => Ok(intent),
            Some(PlanReply::Plan(_)) => Err(CoreError::Tool {
                tool: format!("mesh:{}", self.worker),
                message: "answered an intent call with a plan".into(),
            }),
            None => self.local.parse_intent(prompt, history, uploads).await,
        }
    }

    async fn build_plan(
        &self,
        prompt: &str,
        history: &str,
        intent: &IntentResult,
        uploads: &[InputFile],
        previous_errors: &[String],
    ) -> Result<Plan> {
        let call = PlanCall::BuildPlan {
            prompt: prompt.to_string(),
            history: history.to_string(),
            intent: intent.clone(),
            uploads: without_paths(uploads),
            previous_errors: previous_errors.to_vec(),
        };
        match self.ask(call).await? {
            Some(PlanReply::Plan(plan)) => Ok(plan),
            Some(PlanReply::Intent(_)) => Err(CoreError::Tool {
                tool: format!("mesh:{}", self.worker),
                message: "answered a plan call with an intent".into(),
            }),
            None => self.local.build_plan(prompt, history, intent, uploads, previous_errors).await,
        }
    }
}

/// Run one job on this device. A Worker calls this with its own engine (built
/// with `job.mode`), and the Host calls it for steps placed on [`Node::Local`].
pub async fn run_job(
    job: &StepJob,
    registry: &ToolRegistry,
    config: &PipelineConfig,
    engine: &OllamaEngine,
    sink: &dyn ProgressSink,
) -> Result<ToolResult> {
    let ctx = ToolContext {
        config,
        uploads: &job.files,
        outputs: &job.dep_outputs,
        prompt: &job.prompt,
        session_blob: &job.session_blob,
        engine,
        sink,
    };
    run_step(registry, &job.step, &ctx).await
}

/// Analysis steps fall back to *every* earlier output when their dependencies
/// give no evidence (`tools/analysis.rs`). Such a step waits for all earlier
/// steps and gets all their outputs, which is exactly what the sequential
/// executor gives it. Unknown tasks (e.g. the folded `assess`) are treated the
/// same way, the safe side.
fn reads_prior_outputs(task: &str) -> bool {
    !matches!(lookup(task).map(|t| t.stage), Some(Stage::Extract | Stage::Retrieve))
}

/// The pieces of the turn every job is cut from.
struct Turn<'a> {
    plan: &'a Plan,
    uploads: &'a [InputFile],
    prompt: &'a str,
    session_blob: &'a str,
    mode: TurnMode,
    persona: &'a str,
    /// Step id → plan index (first occurrence).
    index_of: HashMap<&'a str, usize>,
}

impl<'a> Turn<'a> {
    /// Earlier steps that `i` must wait for.
    fn waits_for(&self, i: usize) -> Vec<usize> {
        let step = &self.plan.steps[i];
        if reads_prior_outputs(&step.task) {
            (0..i).collect()
        } else {
            step.depends_on
                .iter()
                .filter_map(|d| self.index_of.get(d.as_str()).copied())
                .filter(|&j| j < i)
                .collect()
        }
    }

    fn job(&self, i: usize, outputs: &[Option<serde_json::Value>]) -> StepJob {
        let step = &self.plan.steps[i];

        // In plan order, so a duplicated id resolves as the sequential map would.
        let mut dep_outputs = HashMap::new();
        for j in self.waits_for(i) {
            if let Some(data) = &outputs[j] {
                dep_outputs.insert(self.plan.steps[j].id.clone(), data.clone());
            }
        }

        let files = lookup(&step.task)
            .and_then(|spec| spec.requires_file)
            .and_then(|kind| resolve_file(self.uploads, step, kind))
            .cloned()
            .into_iter()
            .collect();

        let session_blob = if NEEDS_SESSION_BLOB.contains(&step.task.as_str()) {
            self.session_blob.to_string()
        } else {
            String::new()
        };

        StepJob {
            step: step.clone(),
            dep_outputs,
            files,
            prompt: self.prompt.to_string(),
            session_blob,
            mode: self.mode,
            persona: self.persona.to_string(),
        }
    }
}

/// Which node takes a step now, or `None` to wait for one to free up.
///
/// Untried candidates in order, skipping busy ones. When every candidate has
/// failed, the Host runs it: a turn never dies because a Worker did.
fn pick(candidates: &[Node], tried: &[Node], busy: &HashSet<Node>) -> Option<Node> {
    let untried: Vec<&Node> = candidates.iter().filter(|n| !tried.contains(n)).collect();
    if untried.is_empty() {
        return (!busy.contains(&Node::Local)).then_some(Node::Local);
    }
    untried.into_iter().find(|n| !busy.contains(n)).cloned()
}

/// `Send`, because the Tauri host awaits the whole turn on its multi-threaded runtime.
type InFlight<'a> = Pin<Box<dyn Future<Output = (usize, Node, Result<ToolResult>)> + Send + 'a>>;

/// [`execute_plan`](crate::executor::execute_plan) across devices.
///
/// Ready steps (everything they wait for has finished) run at the same time on
/// different nodes. Each node runs **one job at a time**, so the one-resident-model
/// rule still holds per device. Results come back in plan order, so the quality
/// gate and the compiler see what they always saw.
#[allow(clippy::too_many_arguments)]
pub async fn execute_plan_mesh(
    plan: &Plan,
    registry: &ToolRegistry,
    uploads: &[InputFile],
    config: &PipelineConfig,
    engine: &OllamaEngine,
    prompt: &str,
    session_blob: &str,
    sink: &dyn ProgressSink,
    dispatcher: &dyn Dispatcher,
) -> Result<Vec<ToolResult>> {
    let total = plan.steps.len();
    let mut index_of = HashMap::new();
    for (i, s) in plan.steps.iter().enumerate() {
        index_of.entry(s.id.as_str()).or_insert(i);
    }
    let turn = Turn {
        plan,
        uploads,
        prompt,
        session_blob,
        mode: engine.mode(),
        persona: engine.persona(),
        index_of,
    };

    let mut results: Vec<Option<ToolResult>> = vec![None; total];
    let mut outputs: Vec<Option<serde_json::Value>> = vec![None; total];
    let mut started = vec![false; total];
    let mut tried: Vec<Vec<Node>> = vec![Vec::new(); total];
    let mut busy: HashSet<Node> = HashSet::new();
    let mut in_flight: FuturesUnordered<InFlight<'_>> = FuturesUnordered::new();

    tracing::info!(
        steps = total,
        plan = ?plan.steps.iter().map(|s| s.task.as_str()).collect::<Vec<_>>(),
        "executing plan on the mesh"
    );

    while results.iter().any(Option::is_none) {
        if engine.cancel_check().is_err() {
            tracing::info!("mesh run cancelled by user");
            dispatcher.cancel_all().await;
            return Err(CoreError::Cancelled);
        }

        // Dispatch every ready step that has a free node, in plan order.
        for i in 0..total {
            if started[i] || !turn.waits_for(i).iter().all(|&j| results[j].is_some()) {
                continue;
            }
            let step = &plan.steps[i];
            let Some(node) = pick(&dispatcher.candidates(step), &tried[i], &busy) else {
                continue;
            };
            started[i] = true;
            busy.insert(node.clone());
            sink.emit(StepEvent::ExecutingTool { tool: step.task.clone(), index: i, total });
            tracing::info!(
                step = %step.id,
                tool = %step.task,
                position = format!("{}/{}", i + 1, total),
                ?node,
                "mesh: dispatch"
            );

            let job = turn.job(i, &outputs);
            let run: InFlight<'_> = match node.clone() {
                Node::Local => Box::pin(async move {
                    let r = run_job(&job, registry, config, engine, sink).await;
                    (i, Node::Local, r)
                }),
                Node::Worker(id) => Box::pin(async move {
                    let r = dispatcher.run_on_worker(&id, job, sink).await;
                    (i, Node::Worker(id), r)
                }),
            };
            in_flight.push(run);
        }

        if in_flight.is_empty() {
            // Unreachable for any plan: the lowest unfinished step only waits on
            // earlier, finished steps, and with nothing running every node is free.
            tracing::error!("mesh scheduler stalled with steps left");
            return Err(CoreError::Tool {
                tool: "mesh".into(),
                message: "scheduler stalled with steps left".into(),
            });
        }

        // Wake at least every CANCEL_POLL so Stop is seen while jobs run.
        let Ok(Some((i, node, outcome))) =
            tokio::time::timeout(CANCEL_POLL, in_flight.next()).await
        else {
            continue;
        };
        busy.remove(&node);
        let step = &plan.steps[i];

        match outcome {
            Ok(mut result) => {
                // The Host owns the plan: a Worker cannot relabel a result.
                result.step_id = step.id.clone();
                result.task = step.task.clone();
                let device = match &node {
                    Node::Local => None,
                    Node::Worker(id) => Some(dispatcher.device_name(id).unwrap_or_else(|| id.clone())),
                };
                finish_step(step, &result, device, sink);
                if result.ok {
                    outputs[i] = Some(result.data.clone());
                }
                results[i] = Some(result);
            }
            Err(CoreError::Cancelled) => {
                dispatcher.cancel_all().await;
                return Err(CoreError::Cancelled);
            }
            Err(e) => {
                // Only a Worker can get here: a local run errs only on Cancelled.
                crate::events::warn(
                    sink,
                    format!("A mesh device couldn't run '{}'; running it elsewhere.", step.task),
                    Some(e.to_string()),
                );
                tracing::warn!(step = %step.id, ?node, error = %e, "mesh: job failed, reassigning");
                tried[i].push(node);
                started[i] = false;
            }
        }
    }

    Ok(results.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: &str) -> Node {
        Node::Worker(id.into())
    }

    #[test]
    fn pick_prefers_the_first_free_untried_candidate() {
        let busy: HashSet<Node> = [w("a")].into_iter().collect();
        assert_eq!(pick(&[w("a"), w("b")], &[], &busy), Some(w("b")));
        assert_eq!(pick(&[w("a")], &[], &busy), None, "waits for a busy preferred worker");
    }

    #[test]
    fn pick_falls_back_to_the_host_only_when_every_candidate_failed() {
        let none = HashSet::new();
        assert_eq!(pick(&[], &[], &none), Some(Node::Local));
        assert_eq!(pick(&[w("a")], &[w("a")], &none), Some(Node::Local));
        let host_busy: HashSet<Node> = [Node::Local].into_iter().collect();
        assert_eq!(pick(&[w("a")], &[w("a")], &host_busy), None);
    }

    #[test]
    fn only_extract_and_retrieve_steps_skip_the_barrier() {
        assert!(!reads_prior_outputs("parse_pdf"));
        assert!(!reads_prior_outputs("search_knowledge"));
        assert!(reads_prior_outputs("summarize"));
        assert!(reads_prior_outputs("assess"), "unregistered tasks take the safe side");
    }
}
