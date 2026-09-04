//! `transcribe_audio` — offline speech-to-text via whisper.cpp (`whisper-rs`).
//!
//! `symphonia` decodes any common container → we down-mix to mono and linearly
//! resample to 16 kHz → whisper transcribes → the model is dropped immediately so
//! it does not linger in RAM (blueprint's "unloads from active memory" rule).

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::engine::schemas::{FileKind, TaskStep, ToolResult};
use crate::tools::{Tool, ToolContext};
use crate::{CoreError, Result};

/// `transcribe_audio` tool.
pub struct AudioTool;

#[async_trait::async_trait]
impl Tool for AudioTool {
    fn name(&self) -> &'static str {
        "transcribe_audio"
    }

    async fn run(&self, step: &TaskStep, ctx: &ToolContext<'_>) -> Result<ToolResult> {
        let started = std::time::Instant::now();

        let file = match ctx.first_file_of(FileKind::Audio) {
            Some(f) => f,
            None => {
                return Ok(ToolResult::failure(
                    &step.id,
                    self.name(),
                    "no audio file attached",
                    started.elapsed().as_millis(),
                ))
            }
        };

        let model = ctx.config.whisper_model_path.clone();
        if !model.is_file() {
            return Ok(ToolResult::failure(
                &step.id,
                self.name(),
                format!(
                    "whisper model not installed (expected {})",
                    model.display()
                ),
                started.elapsed().as_millis(),
            ));
        }

        let path = file.path.clone();
        let outcome = tokio::task::spawn_blocking(move || transcribe(&model, &path))
            .await
            .map_err(|e| CoreError::Audio(e.to_string()))?;

        match outcome {
            Ok(text) => {
                let empty = text.trim().is_empty();
                Ok(ToolResult {
                    step_id: step.id.clone(),
                    task: self.name().into(),
                    ok: true,
                    data: serde_json::json!({ "text": text, "source": file.original_name }),
                    error: None,
                    warning: empty.then(|| "no speech detected".to_string()),
                    elapsed_ms: started.elapsed().as_millis(),
                })
            }
            Err(e) => Ok(ToolResult::failure(
                &step.id,
                self.name(),
                e.to_string(),
                started.elapsed().as_millis(),
            )),
        }
    }
}

/// Decode → resample → whisper. Model + state are dropped on return, freeing RAM.
fn transcribe(model_path: &Path, audio_path: &str) -> Result<String> {
    let samples = decode_to_mono_16k(audio_path)?;
    if samples.is_empty() {
        return Ok(String::new());
    }

    let ctx = WhisperContext::new_with_params(
        model_path,
        WhisperContextParameters::default(),
    )
    .map_err(|e| CoreError::Audio(format!("load whisper model: {e}")))?;

    let mut state = ctx
        .create_state()
        .map_err(|e| CoreError::Audio(e.to_string()))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_translate(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    // Leave a core for the OS. whisper.cpp otherwise takes every core it can see,
    // which on a 4-core CPU-only host freezes the desktop for the whole step.
    params.set_n_threads(crate::worker_threads() as i32);

    state
        .full(params, &samples)
        .map_err(|e| CoreError::Audio(e.to_string()))?;

    let mut text = String::new();
    for segment in state.as_iter() {
        if let Ok(chunk) = segment.to_str_lossy() {
            let chunk = chunk.trim();
            if !chunk.is_empty() {
                text.push_str(chunk);
                text.push(' ');
            }
        }
    }
    Ok(text.trim().to_string())
}

/// Decode `path` to mono f32 PCM at 16 kHz (whisper's required input format).
fn decode_to_mono_16k(path: &str) -> Result<Vec<f32>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| CoreError::Audio(e.to_string()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| CoreError::Audio(format!("probe format: {e}")))?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| CoreError::Audio("file has no audio track".into()))?
        .clone();
    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(16_000);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count().max(1))
        .unwrap_or(1);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| CoreError::Audio(format!("make decoder: {e}")))?;

    let mut mono: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else {
            continue;
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks(channels) {
            mono.push(frame.iter().copied().sum::<f32>() / frame.len() as f32);
        }
    }

    Ok(resample_linear(&mono, src_rate, 16_000))
}

/// Linear-interpolation resampler. Not audiophile quality, but speech into a
/// quantised whisper model does not need more, and it keeps a dependency out.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == to {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}
