//! TrustCore mesh — the network side of `workbench_core::mesh`.
//!
//! A Host lends its plan steps to Workers on the same LAN (docs/12-mesh.md).
//! This crate is how they meet and talk:
//!   * [`code`]      — the pairing code the Host shows and a Worker types in
//!   * [`identity`]  — each device's long-term key, and the devices it trusts
//!   * [`link`]      — PAKE + Noise handshake, then encrypted frames over a WebSocket
//!   * [`frame`]     — what travels on a link
//!   * [`discovery`] — finding the Host on the LAN by its code (mDNS)
//!   * [`scheduler`] — which device runs a step, and who plans (pure rules)
//!   * [`hub`]       — the Host at runtime: Workers, consent, heartbeats, `Dispatcher`
//!   * [`worker`]    — the Worker at runtime: run one job at a time, keep nothing
//!
//! Like `workbench-core`, it is Tauri-free, so its tests run without building the app.

pub mod code;
pub mod discovery;
pub mod frame;
pub mod hub;
pub mod identity;
pub mod link;
pub mod scheduler;
pub mod worker;

/// Bumped on any change to the handshake or the frame format. A mismatch is
/// rejected with a clear message instead of a confusing decode error.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),

    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("encryption error: {0}")]
    Noise(#[from] snow::Error),

    #[error("malformed frame: {0}")]
    Json(#[from] serde_json::Error),

    /// The other side said no, with its reason ("wrong code", "unknown device"…).
    #[error("{0}")]
    Rejected(String),

    #[error("the other device speaks mesh protocol {theirs}, this one {ours}; update both to the same TrustCore version")]
    ProtocolMismatch { ours: u32, theirs: u32 },

    #[error("the connection closed")]
    Closed,

    #[error("protocol violation: {0}")]
    Protocol(String),

    #[error("timed out: {0}")]
    Timeout(&'static str),

    #[error("could not read or write mesh keys: {0}")]
    Store(String),
}

pub type Result<T, E = MeshError> = std::result::Result<T, E>;
