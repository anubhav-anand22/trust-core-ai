//! Ollama presence + installation.
//!
//! The installer itself is unchanged from the original `lib.rs` scaffold — it is
//! the one place the app reaches the network, a one-time pre-air-gap bootstrap.
//! The *detection* around it is deliberately more forgiving than a bare `PATH`
//! lookup: a GUI process (launched from Explorer, or from a dev shell through
//! several layers of `npm`/`cargo`) does not reliably inherit the `PATH` the
//! Ollama installer updated, and a false negative here sends the user through a
//! pointless reinstall they cannot escape.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tauri::ipc::Channel;
use tokio::process::Command as TokioCommand;

/// Default Ollama server address. Fixed — this is an offline desktop app.
const OLLAMA_ADDR: &str = "127.0.0.1:11434";

/// Is a local Ollama server already answering?
///
/// The strongest signal available: if the port accepts a connection, Ollama is
/// both installed *and* running, whatever `PATH` happens to say.
fn ollama_server_responds() -> bool {
    use std::net::{SocketAddr, TcpStream};

    let Ok(addr) = OLLAMA_ADDR.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Where Ollama's own installers place the binary, per OS.
///
/// Consulted only when the `PATH` lookup fails, to catch the "installed, but this
/// process cannot see it" case.
fn known_ollama_binaries() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join("Programs").join("Ollama").join("ollama.exe"));
        }
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join("Ollama").join("ollama.exe"));
        }
    } else {
        for path in [
            "/usr/local/bin/ollama",
            "/usr/bin/ollama",
            "/opt/homebrew/bin/ollama",
            "/Applications/Ollama.app/Contents/Resources/ollama",
        ] {
            candidates.push(PathBuf::from(path));
        }
    }

    candidates
}

/// Is Ollama available to this machine?
///
/// **Output:** `true` if any of three checks pass, cheapest and most meaningful
/// first: a responding server, the binary on `PATH`, or the binary sitting in a
/// known install location.
#[tauri::command]
pub fn is_ollama_installed() -> bool {
    if ollama_server_responds() {
        return true;
    }

    let (shell, shell_arg, check_cmd) = if cfg!(target_os = "windows") {
        ("cmd", "/C", "where ollama")
    } else {
        ("sh", "-c", "command -v ollama")
    };
    if let Ok(output) = Command::new(shell).args([shell_arg, check_cmd]).output() {
        if output.status.success() {
            return true;
        }
    }

    known_ollama_binaries().iter().any(|path| path.is_file())
}

/// Install Ollama for the current OS if it is missing, streaming progress lines
/// back to the front-end over `on_progress`.
///
/// **Output:** `Ok(true)` when Ollama is present (already, or after a successful
/// install); `Err` only when it is still missing afterwards.
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
        return Ok(true);
    }

    // A non-zero exit is not proof of failure: `winget install` reports non-zero
    // for benign outcomes such as "no applicable upgrade found" when the package
    // is already present. Re-check the machine and trust that over the exit code.
    if is_ollama_installed() {
        let _ = on_progress.send("✔ Ollama is present.".to_string());
        return Ok(true);
    }

    let _ = on_progress.send(
        "⚠️ Installation finished with errors. Restart your terminal/process to refresh PATH."
            .to_string(),
    );
    Err("Installation failed".into())
}

/// Drop every running Ollama process to below-normal scheduling priority.
///
/// Ollama is a *separate* process from this app, so the pipeline's `num_thread`
/// cap limits how many CPU workers it spawns but not how the OS schedules them.
/// On a CPU-only machine a full-core inference run at normal priority competes
/// with the window manager, and the desktop stops responding for the length of a
/// turn — the failure this app hit twice on a 4-core laptop. Below-normal keeps
/// inference fast while guaranteeing the shell always wins a contested core.
///
/// Best-effort and idempotent: model *runner* subprocesses are spawned lazily on
/// the first inference, so this is called again after warm-up. Returns how many
/// processes were adjusted, for the log.
pub fn lower_ollama_priority() -> usize {
    let output = if cfg!(target_os = "windows") {
        // `ollama*` covers the server (`ollama.exe`), the tray app
        // (`ollama app.exe`) and the per-model runner, whose exact name has
        // changed across releases.
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "$p = Get-Process | Where-Object { $_.Name -like 'ollama*' }; \
                 $n = 0; \
                 foreach ($proc in $p) { \
                   try { $proc.PriorityClass = 'BelowNormal'; $n++ } catch {} \
                 }; \
                 Write-Output $n",
            ])
            .output()
    } else {
        // `renice` to +5; `pgrep -f` catches the runner even when it is argv[0]
        // `ollama` invoked as a subcommand.
        Command::new("sh")
            .args([
                "-c",
                "pids=$(pgrep -f '[o]llama'); [ -n \"$pids\" ] && renice -n 5 $pids >/dev/null 2>&1; \
                 echo $pids | wc -w",
            ])
            .output()
    };

    let count = output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<usize>().ok())
        .unwrap_or(0);

    if count > 0 {
        tracing::info!(count, "lowered Ollama process priority to below-normal");
    } else {
        tracing::debug!("no Ollama processes found to deprioritise (yet)");
    }
    count
}
