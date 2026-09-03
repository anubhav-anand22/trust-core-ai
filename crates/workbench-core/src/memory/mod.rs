//! Two-tier memory, per the blueprint's "Hierarchical Memory Architecture".
//!
//! * [`SessionContext`] — this session's scratch. Plain JSON, rolling compression.
//! * [`PersistentMemory`] — user-level history. AES-256-GCM at rest, machine-derived key.

pub mod persistent;
pub mod session;

pub use persistent::PersistentMemory;
pub use session::{SessionContext, TurnSummary};
