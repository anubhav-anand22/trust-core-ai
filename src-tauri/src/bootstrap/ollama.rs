//! Ollama presence + installation.
//!
//! The installer itself is unchanged from the original `lib.rs` scaffold — it is
//! the one place the app reaches the network, a one-time pre-air-gap bootstrap.
//! The *detection* around it is deliberately more forgiving than a bare `PATH`
//! lookup: a GUI process (launched from Explorer, or from a dev shell through
//! several layers of `npm`/`cargo`) does not reliably inherit the `PATH` the
//! Ollama installer updated, and a false negative here sends the user through a
//! pointless reinstall they cannot escape.

use std::path::{Path, PathBuf};
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
        // The Linux user-level install (see `ensure_ollama_installed_linux`) puts
        // the binary here instead of a system location — nothing above would
        // ever find it otherwise.
        if let Some(p) = user_level_ollama_bin() {
            candidates.push(p);
        }
    }

    candidates
}

/// Where the Linux user-level install places the binary:
/// `$HOME/.local/share/trustcore-ai/ollama/bin/ollama`.
///
/// No root anywhere on this path. Because it is not a systemd unit, there is no
/// OS mechanism to start it automatically — this app starts (and, on every
/// launch, re-checks) the server itself. See `ensure_ollama_installed_linux`.
fn user_level_ollama_bin() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/trustcore-ai/ollama/bin/ollama"))
}

/// Is Ollama available to this machine?
///
/// **Output:** `true` if any of three checks pass, cheapest and most meaningful
/// first: a responding server, the binary on `PATH`, or the binary sitting in a
/// known install location.
///
/// Linux-only wrinkle: the user-level install has no systemd unit, so finding
/// the binary present but the server not responding (a reboot, a crashed
/// server) is itself the signal to restart it — otherwise this would keep
/// reporting "installed" while every real command that needs the server fails.
#[tauri::command]
pub async fn is_ollama_installed() -> bool {
    if ollama_server_responds() {
        return true;
    }

    if cfg!(target_os = "linux") {
        if let Some(bin) = user_level_ollama_bin() {
            if bin.is_file() {
                let _ = start_server_if_needed(&bin, None).await;
                return ollama_server_responds();
            }
        }
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
    // Linux gets its own path: a user-level install with no `sudo` anywhere,
    // instead of the official `install.sh` (which elevates itself — see
    // `ensure_ollama_installed_linux`'s doc comment for why that breaks here).
    // Windows/macOS are unchanged below.
    if cfg!(target_os = "linux") {
        return ensure_ollama_installed_linux(on_progress).await;
    }

    if is_ollama_installed().await {
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
    if is_ollama_installed().await {
        let _ = on_progress.send("✔ Ollama is present.".to_string());
        return Ok(true);
    }

    let _ = on_progress.send(
        "⚠️ Installation finished with errors. Restart your terminal/process to refresh PATH."
            .to_string(),
    );
    Err("Installation failed".into())
}

/// Linux install path: a plain **user-level** binary, no root, no systemd unit.
///
/// The official `install.sh` elevates itself with `sudo` to install system-wide
/// — the binary into `/usr/local/bin`, a dedicated `ollama` *system user*, and a
/// systemd service running as that user. Two problems follow from that:
///
/// 1. `sudo` needs a terminal (or an askpass helper) to show its password
///    prompt. This app, spawned as a normal desktop process, has neither, so
///    the request to elevate has nowhere to go — the install just stalls or
///    fails silently instead of ever asking for a password.
/// 2. Even if it completed, `lower_ollama_priority`'s `renice` only works on
///    processes *you* own. A systemd-managed `ollama` user's process is not
///    yours, so the one guard against the desktop losing a core to Ollama
///    silently never applies — which is how a background daemon at normal
///    priority can freeze a 2-core machine.
///
/// Installing into `$HOME` instead needs no elevation anywhere, and as a
/// side effect fixes problem 2 too: there is no separate system user, so
/// `renice` works.
///
/// Since there is no systemd unit, nothing auto-starts the server — this
/// function (and therefore this app) is responsible for starting it, and for
/// re-starting it on every later launch if it is not already running.
async fn ensure_ollama_installed_linux(on_progress: Channel<String>) -> Result<bool, String> {
    let bin = user_level_ollama_bin().ok_or_else(|| "could not resolve $HOME".to_string())?;

    if !bin.is_file() {
        let _ = on_progress.send(
            "Ollama not found. Installing for this user only — no admin password needed..."
                .to_string(),
        );

        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => {
                return Err(format!(
                    "unsupported CPU architecture `{other}` for the user-level Ollama install"
                ))
            }
        };
        let dir = bin
            .parent() // .../ollama/bin
            .and_then(Path::parent) // .../ollama
            .ok_or_else(|| "could not resolve the install directory".to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

        let tmp = std::env::temp_dir().join("trustcore-ollama.tar.zst");
        // Official release asset, same one `install.sh` fetches and the same
        // layout it unpacks (`bin/ollama` + `lib/ollama/...`) — just extracted
        // under $HOME instead of `/usr/local`, so nothing here needs a
        // privileged directory. `/releases/latest/download/...` is GitHub's
        // stable alias; it 302s to the actual versioned asset. Current
        // releases ship `.tar.zst` (zstd), not `.tar.gz` — `tar --zstd` needs a
        // tar built with libzstd support, which every mainstream distro ships.
        let script = format!(
            "curl -fsSL -L 'https://github.com/ollama/ollama/releases/latest/download/ollama-linux-{arch}.tar.zst' -o '{tmp}' && \
             tar --zstd -xf '{tmp}' -C '{dir}' && rm -f '{tmp}'",
            arch = arch,
            tmp = tmp.display(),
            dir = dir.display(),
        );

        let _ = on_progress.send("Downloading Ollama (no sudo required)...".to_string());
        let status = TokioCommand::new("sh")
            .args(["-c", &script])
            .status()
            .await
            .map_err(|e| e.to_string())?;

        if !status.success() || !bin.is_file() {
            let _ = on_progress.send("⚠️ Installation finished with errors.".to_string());
            return Err("user-level Ollama install failed".into());
        }
        let _ = on_progress.send("✔ Ollama installed for this user.".to_string());
    } else {
        let _ = on_progress.send("✔ Ollama is already installed for this user.".to_string());
    }

    start_server_if_needed(&bin, Some(&on_progress)).await
}

/// Make the binary executable (extraction does not always preserve the bit the
/// way `install.sh` would set it) and start `ollama serve` as a detached
/// background process if nothing is already listening. Polls briefly for the
/// port to come up so the caller never reports success before the server can
/// actually take requests.
///
/// The spawned process is deliberately not tracked or killed on drop — it is
/// meant to outlive this one call. With no systemd unit to restart it, this is
/// called again (cheaply — it no-ops if the port already responds) every time
/// the app starts.
async fn start_server_if_needed(
    bin: &Path,
    on_progress: Option<&Channel<String>>,
) -> Result<bool, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(bin) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            let _ = std::fs::set_permissions(bin, perms);
        }
    }

    if ollama_server_responds() {
        return Ok(true);
    }

    if let Some(ch) = on_progress {
        let _ = ch.send("Starting Ollama server...".to_string());
    }
    TokioCommand::new(bin)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start `ollama serve`: {e}"))?;

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if ollama_server_responds() {
            if let Some(ch) = on_progress {
                let _ = ch.send("✔ Ollama server is up.".to_string());
            }
            return Ok(true);
        }
    }
    Err("Ollama server did not start within 10 seconds of launching it".into())
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
