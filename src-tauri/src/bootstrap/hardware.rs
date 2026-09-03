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
    /// "NVIDIA", "AMD", "Intel", or "none".
    pub gpu_vendor: String,
    pub gpu_name: String,
    /// Total dedicated VRAM if we could read it. `None` == unknown, not zero.
    pub vram_gb: Option<f32>,
    pub cuda_available: bool,
}

/// Probe RAM (via `sysinfo`) and the GPU (via `nvidia-smi`, then Windows CIM).
#[tauri::command]
pub fn detect_hardware() -> HardwareInfo {
    let total_ram_gb = read_total_ram_gb();
    let gpu = detect_gpu();

    HardwareInfo {
        total_ram_gb,
        gpu_vendor: gpu.vendor,
        gpu_name: gpu.name,
        vram_gb: gpu.vram_gb,
        cuda_available: gpu.cuda,
    }
}

fn read_total_ram_gb() -> f32 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    // sysinfo reports bytes.
    sys.total_memory() as f32 / (1024.0 * 1024.0 * 1024.0)
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
