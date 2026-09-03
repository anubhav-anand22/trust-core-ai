//! Ollama presence + installation.
//!
//! Moved verbatim from the original `lib.rs` scaffold — this logic already worked
//! and is not part of the pipeline rewrite. It is the one place the app reaches the
//! network: a one-time, pre-air-gap bootstrap.

use std::process::Command;
use tauri::ipc::Channel;
use tokio::process::Command as TokioCommand;

/// Is the `ollama` binary on `PATH`?
///
/// **Output:** `true` if `where ollama` / `command -v ollama` succeeds.
#[tauri::command]
pub fn is_ollama_installed() -> bool {
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

/// Install Ollama for the current OS if it is missing, streaming progress lines
/// back to the front-end over `on_progress`.
///
/// **Output:** `Ok(true)` when Ollama is present (already, or after a successful
/// install); `Err` if the install script exited non-zero.
#[tauri::command]
pub async fn ensure_ollama_installed(on_progress: Channel<String>) -> Result<bool, String> {
    if is_ollama_installed() {
        let _ = on_progress.send("✔ Ollama is already installed.".to_string());
        return Ok(true);
    }

    let _ = on_progress.send("Ollama not found. Starting installation...".to_string());

    let (shell, arg, script) = if cfg!(target_os = "macos") {
        (
            "sh",
            "-c",
            "brew install --cask ollama || (curl -fsSL https://ollama.com/download/Ollama-darwin.zip -o /tmp/Ollama-darwin.zip && unzip -qo /tmp/Ollama-darwin.zip -d /Applications && rm /tmp/Ollama-darwin.zip)",
        )
    } else if cfg!(target_os = "linux") {
        ("sh", "-c", "curl -fsSL https://ollama.com/install.sh | sh")
    } else if cfg!(target_os = "windows") {
        let ps_script = "try { winget install -e --id Ollama.Ollama --accept-source-agreements --accept-package-agreements } catch { $installerPath = \"$env:TEMP\\OllamaSetup.exe\"; Invoke-WebRequest -Uri \"https://ollama.com/download/OllamaSetup.exe\" -OutFile $installerPath; Start-Process -FilePath $installerPath -Args \"/silent\" -Wait; Remove-Item $installerPath; }";
        ("powershell", "-Command", ps_script)
    } else {
        return Err("Unsupported OS".into());
    };

    let _ = on_progress.send(format!(
        "Running installation script for {}...",
        std::env::consts::OS
    ));

    let status = TokioCommand::new(shell)
        .args([arg, script])
        .status()
        .await
        .map_err(|e| e.to_string())?;

    if status.success() {
        let _ = on_progress.send("✔ Ollama successfully installed!".to_string());
        Ok(true)
    } else {
        let _ = on_progress.send(
            "⚠️ Installation finished with errors. Restart your terminal/process to refresh PATH."
                .to_string(),
        );
        Err("Installation failed".into())
    }
}
