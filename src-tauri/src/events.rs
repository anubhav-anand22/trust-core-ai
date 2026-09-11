//! Adapter: `workbench_core::StepEvent` → Tauri IPC channel.
//!
//! `workbench-core` knows nothing about Tauri; it just calls
//! [`ProgressSink::emit`]. The front-end creates a `Channel<StepEvent>` and passes
//! it into `submit_turn`; this wrapper forwards each event onto it.

use tauri::ipc::Channel;
use workbench_core::{ProgressSink, StepEvent};

/// Forwards pipeline progress to the front-end stepper.
pub struct ChannelSink(pub Channel<StepEvent>);

impl ProgressSink for ChannelSink {
    fn emit(&self, event: StepEvent) {
        // A send failure just means the window went away mid-run; nothing to do.
        let _ = self.0.send(event);
    }
}
