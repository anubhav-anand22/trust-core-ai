//! What travels on a link, once the handshake has made it private.
//!
//! JSON, because every payload (`StepJob`, `ToolResult`, `StepEvent`) is already
//! serde, and a frame log is readable when debugging. Attachments ride as base64
//! inside a `Job`: about a third bigger on the wire, which is noise next to
//! running a model on a LAN.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use workbench_core::mesh::StepJob;
use workbench_core::schemas::{FileKind, ToolResult};
use workbench_core::StepEvent;

/// The first frame each side sends after the handshake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    /// Hardware, models and measured speed. The schema is owned by the
    /// scheduler; the link only carries it.
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// Externally tagged (`{"done": {...}}`), not `#[serde(tag = "t")]`: serde
/// decodes an internally tagged enum through a buffer that can't hold a `u128`,
/// and `ToolResult::elapsed_ms` is one. The round-trip test below pins this.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frame {
    Hello(Hello),
    /// Host → Worker: run this step. `files` are the bytes of `job.files`, in order.
    Job { job_id: String, job: StepJob, files: Vec<FileBlob> },
    /// Worker → Host: live progress from the running job (streamed text, deep-read windows).
    Event { job_id: String, event: StepEvent },
    /// Worker → Host: the tool ran. `result.ok` may still be false.
    Done { job_id: String, result: ToolResult },
    /// Worker → Host: the job could not run here (missing model, busy, panicked).
    Failed { job_id: String, message: String },
    /// Host → Worker: Stop was pressed. `None` cancels everything.
    Cancel { job_id: Option<String> },
    Ping { n: u64 },
    Pong { n: u64 },
    /// Host → Worker at join: answer with `Pong { n }`. The round trip of the
    /// padding estimates the link speed for the file-transfer budget.
    Probe {
        n: u64,
        #[serde(serialize_with = "to_b64", deserialize_with = "from_b64")]
        padding: Vec<u8>,
    },
    /// Either side, before closing, so the other can show why.
    Bye { reason: String },
}

/// One attachment's bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileBlob {
    pub original_name: String,
    pub kind: FileKind,
    #[serde(serialize_with = "to_b64", deserialize_with = "from_b64")]
    pub bytes: Vec<u8>,
}

fn to_b64<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&B64.encode(bytes))
}

fn from_b64<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    B64.decode(s).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(frame: Frame) {
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}: {json}"));
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    fn result(elapsed_ms: u128, data: serde_json::Value) -> ToolResult {
        ToolResult {
            step_id: "p1".into(),
            task: "parse_pdf".into(),
            ok: true,
            data,
            error: None,
            warning: None,
            elapsed_ms,
        }
    }

    #[test]
    fn every_frame_survives_the_wire() {
        round_trip(Frame::Done { job_id: "j".into(), result: result(5, serde_json::json!({"text": "x"})) });
        round_trip(Frame::Done { job_id: "j".into(), result: result(0, serde_json::json!({"bytes": 4096, "ran_on": "w"})) });
        round_trip(Frame::Done { job_id: "j".into(), result: result(u64::MAX as u128 + 1, serde_json::Value::Null) });
        round_trip(Frame::Event {
            job_id: "j".into(),
            event: StepEvent::Streaming { tool: "parse_pdf".into(), text: "reading".into() },
        });
        round_trip(Frame::Event { job_id: "j".into(), event: StepEvent::Done { report_json: "{}".into() } });
        round_trip(Frame::Probe { n: 1, padding: vec![1, 2, 3] });
        round_trip(Frame::Cancel { job_id: None });
        round_trip(Frame::Hello(Hello { name: "w".into(), capabilities: serde_json::json!({"gpu": true}) }));
    }
}
