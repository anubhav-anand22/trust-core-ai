use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::Ollama;
use std::process::Command;
use tauri::ipc::Channel;
use tauri::State;
use tokio::process::Command as TokioCommand;

// Managed state for Tauri
pub struct AppState {
    pub ollama: Ollama,
}

#[tauri::command]
fn is_ollama_installed() -> bool {
    let (shell, shell_arg, check_cmd) = if cfg!(target_os = "windows") {
        ("cmd", "/C", "where ollama")
    } else {
        ("sh", "-c", "command -v ollama")
    };

    match Command::new(shell).args([shell_arg, check_cmd]).output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

#[tauri::command]
async fn ensure_ollama_installed(on_progress: Channel<String>) -> Result<bool, String> {
    if is_ollama_installed() {
        let _ = on_progress.send("✔ Ollama is already installed.".to_string());
        return Ok(true);
    }

    let _ = on_progress.send("Ollama not found. Starting installation...".to_string());

    let (shell, arg, script) = if cfg!(target_os = "macos") {
        (
            "sh",
            "-c",
            "brew install --cask ollama || (curl -fsSL https://ollama.com/download/Ollama-darwin.zip -o /tmp/Ollama-darwin.zip && unzip -qo /tmp/Ollama-darwin.zip -d /Applications && rm /tmp/Ollama-darwin.zip)"
        )
    } else if cfg!(target_os = "linux") {
        (
            "sh",
            "-c",
            "curl -fsSL https://ollama.com/install.sh | sh"
        )
    } else if cfg!(target_os = "windows") {
        let ps_script = "try { winget install -e --id Ollama.Ollama --accept-source-agreements --accept-package-agreements } catch { $installerPath = \"$env:TEMP\\OllamaSetup.exe\"; Invoke-WebRequest -Uri \"https://ollama.com/download/OllamaSetup.exe\" -OutFile $installerPath; Start-Process -FilePath $installerPath -Args \"/silent\" -Wait; Remove-Item $installerPath; }";
        (
            "powershell",
            "-Command",
            ps_script
        )
    } else {
        return Err("Unsupported OS".into());
    };

    let _ = on_progress.send(format!("Running installation script for {}...", std::env::consts::OS));

    let status = TokioCommand::new(shell)
        .args([arg, script])
        .status()
        .await
        .map_err(|e| e.to_string())?;

    if status.success() {
        let _ = on_progress.send("✔ Ollama successfully installed!".to_string());
        Ok(true)
    } else {
        let _ = on_progress.send("⚠️ Installation finished with errors. Restart your terminal/process to refresh PATH.".to_string());
        Err("Installation failed".into())
    }
}

#[tauri::command]
async fn generate_response(prompt: String, state: State<'_, AppState>) -> Result<String, String> {
    // Keep model in memory indefinitely (-1) or for a long duration (e.g. "60m")
    let request = GenerationRequest::new("llama3.2:3b".to_string(), prompt)
        .keep_alive(ollama_rs::generation::parameters::KeepAlive::Indefinitely);

    let res = state.ollama.generate(request).await;
    match res {
        Ok(res) => Ok(res.response),
        Err(e) => Err(e.to_string()),
    }
}

const REQUIRED_OLLAMA_MODELS: &[&str] = &["qwen3:4b", "llama3.2:3b"];

#[derive(Clone, serde::Serialize)]
struct ModelProgress {
    model: String,
    message: String,
    percentage: Option<u8>,
}

#[tauri::command]
async fn ensure_required_models(
    on_progress: Channel<ModelProgress>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    for model_name in REQUIRED_OLLAMA_MODELS {
        let target = if model_name.contains(':') {
            model_name.to_string()
        } else {
            format!("{}:latest", model_name)
        };

        let models = state.ollama.list_local_models().await.map_err(|e| e.to_string())?;
        let is_installed = models.iter().any(|m| m.name == target || m.name == *model_name);

        if is_installed {
            let _ = on_progress.send(ModelProgress {
                model: model_name.to_string(),
                message: format!("✔ Model \"{}\" is already installed.", model_name),
                percentage: Some(100),
            });
            continue;
        }

        let _ = on_progress.send(ModelProgress {
            model: model_name.to_string(),
            message: format!("Model \"{}\" not found. Starting download...", model_name),
            percentage: Some(0),
        });

        use tokio_stream::StreamExt;
        let mut stream = state
            .ollama
            .pull_model_stream(model_name.to_string(), false)
            .await
            .map_err(|e| e.to_string())?;

        while let Some(res) = stream.next().await {
            match res {
                Ok(progress) => {
                    let percentage = if let (Some(total), Some(completed)) = (progress.total, progress.completed) {
                        if total > 0 {
                            Some(((completed as f64 / total as f64) * 100.0).round() as u8)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let _ = on_progress.send(ModelProgress {
                        model: model_name.to_string(),
                        message: progress.message,
                        percentage,
                    });
                }
                Err(e) => {
                    return Err(format!("Error downloading {}: {}", model_name, e));
                }
            }
        }

        let _ = on_progress.send(ModelProgress {
            model: model_name.to_string(),
            message: format!("✔ Successfully downloaded \"{}\".", model_name),
            percentage: Some(100),
        });
    }

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            ollama: Ollama::default(),
        })
        .setup(|app| {
            // Warm up model in the background on startup so the first user prompt is instant
            tauri::async_runtime::spawn(async {
                let client = Ollama::default();
                let warmup_req = GenerationRequest::new("llama3.2:3b".to_string(), "".to_string())
                    .keep_alive(ollama_rs::generation::parameters::KeepAlive::Indefinitely);
                let _ = client.generate(warmup_req).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            generate_response,
            is_ollama_installed,
            ensure_ollama_installed,
            ensure_required_models
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
