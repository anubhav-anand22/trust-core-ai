//! Host capability probe: how much RAM, and what GPU (if any).
//!
//! Used to pick a model tier the machine can actually run without an OOM crash —
//! the blueprint's constrained-hardware requirement. RAM is the *primary* signal
//! because it is always reliable; VRAM is a bonus because cross-vendor detection on
//! Windows is not.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// What we managed to learn about the host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub total_ram_gb: f32,
    /// RAM actually free right now. Total RAM only says the weights *fit*; this
    /// says whether they fit **alongside** the browser, the webview and Ollama's
    /// own working set, which is what decides whether the machine starts swapping.
    pub available_ram_gb: f32,
    /// Logical CPU cores — hyperthreads. Reported for display only.
    pub cpu_cores: usize,
    /// Physical cores, and the number that actually matters. Hyperthreads share
    /// one set of execution units, so they roughly double the *count* without
    /// doubling throughput. Sizing a thread pool off the logical count on a
    /// 4-core/8-thread laptop asks for 7 threads on 4 real cores, which is how
    /// this app twice froze the Windows shell mid-turn.
    pub physical_cores: usize,
    /// "NVIDIA", "AMD", "Intel", or "none".
    pub gpu_vendor: String,
    pub gpu_name: String,
    /// Total dedicated VRAM if we could read it. `None` == unknown, not zero.
    pub vram_gb: Option<f32>,
    pub cuda_available: bool,
}

impl HardwareInfo {
    /// Can Ollama actually offload to a GPU here?
    ///
    /// Requires CUDA *and* a VRAM reading — an integrated Intel/AMD adapter with
    /// no usable VRAM figure means CPU inference, whatever the adapter is called.
    pub fn has_usable_gpu(&self) -> bool {
        self.cuda_available && self.vram_gb.unwrap_or(0.0) >= 4.0
    }

    /// Threads to hand to a compute-heavy step, always leaving one core for the
    /// OS so the desktop stays responsive while a turn runs.
    ///
    /// Derived from **physical** cores. See [`HardwareInfo::physical_cores`].
    pub fn worker_threads(&self) -> usize {
        self.physical_cores.saturating_sub(1).max(1)
    }
}

/// Probe RAM + cores (via `sysinfo`) and the GPU (via `nvidia-smi`, then Windows CIM).
///
/// Not cheap: on a machine without NVIDIA this shells out to PowerShell, so cache
/// the result rather than calling it per turn.
#[tauri::command]
pub fn detect_hardware() -> HardwareInfo {
    let (total_ram_gb, available_ram_gb) = read_ram_gb();
    let (cpu_cores, physical_cores) = read_cores();
    let gpu = detect_gpu();

    let hw = HardwareInfo {
        total_ram_gb,
        available_ram_gb,
        cpu_cores,
        physical_cores,
        gpu_vendor: gpu.vendor,
        gpu_name: gpu.name,
        vram_gb: gpu.vram_gb,
        cuda_available: gpu.cuda,
    };
    tracing::info!(
        total_ram_gb = hw.total_ram_gb,
        available_ram_gb = hw.available_ram_gb,
        cpu_cores = hw.cpu_cores,
        physical_cores = hw.physical_cores,
        gpu = %hw.gpu_name,
        vram_gb = ?hw.vram_gb,
        cuda = hw.cuda_available,
        worker_threads = hw.worker_threads(),
        "hardware probe"
    );
    hw
}

/// `(total, available)` in GB. `sysinfo` reports bytes.
fn read_ram_gb() -> (f32, f32) {
    const BYTES_PER_GB: f32 = 1024.0 * 1024.0 * 1024.0;
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    (
        sys.total_memory() as f32 / BYTES_PER_GB,
        sys.available_memory() as f32 / BYTES_PER_GB,
    )
}

/// `(logical, physical)` core counts.
///
/// `physical_core_count()` can fail (containers, exotic platforms). When it does,
/// assume hyperthreading and halve the logical count rather than trusting it —
/// guessing high here is what starves the desktop.
fn read_cores() -> (usize, usize) {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let physical = sysinfo::System::new()
        .physical_core_count()
        .filter(|&n| n > 0)
        .unwrap_or_else(|| (logical / 2).max(1));
    (logical, physical.min(logical))
}

struct GpuInfo {
    vendor: String,
    name: String,
    vram_gb: Option<f32>,
    cuda: bool,
}

/// Try NVIDIA first (covers most ML demo machines and gives exact VRAM), then fall
/// back to a Windows WMI/CIM query for the adapter name.
fn detect_gpu() -> GpuInfo {
    if let Some((name, vram_gb)) = query_nvidia_smi() {
        return GpuInfo {
            vendor: "NVIDIA".into(),
            name,
            vram_gb,
            cuda: true,
        };
    }

    #[cfg(target_os = "windows")]
    if let Some(name) = query_windows_gpu_name() {
        let vendor = classify_vendor(&name);
        return GpuInfo {
            vendor,
            name,
            vram_gb: None,
            cuda: false,
        };
    }

    GpuInfo {
        vendor: "none".into(),
        name: "no dedicated GPU detected".into(),
        vram_gb: None,
        cuda: false,
    }
}

/// `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`
/// → `("NVIDIA GeForce RTX 4060", Some(8.0))`
fn query_nvidia_smi() -> Option<(String, Option<f32>)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let first = line.lines().next()?.trim();
    let mut parts = first.split(',');
    let name = parts.next()?.trim().to_string();
    let vram_gb = parts
        .next()
        .and_then(|m| m.trim().parse::<f32>().ok())
        .map(|mib| mib / 1024.0);
    Some((name, vram_gb))
}

/// PowerShell CIM query for the primary video controller's name. `AdapterRAM` is
/// deliberately not read: it is a 32-bit field that caps at 4 GB and lies for
/// bigger cards, so it would do more harm than good.
#[cfg(target_os = "windows")]
fn query_windows_gpu_name() -> Option<String> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_VideoController | Select-Object -First 1 -ExpandProperty Name)",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "windows")]
fn classify_vendor(name: &str) -> String {
    let n = name.to_lowercase();
    if n.contains("nvidia") || n.contains("geforce") || n.contains("quadro") || n.contains("rtx") {
        "NVIDIA".into()
    } else if n.contains("amd") || n.contains("radeon") {
        "AMD".into()
    } else if n.contains("intel") || n.contains("arc") {
        "Intel".into()
    } else {
        "unknown".into()
    }
}
