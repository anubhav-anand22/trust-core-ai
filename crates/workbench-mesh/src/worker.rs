//! The Worker side at runtime: run the Host's jobs, one at a time, and keep nothing.
//!
//! A job's attachments are written to `scratch/job-<id>/`, because the tools
//! read files by path. The directory is removed when the job ends, however it
//! ends (done, failed, cancelled, the Host vanished), and the whole scratch dir
//! is wiped when the Worker starts, in case the app was killed mid-job.
//!
//! Logs carry job ids, task names and timings only: never the prompt, the
//! arguments, the file names or the results (MESH-12).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use workbench_core::engine::OllamaEngine;
use workbench_core::executor::ToolRegistry;
use workbench_core::mesh::{run_job, run_plan_job, PlanJob, StepJob};
use workbench_core::planner::PlanSource;
use workbench_core::{CancelFlag, PipelineConfig, ProgressSink, StepEvent};

use crate::frame::{FileBlob, Frame};
use crate::link::Link;

/// Builds this Worker's engine for one job: its own models (MESH-8) and caps,
/// with the job's mode and persona, and the job's cancel flag.
pub type EngineFactory = Arc<dyn Fn(&StepJob, CancelFlag) -> OllamaEngine + Send + Sync>;

/// Builds the planner for a delegated planning call: in the app, this Worker's
/// own engine with the job's persona. Separate from [`EngineFactory`] so tests
/// can plan without a model.
pub type PlannerFactory = Arc<dyn Fn(&PlanJob, CancelFlag) -> Box<dyn PlanSource> + Send + Sync>;

pub struct WorkerRuntime {
    pub registry: ToolRegistry,
    pub config: PipelineConfig,
    /// Per-job temp dirs live here. Wiped on start.
    pub scratch: PathBuf,
    pub make_engine: EngineFactory,
    pub make_planner: PlannerFactory,
}

/// What the Worker's own screen may show: the job's *type*, never its content.
#[derive(Clone, Debug, PartialEq)]
pub enum Activity {
    Idle,
    Busy { task: String },
}

/// Serve one Host until it disconnects or says goodbye. Returns why it ended.
pub async fn serve(
    link: Link,
    rt: Arc<WorkerRuntime>,
    activity: Option<mpsc::UnboundedSender<Activity>>,
) -> String {
    let _ = std::fs::remove_dir_all(&rt.scratch);
    if let Err(e) = std::fs::create_dir_all(&rt.scratch) {
        return format!("could not create the scratch folder: {e}");
    }
    let show = |a: Activity| {
        if let Some(tx) = &activity {
            let _ = tx.send(a);
        }
    };

    let (mut writer, mut reader, host) = link.split();
    tracing::info!(host = %host.device_id, "mesh worker: serving");
    let (outbox, mut out) = mpsc::unbounded_channel::<Frame>();
    let (frames_tx, mut frames) = mpsc::unbounded_channel::<Frame>();
    let (finished_tx, mut finished) = mpsc::unbounded_channel::<String>();

    // Guards, not bare handles: if the app drops this future (the user left the
    // mesh, the window closed), the link must close too, or the Host would keep
    // sending work to a Worker nobody is running.
    let _writer_task = AbortOnDrop(tokio::spawn(async move {
        while let Some(frame) = out.recv().await {
            if writer.send(&frame).await.is_err() {
                break;
            }
        }
    }));
    let _reader_task = AbortOnDrop(tokio::spawn(async move {
        while let Ok(frame) = reader.recv().await {
            if frames_tx.send(frame).is_err() {
                break;
            }
        }
    }));

    let mut current: Option<(String, CancelFlag, AbortOnDrop)> = None;
    let reason = loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                None => break "the Host disconnected".to_string(),
                Some(Frame::Job { job_id, job, files }) => {
                    if current.is_some() {
                        // The Host schedules one job per Worker; this is a guard, not a queue.
                        let _ = outbox.send(Frame::Failed { job_id, message: "this device is busy".into() });
                        continue;
                    }
                    tracing::info!(job = %job_id, task = %job.step.task, "mesh worker: job start");
                    show(Activity::Busy { task: job.step.task.clone() });
                    let flag = CancelFlag::new();
                    let handle = tokio::spawn(run_one(
                        job_id.clone(), job, files, rt.clone(), flag.clone(), outbox.clone(), finished_tx.clone(),
                    ));
                    current = Some((job_id, flag, AbortOnDrop(handle)));
                }
                Some(Frame::PlanJob { job_id, job }) => {
                    if current.is_some() {
                        let _ = outbox.send(Frame::Failed { job_id, message: "this device is busy".into() });
                        continue;
                    }
                    tracing::info!(job = %job_id, "mesh worker: planning call");
                    show(Activity::Busy { task: "planning".into() });
                    let flag = CancelFlag::new();
                    let handle = tokio::spawn(plan_one(
                        job_id.clone(), job, rt.clone(), flag.clone(), outbox.clone(), finished_tx.clone(),
                    ));
                    current = Some((job_id, flag, AbortOnDrop(handle)));
                }
                Some(Frame::Cancel { job_id }) => {
                    if let Some((id, flag, _)) = &current {
                        if job_id.as_deref().map_or(true, |j| j == id) {
                            tracing::info!(job = %id, "mesh worker: cancelled by the Host");
                            flag.cancel();
                        }
                    }
                }
                Some(Frame::Ping { n }) | Some(Frame::Probe { n, .. }) => {
                    let _ = outbox.send(Frame::Pong { n });
                }
                Some(Frame::Bye { reason }) => break reason,
                Some(_) => {}
            },
            Some(job_id) = finished.recv() => {
                if current.as_ref().is_some_and(|(id, _, _)| *id == job_id) {
                    current = None;
                    show(Activity::Idle);
                }
            }
        }
    };

    if let Some((_, flag, mut job)) = current.take() {
        flag.cancel();
        // Aborting drops the job's future, which runs its cleanup (the temp dir goes).
        job.0.abort();
        let _ = (&mut job.0).await;
    }
    show(Activity::Idle);
    tracing::info!(reason, "mesh worker: stopped serving");
    reason
}

async fn run_one(
    job_id: String,
    mut job: StepJob,
    files: Vec<FileBlob>,
    rt: Arc<WorkerRuntime>,
    flag: CancelFlag,
    outbox: mpsc::UnboundedSender<Frame>,
    finished: mpsc::UnboundedSender<String>,
) {
    let started = std::time::Instant::now();
    let dir = rt.scratch.join(format!("job-{}", safe_name(&job_id)));
    let _cleanup = RemoveOnDrop(dir.clone());

    let outcome: Result<_, String> = async {
        if files.len() != job.files.len() {
            return Err("the job's files did not arrive with it".to_string());
        }
        tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
        for (i, (file, blob)) in job.files.iter_mut().zip(files).enumerate() {
            let path = dir.join(format!("{i}-{}", safe_name(&blob.original_name)));
            tokio::fs::write(&path, &blob.bytes).await.map_err(|e| e.to_string())?;
            file.path = path.to_string_lossy().into_owned();
        }
        let engine = (rt.make_engine)(&job, flag);
        let sink = Forward { job_id: job_id.clone(), outbox: outbox.clone() };
        let result = run_job(&job, &rt.registry, &rt.config, &engine, &sink).await.map_err(|e| e.to_string())?;
        Ok((result, engine.timings()))
    }
    .await;

    let elapsed_ms = started.elapsed().as_millis();
    match outcome {
        Ok((result, timings)) => {
            tracing::info!(job = %job_id, ok = result.ok, elapsed_ms, "mesh worker: job done");
            let _ = outbox.send(Frame::Done { job_id: job_id.clone(), result, timings });
        }
        Err(message) => {
            tracing::warn!(job = %job_id, elapsed_ms, "mesh worker: job could not run");
            let _ = outbox.send(Frame::Failed { job_id: job_id.clone(), message });
        }
    }
    let _ = finished.send(job_id);
}

async fn plan_one(
    job_id: String,
    job: PlanJob,
    rt: Arc<WorkerRuntime>,
    flag: CancelFlag,
    outbox: mpsc::UnboundedSender<Frame>,
    finished: mpsc::UnboundedSender<String>,
) {
    let started = std::time::Instant::now();
    let planner = (rt.make_planner)(&job, flag);
    let frame = match run_plan_job(&job, planner.as_ref()).await {
        Ok(reply) => Frame::Planned { job_id: job_id.clone(), reply },
        Err(e) => Frame::Failed { job_id: job_id.clone(), message: e.to_string() },
    };
    tracing::info!(job = %job_id, elapsed_ms = started.elapsed().as_millis(), "mesh worker: planning call done");
    let _ = outbox.send(frame);
    let _ = finished.send(job_id);
}

/// Sends the job's progress to the Host.
struct Forward {
    job_id: String,
    outbox: mpsc::UnboundedSender<Frame>,
}

impl ProgressSink for Forward {
    fn emit(&self, event: StepEvent) {
        let _ = self.outbox.send(Frame::Event { job_id: self.job_id.clone(), event });
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file-system-safe stem: the name comes from the Host and is not trusted.
fn safe_name(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(64)
        .collect();
    let s = s.trim_start_matches('.').to_string();
    if s.is_empty() { "file".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn a_hostile_file_name_cannot_leave_the_job_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name("report (final).pdf"), "report__final_.pdf");
        assert!(!safe_name("a/b\\c").contains(['/', '\\']));
    }
}
