//! Diagnostics for the whole app: Rust pipeline *and* React front-end.
//!
//! `workbench-core` emits `tracing` events throughout, but a `tracing` macro with
//! no subscriber installed is a no-op — every one of those events was being
//! discarded before this module existed. The host installs the subscriber once at
//! startup and fans events out to two places:
//!
//! * **stdout** — visible in the `npm run tauri dev` terminal while developing;
//! * **a daily rotating file** under the app-data dir — what you ask a user to
//!   send you when something misbehaves on their machine.
//!
//! The front-end ships its own events through [`ui_log`] so a single file holds
//! the whole story of a turn: the button that was clicked, the command it
//! invoked, every pipeline stage, and the failure at the end.
//!
//! Verbosity is controlled by the `WB_LOG` environment variable using the usual
//! `RUST_LOG` syntax (e.g. `WB_LOG=debug`, or
//! `WB_LOG=workbench_core::tools=trace,info`). The default is chatty enough to
//! debug a turn without drowning in Tauri/hyper internals.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Directory (under app data) holding the rotating log files.
pub const LOG_DIR: &str = "logs";
/// Basename; `tracing-appender` appends the date, e.g. `workbench.log.2026-09-05`.
const LOG_FILE: &str = "workbench.log";

/// Default verbosity when `WB_LOG` is unset.
///
/// Our own crates at debug (the point of the exercise); dependencies at warn, or
/// the webview/HTTP internals bury everything useful.
///
/// **Every source we care about needs its own directive here.** A target that
/// matches no directive inherits the leading `warn`, which silently drops its
/// `info!` and `debug!` events — there is no error, the lines simply never
/// appear. Note in particular:
///
/// * `ui` is the target [`ui_log`] stamps on front-end events. Without it, the
///   whole React side of the log goes missing (this was a real bug).
/// * `tauri_app` matches by prefix, so it covers the real crate name
///   `tauri_app_lib` as well.
///
/// [`default_filter_admits_our_targets`] guards this string against edits that
/// drop a source by accident.
const DEFAULT_FILTER: &str = "warn,tauri_app=debug,workbench_core=debug,ui=debug";

/// Install the global subscriber. Call once, as early as possible.
///
/// **Returns** a guard that must be held for the lifetime of the process — the
/// file writer is non-blocking and dropping the guard stops the flushing thread,
/// silently losing buffered lines.
pub fn init(app_data_dir: &Path) -> WorkerGuard {
    let log_dir = app_data_dir.join(LOG_DIR);
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, LOG_FILE);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter =
        EnvFilter::try_from_env("WB_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        // One filter for the whole registry — both sinks want the same verbosity,
        // and a single `EnvFilter` layer avoids needing per-layer filtering.
        .with(filter)
        // Console: compact, for the dev terminal.
        .with(fmt::layer().with_target(true).with_level(true).compact())
        // File: full detail, no ANSI colour codes to corrupt the file.
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_target(true)
                .with_level(true)
                .with_thread_ids(true)
                .with_ansi(false),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        log_dir = %log_dir.display(),
        "workbench starting"
    );

    guard
}

/// Absolute path of the log *directory*, for the audit sidebar / "open logs".
pub fn log_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(LOG_DIR)
}

/// Severity accepted from the front-end.
#[derive(serde::Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum UiLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Record a front-end event into the same log stream as the backend.
///
/// The UI calls this for every user action and every pipeline event it receives,
/// so one file shows the click, the command, and the pipeline's reaction in
/// order. `fields` is free-form JSON so the caller can attach whatever context
/// matters without a schema change here.
#[tauri::command]
pub fn ui_log(level: UiLevel, message: String, fields: Option<serde_json::Value>) {
    let ctx = fields
        .filter(|v| !v.is_null())
        .map(|v| v.to_string())
        .unwrap_or_default();

    match level {
        UiLevel::Debug => tracing::debug!(target: "ui", ctx = %ctx, "{message}"),
        UiLevel::Info => tracing::info!(target: "ui", ctx = %ctx, "{message}"),
        UiLevel::Warn => tracing::warn!(target: "ui", ctx = %ctx, "{message}"),
        UiLevel::Error => tracing::error!(target: "ui", ctx = %ctx, "{message}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{EnvFilter, Layer};

    use super::DEFAULT_FILTER;

    /// Records the target of every event that survives the filter.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }

    /// The bug this guards against: `ui` matched no directive in [`DEFAULT_FILTER`],
    /// so it inherited the leading `warn` and every front-end `info!` was dropped
    /// before reaching either sink — a silent failure that looked like the React
    /// logging simply not being wired up.
    ///
    /// All four events live in one test because `tracing` caches callsite interest
    /// per callsite; splitting them across tests would let one subscriber's verdict
    /// leak into another.
    #[test]
    fn default_filter_admits_our_targets_but_not_dependencies() {
        let captured = Captured::default();
        let seen = captured.0.clone();

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(DEFAULT_FILTER))
            .with(captured);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "ui", "front-end event");
            tracing::info!(target: "workbench_core::pipeline", "pipeline event");
            tracing::info!(target: "tauri_app_lib::logging", "host event");
            // A stand-in for the webview/HTTP crates whose internals bury ours.
            tracing::info!(target: "hyper::client", "noisy dependency");
        });

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|t| t == "ui"),
            "front-end logs are being filtered out; DEFAULT_FILTER needs a `ui` \
             directive. Admitted targets: {seen:?}"
        );
        assert!(
            seen.iter().any(|t| t == "workbench_core::pipeline"),
            "pipeline logs filtered out. Admitted targets: {seen:?}"
        );
        assert!(
            seen.iter().any(|t| t == "tauri_app_lib::logging"),
            "host logs filtered out — `tauri_app` should prefix-match the real \
             crate name `tauri_app_lib`. Admitted targets: {seen:?}"
        );
        assert!(
            !seen.iter().any(|t| t == "hyper::client"),
            "dependency chatter at info is getting through and will bury our own \
             lines. Admitted targets: {seen:?}"
        );
    }
}
