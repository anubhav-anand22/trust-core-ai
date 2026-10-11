//! Tool models: the OCR and speech-to-text weights that are not Ollama models.
//!
//! `ocr_image` needs two `.rten` files and `transcribe_audio` needs a whisper
//! `.bin`. They used to be a manual download (README "Optional tool models"), so
//! on every fresh machine OCR and audio were silently off — an attached invoice
//! photo was "not read by any step" and the report said "no data". Setup now
//! fetches them during **Download & start**, with the same progress stream as the
//! Ollama pulls.
//!
//! Policy: OCR always (~12 MB; documents and photographed invoices are the common
//! case), whisper (~148 MB) only when the profile expects voice notes or is
//! unsure. A failed download is **not fatal**: the app runs without that tool, and
//! the message names the file and the folder to drop it into (the air-gapped
//! path).

use std::path::{Path, PathBuf};

use tauri::ipc::Channel;
use tauri::Manager;
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

use super::models::ModelProgress;
use super::profile;

/// File names the pipeline expects under `<app data>/models/`.
pub const WHISPER_FILE: &str = "ggml-base.en.bin";
pub const OCR_DETECTION_FILE: &str = "text-detection.rten";
pub const OCR_RECOGNITION_FILE: &str = "text-recognition.rten";

struct ToolModel {
    file: &'static str,
    url: &'static str,
    /// For the disk pre-flight before anything starts.
    approx_mb: u32,
}

const OCR: [ToolModel; 2] = [
    ToolModel {
        file: OCR_DETECTION_FILE,
        url: "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten",
        approx_mb: 3,
    },
    ToolModel {
        file: OCR_RECOGNITION_FILE,
        url: "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten",
        approx_mb: 10,
    },
];

const WHISPER: ToolModel = ToolModel {
    file: WHISPER_FILE,
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    approx_mb: 148,
};

/// `<app data>/models` — where `PipelineConfig` looks for the tool models.
pub fn models_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("models"))
}

fn present(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

/// Download whichever tool models this profile needs and doesn't have yet,
/// streaming progress like `ensure_models`. Always returns `Ok`: a tool model
/// that can't be fetched only disables that tool, and the user is told how to
/// install it by hand.
#[tauri::command]
pub async fn ensure_tool_models(
    app: tauri::AppHandle,
    on_progress: Channel<ModelProgress>,
) -> Result<(), String> {
    let dir = models_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let profile = profile::load(&app).unwrap_or_default();
    // "Not sure" about inputs keeps audio on, same as vision: don't silently
    // drop a capability the user may need.
    let want_audio = profile.inputs.is_empty() || profile.needs().needs_audio;
    let mut wanted: Vec<&ToolModel> = OCR.iter().collect();
    if want_audio {
        wanted.push(&WHISPER);
    }

    let send = |file: &str, message: String, percentage: Option<u8>| {
        let _ = on_progress.send(ModelProgress {
            model: file.to_string(),
            message,
            percentage,
        });
    };

    let missing: Vec<&ToolModel> = wanted.into_iter().filter(|m| !present(&dir.join(m.file))).collect();
    if missing.is_empty() {
        tracing::info!(want_audio, "tool models already present");
        return Ok(());
    }

    let need_mb: u32 = missing.iter().map(|m| m.approx_mb).sum();
    if let Some(free_gb) = super::models::free_disk_gb(&dir) {
        if free_gb * 1024.0 < need_mb as f32 + 200.0 {
            for m in &missing {
                send(m.file, format!("⚠ Skipped: only {free_gb:.1} GB free on this drive."), None);
            }
            tracing::warn!(free_gb, need_mb, "tool models skipped: disk too full");
            return Ok(());
        }
    }

    // Short connect timeout: setup runs on every Start, so an offline machine
    // without the files pays this per missing file on every launch.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "tool models: no HTTP client");
            return Ok(());
        }
    };

    for m in missing {
        let dest = dir.join(m.file);
        send(m.file, format!("Downloading \"{}\" (~{} MB)…", m.file, m.approx_mb), Some(0));
        match download(&client, m.url, &dest, STALL_AFTER, |pct| {
            send(m.file, format!("Downloading \"{}\"…", m.file), Some(pct))
        })
        .await
        {
            Ok(bytes) => {
                tracing::info!(file = m.file, bytes, "tool model installed");
                send(m.file, format!("✔ \"{}\" ready.", m.file), Some(100));
            }
            Err(e) => {
                tracing::warn!(file = m.file, error = %e, "tool model download failed");
                send(
                    m.file,
                    format!(
                        "⚠ Couldn't download \"{}\" ({e}). The app works without it; to enable it \
                         offline, put the file in {}.",
                        m.file,
                        dir.display()
                    ),
                    None,
                );
            }
        }
    }
    Ok(())
}

/// Give up on a download that has delivered no bytes for this long. Setup waits
/// for the tool models before opening the app, so a connection that stays open
/// but goes silent (flaky venue Wi-Fi) must not hang Start forever.
const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// Stream `url` into `<dest>.part`, then rename it into place, so an
/// interrupted download never leaves a truncated file that looks installed.
/// Fails if no data arrives for `stall_after`.
async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    stall_after: std::time::Duration,
    progress: impl Fn(u8),
) -> Result<u64, String> {
    let res = tokio::time::timeout(stall_after, client.get(url).send())
        .await
        .map_err(|_| format!("no response for {} s", stall_after.as_secs()))?
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let total = res.content_length();
    let part = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;

    // Every failure past this point removes the `.part`.
    async fn fail(part: &Path, why: String) -> Result<u64, String> {
        let _ = tokio::fs::remove_file(part).await;
        Err(why)
    }

    let mut stream = res.bytes_stream();
    let mut written: u64 = 0;
    let mut last_pct: u8 = 0;
    loop {
        let chunk = match tokio::time::timeout(stall_after, stream.next()).await {
            Err(_) => {
                drop(file);
                return fail(&part, format!("stalled: no data for {} s", stall_after.as_secs())).await;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                drop(file);
                return fail(&part, e.to_string()).await;
            }
            Ok(Some(Ok(c))) => c,
        };
        if let Err(e) = file.write_all(&chunk).await {
            drop(file);
            return fail(&part, e.to_string()).await;
        }
        written += chunk.len() as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            let pct = ((written * 100) / t).min(99) as u8;
            if pct > last_pct {
                last_pct = pct;
                progress(pct);
            }
        }
    }
    let flushed = file.flush().await;
    drop(file);
    if let Err(e) = flushed {
        return fail(&part, e.to_string()).await;
    }

    if written == 0 || total.is_some_and(|t| t != written) {
        return fail(&part, format!("incomplete download ({written} of {total:?} bytes)")).await;
    }
    tokio::fs::rename(&part, dest).await.map_err(|e| e.to_string())?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// One-shot HTTP server on 127.0.0.1: answers the first request with
    /// `content_length` in the header but only `body` on the wire, then closes.
    async fn serve(body: &'static [u8], content_length: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n");
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
        });
        format!("http://{addr}/model.rten")
    }

    #[tokio::test]
    async fn a_complete_download_lands_under_its_real_name() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(OCR_DETECTION_FILE);
        let url = serve(b"weights", 7).await;
        let n = download(&reqwest::Client::new(), &url, &dest, STALL_AFTER, |_| {}).await.unwrap();
        assert_eq!(n, 7);
        assert_eq!(std::fs::read(&dest).unwrap(), b"weights");
        assert!(!dest.with_extension("part").exists(), "no leftover .part");
    }

    /// The case `.part` exists for: a cut-off download must never leave a file
    /// under the real name, or `present()` would treat a truncated model as
    /// installed and OCR would fail on every turn.
    #[tokio::test]
    async fn a_truncated_download_leaves_nothing_that_looks_installed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(OCR_DETECTION_FILE);
        let url = serve(b"half", 1000).await;
        assert!(download(&reqwest::Client::new(), &url, &dest, STALL_AFTER, |_| {}).await.is_err());
        assert!(!present(&dest));
        assert!(!dest.with_extension("part").exists());
    }

    /// Header and a few bytes, then the socket stays open and silent: the
    /// flaky-Wi-Fi case that used to hang Start forever.
    async fn serve_then_stall() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nabc").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            drop(sock);
        });
        format!("http://{addr}/model.rten")
    }

    #[tokio::test]
    async fn a_stalled_download_gives_up_instead_of_hanging_startup() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(OCR_RECOGNITION_FILE);
        let url = serve_then_stall().await;
        let started = std::time::Instant::now();
        let err = download(
            &reqwest::Client::new(),
            &url,
            &dest,
            std::time::Duration::from_millis(300),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("stalled"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "must not wait for the server");
        assert!(!present(&dest));
        assert!(!dest.with_extension("part").exists(), "a stalled download leaves no .part");
    }

    #[test]
    fn present_needs_a_non_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        assert!(!present(&p));
        std::fs::write(&p, b"").unwrap();
        assert!(!present(&p), "an empty file is not an installed model");
        std::fs::write(&p, b"1").unwrap();
        assert!(present(&p));
    }
}
