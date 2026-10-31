//! Host capability probe: RAM, CPU, and which GPU backend (if any) Ollama can use.
//!
//! Used to pick a model the machine can actually run without an OOM crash — the
//! blueprint's constrained-hardware requirement. RAM is the *primary* signal
//! because it is always reliable; the GPU backend is per-OS detective work:
//!
//! | OS | NVIDIA | AMD | Apple | Intel / other iGPU |
//! |---|---|---|---|---|
//! | Linux | `nvidia-smi` → CUDA | sysfs `mem_info_vram_total` → ROCm | — | CPU |
//! | Windows | `nvidia-smi` → CUDA | registry `qwMemorySize` → ROCm | — | CPU |
//! | macOS | — | — | `sysctl` "Apple M…" → Metal (unified memory) | CPU |
//!
//! Each probe's parsing is a pure function over the command's text output, so
//! the logic is unit-tested here even though only Linux is on the dev machine.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// Which inference backend Ollama will actually use on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Every token computed on the CPU.
    Cpu,
    Cuda,
    Rocm,
    /// Apple Silicon: the GPU shares system RAM (unified memory).
    Metal,
}

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
    /// "NVIDIA", "AMD", "Apple", "Intel", or "none".
    pub gpu_vendor: String,
    pub gpu_name: String,
    /// Dedicated VRAM if we could read it. `None` == unknown or unified memory.
    pub vram_gb: Option<f32>,
    pub cuda_available: bool,
    /// The backend Ollama will use. Drives the memory budget the recommender
    /// works against.
    pub backend: Backend,
    /// `"linux"`, `"windows"`, `"macos"`.
    pub os: String,
    /// `"x86_64"`, `"aarch64"`.
    pub arch: String,
    /// AVX2 is what llama.cpp's fast CPU kernels are built on. Without it
    /// (Atom/Celeron/pre-2013) CPU inference is several times slower per core.
    /// Always `true` on aarch64 — NEON plays the same role there.
    pub avx2: bool,
    pub avx512: bool,
}

impl HardwareInfo {
    /// Can Ollama actually offload to a GPU here, with enough memory to matter?
    pub fn has_usable_gpu(&self) -> bool {
        self.backend != Backend::Cpu && self.gpu_budget_gb() >= 3.5
    }

    /// Memory the GPU can give a model, in GB. 0 on CPU-only hosts.
    ///
    /// Discrete cards keep ~0.8 GB for the driver and display. Apple Silicon
    /// has no separate VRAM: macOS lets the GPU wire roughly two thirds of
    /// system RAM by default, and the OS still needs some of that.
    pub fn gpu_budget_gb(&self) -> f32 {
        match self.backend {
            Backend::Cuda | Backend::Rocm => (self.vram_gb.unwrap_or(0.0) - 0.8).max(0.0),
            Backend::Metal => (self.total_ram_gb * 0.65 - 0.5).max(0.0),
            Backend::Cpu => 0.0,
        }
    }

    /// RAM to leave for the OS, the webview and Ollama itself. Windows and
    /// macOS idle noticeably heavier than a typical Linux desktop.
    pub fn os_headroom_gb(&self) -> f32 {
        match self.os.as_str() {
            "windows" | "macos" => 3.0,
            _ => 1.5,
        }
    }

    /// Memory a model may use when it runs on the CPU (or, on Apple Silicon,
    /// shares system RAM): total RAM minus the OS's share.
    pub fn cpu_budget_gb(&self) -> f32 {
        (self.total_ram_gb - self.os_headroom_gb()).max(0.0)
    }

    /// Threads to hand to a compute-heavy step, always leaving one core for the
    /// OS so the desktop stays responsive while a turn runs.
    ///
    /// Derived from **physical** cores. See [`HardwareInfo::physical_cores`].
    pub fn worker_threads(&self) -> usize {
        self.physical_cores.saturating_sub(1).max(1)
    }
}

/// Probe RAM, cores, CPU features and the GPU backend.
///
/// Not cheap: on Windows it shells out to PowerShell, so cache the result rather
/// than calling it per turn.
#[tauri::command]
pub fn detect_hardware() -> HardwareInfo {
    let (total_ram_gb, available_ram_gb) = read_ram_gb();
    let (cpu_cores, physical_cores) = read_cores();
    let gpu = detect_gpu();
    let (avx2, avx512) = cpu_features();

    let hw = HardwareInfo {
        total_ram_gb,
        available_ram_gb,
        cpu_cores,
        physical_cores,
        gpu_vendor: gpu.vendor,
        gpu_name: gpu.name,
        vram_gb: gpu.vram_gb,
        cuda_available: gpu.backend == Backend::Cuda,
        backend: gpu.backend,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        avx2,
        avx512,
    };
    tracing::info!(
        total_ram_gb = hw.total_ram_gb,
        available_ram_gb = hw.available_ram_gb,
        cpu_cores = hw.cpu_cores,
        physical_cores = hw.physical_cores,
        gpu = %hw.gpu_name,
        vram_gb = ?hw.vram_gb,
        backend = ?hw.backend,
        os = %hw.os,
        avx2 = hw.avx2,
        avx512 = hw.avx512,
        gpu_budget_gb = hw.gpu_budget_gb(),
        cpu_budget_gb = hw.cpu_budget_gb(),
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

/// Physical cores − 1 (min 1), without the GPU probe — cheap enough to call at
/// startup to seed the pipeline crate's thread budget (whisper, OCR), so every
/// compute pool in the app agrees with [`HardwareInfo::worker_threads`].
pub fn physical_worker_threads() -> usize {
    read_cores().1.saturating_sub(1).max(1)
}

/// `(avx2, avx512f)`. aarch64 has NEON, which serves the same role, so it counts
/// as `avx2 = true` for the recommender's purposes.
fn cpu_features() -> (bool, bool) {
    #[cfg(target_arch = "x86_64")]
    {
        (
            std::arch::is_x86_feature_detected!("avx2"),
            std::arch::is_x86_feature_detected!("avx512f"),
        )
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        (true, false)
    }
}

struct GpuInfo {
    vendor: String,
    name: String,
    vram_gb: Option<f32>,
    backend: Backend,
}

impl GpuInfo {
    fn none() -> Self {
        Self {
            vendor: "none".into(),
            name: "no dedicated GPU detected".into(),
            vram_gb: None,
            backend: Backend::Cpu,
        }
    }
}

/// NVIDIA first everywhere (exact VRAM), then the OS-specific probe.
fn detect_gpu() -> GpuInfo {
    if let Some((name, vram_gb)) = query_nvidia_smi() {
        return GpuInfo {
            vendor: "NVIDIA".into(),
            name,
            vram_gb,
            backend: Backend::Cuda,
        };
    }
    if cfg!(target_os = "macos") {
        return detect_apple();
    }
    if cfg!(target_os = "linux") {
        return detect_linux_amd();
    }
    if cfg!(target_os = "windows") {
        return detect_windows_gpu();
    }
    GpuInfo::none()
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

// --- macOS ----------------------------------------------------------------

fn detect_apple() -> GpuInfo {
    let brand = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    match apple_silicon_chip(&brand) {
        Some(chip) => GpuInfo {
            vendor: "Apple".into(),
            name: format!("{chip} (unified memory)"),
            vram_gb: None,
            backend: Backend::Metal,
        },
        // An Intel Mac: Ollama has no Metal path for its iGPU.
        None => GpuInfo::none(),
    }
}

/// `"Apple M2 Pro"` → `Some("Apple M2 Pro")`; an Intel brand string → `None`.
fn apple_silicon_chip(brand: &str) -> Option<String> {
    let b = brand.trim();
    b.starts_with("Apple M").then(|| b.to_string())
}

// --- Linux AMD --------------------------------------------------------------

/// AMD GPUs expose VRAM in sysfs, readable without root. APUs report a small
/// carve-out there too, so anything under 3.5 GB is treated as integrated.
fn detect_linux_amd() -> GpuInfo {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return GpuInfo::none();
    };
    let mut best: Option<u64> = None;
    for card in cards.flatten() {
        let dev = card.path().join("device");
        let vendor = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default();
        let vram = std::fs::read_to_string(dev.join("mem_info_vram_total")).unwrap_or_default();
        if let Some(bytes) = parse_amd_drm(&vendor, &vram) {
            best = best.max(Some(bytes));
        }
    }
    match best.map(|b| b as f32 / (1024.0 * 1024.0 * 1024.0)) {
        Some(gb) if gb >= 3.5 => GpuInfo {
            vendor: "AMD".into(),
            name: "AMD Radeon (ROCm)".into(),
            vram_gb: Some(gb),
            backend: Backend::Rocm,
        },
        _ => GpuInfo::none(),
    }
}

/// `vendor` file contents + `mem_info_vram_total` contents → VRAM bytes, for
/// AMD (PCI vendor 0x1002) only.
fn parse_amd_drm(vendor: &str, vram: &str) -> Option<u64> {
    (vendor.trim() == "0x1002").then(|| vram.trim().parse().ok()).flatten()
}

// --- Windows ------------------------------------------------------------

/// Every display adapter's name and **real** VRAM from the registry.
/// `Win32_VideoController.AdapterRAM` is a 32-bit field that caps at 4 GB, so
/// it would read every modern card as 4 GB; `HardwareInformation.qwMemorySize`
/// is the 64-bit value the driver writes.
fn detect_windows_gpu() -> GpuInfo {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-ItemProperty -Path 'HKLM:\\SYSTEM\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0*' \
             -ErrorAction SilentlyContinue | ForEach-Object { \"$($_.DriverDesc)|$($_.'HardwareInformation.qwMemorySize')\" }",
        ])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return GpuInfo::none(),
    };
    pick_windows_gpu(&parse_win_adapters(&text))
}

/// `"NVIDIA GeForce RTX 4060|8585740288"` lines → `(name, vram_bytes)`.
fn parse_win_adapters(text: &str) -> Vec<(String, Option<u64>)> {
    text.lines()
        .filter_map(|l| {
            let (name, mem) = l.trim().split_once('|')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), mem.trim().parse().ok()))
        })
        .collect()
}

/// The adapter with the most VRAM, mapped to the backend Ollama would use.
/// Intel and unknown adapters are CPU inference.
fn pick_windows_gpu(adapters: &[(String, Option<u64>)]) -> GpuInfo {
    let Some((name, bytes)) = adapters.iter().max_by_key(|(_, b)| b.unwrap_or(0)) else {
        return GpuInfo::none();
    };
    let vendor = classify_vendor(name);
    let vram_gb = bytes.map(|b| b as f32 / (1024.0 * 1024.0 * 1024.0));
    let backend = match vendor.as_str() {
        "NVIDIA" => Backend::Cuda,
        "AMD" if vram_gb.unwrap_or(0.0) >= 3.5 => Backend::Rocm,
        _ => Backend::Cpu,
    };
    GpuInfo {
        vendor,
        name: name.clone(),
        vram_gb,
        backend,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_silicon_is_recognised_and_intel_macs_are_not() {
        assert_eq!(apple_silicon_chip("Apple M2 Pro\n").as_deref(), Some("Apple M2 Pro"));
        assert_eq!(apple_silicon_chip("Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz"), None);
    }

    #[test]
    fn amd_sysfs_only_counts_amd() {
        assert_eq!(parse_amd_drm("0x1002\n", "8573157376\n"), Some(8_573_157_376));
        assert_eq!(parse_amd_drm("0x8086\n", "536870912\n"), None, "Intel is not AMD");
        assert_eq!(parse_amd_drm("0x1002", ""), None);
    }

    /// The bug this replaces: AdapterRAM caps at 4 GB. The registry value must
    /// come through as the real 8 GB and pick the discrete card over the iGPU.
    #[test]
    fn windows_registry_picks_the_biggest_real_vram() {
        let text = "Intel(R) UHD Graphics 630|1073741824\r\n\
                    NVIDIA GeForce RTX 4060 Laptop GPU|8585740288\r\n\
                    Microsoft Basic Display Adapter|\r\n";
        let gpu = pick_windows_gpu(&parse_win_adapters(text));
        assert_eq!(gpu.backend, Backend::Cuda);
        assert!(gpu.vram_gb.unwrap() > 7.9, "{:?}", gpu.vram_gb);
    }

    #[test]
    fn windows_intel_only_is_cpu() {
        let gpu = pick_windows_gpu(&parse_win_adapters("Intel(R) Iris(R) Xe Graphics|2147483648"));
        assert_eq!(gpu.backend, Backend::Cpu);
    }

    fn host(backend: Backend, os: &str, total: f32, vram: Option<f32>) -> HardwareInfo {
        HardwareInfo {
            total_ram_gb: total,
            available_ram_gb: total / 2.0,
            cpu_cores: 8,
            physical_cores: 4,
            gpu_vendor: "x".into(),
            gpu_name: "x".into(),
            vram_gb: vram,
            cuda_available: backend == Backend::Cuda,
            backend,
            os: os.into(),
            arch: "x86_64".into(),
            avx2: true,
            avx512: false,
        }
    }

    /// A 16 GB M1 used to read as CPU-only because only CUDA counted.
    #[test]
    fn apple_unified_memory_is_a_usable_gpu() {
        let mac = host(Backend::Metal, "macos", 16.0, None);
        assert!(mac.has_usable_gpu());
        assert!((mac.gpu_budget_gb() - 9.9).abs() < 0.01, "{}", mac.gpu_budget_gb());
    }

    #[test]
    fn budgets_respect_os_headroom() {
        assert_eq!(host(Backend::Cpu, "linux", 16.0, None).cpu_budget_gb(), 14.5);
        assert_eq!(host(Backend::Cpu, "windows", 16.0, None).cpu_budget_gb(), 13.0);
        assert_eq!(host(Backend::Cuda, "windows", 16.0, Some(8.0)).gpu_budget_gb(), 7.2);
        assert!(!host(Backend::Cuda, "linux", 16.0, Some(2.0)).has_usable_gpu());
    }
}
