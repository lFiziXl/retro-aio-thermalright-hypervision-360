//! Hardware telemetry module for the Pop!_OS AIO
//! (Ryzen 9 9950X3D + Radeon 9070 XT, 32 GiB RAM).
//!
//! RAM usage is read via the `sysinfo` crate. CPU/GPU temperatures are read
//! directly from the kernel hwmon interface (`/sys/class/hwmon/hwmonN/...`),
//! because `sysinfo` does not reliably map the `k10temp` (CPU) and `amdgpu`
//! (GPU) sensor files.
//!
//! Temperature convention: `f32::NAN` means "no sensor value could be read"
//! (driver not loaded, file missing, or value implausible).

use std::fs;
use std::path::{Path, PathBuf};

/// Snapshot of the key hardware metrics for the AIO.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemStats {
    /// CPU core temperature in °C; `f32::NAN` when unreadable.
    pub cpu_temp: f32,
    /// GPU core temperature in °C; `f32::NAN` when unreadable.
    pub gpu_temp: f32,
    /// Average CPU core frequency in MHz; `0.0` when unreadable.
    pub cpu_freq_mhz: f32,
    /// AMD GPU VRAM in use, in GiB; `0.0` when the sysfs counter is missing.
    pub gpu_vram_used_gb: f32,
    /// GPU busy percentage, 0–100; the maximum of the AMD sysfs reading
    /// (`card0` / `card1`) and the NVIDIA `nvidia-smi` reading; `0.0` when
    /// neither source is available.
    pub gpu_busy_percent: f32,
    /// RAM currently in use, in GiB.
    pub ram_used_gb: f32,
    /// Total physical RAM, in GiB.
    pub ram_total_gb: f32,
}

/// Driver names, in priority order, considered to be the CPU sensor.
/// `k10temp` is the standard driver for AMD Ryzen cores on Linux.
const CPU_DRIVERS: &[&str] = &["k10temp", "cpu_thermal", "coretemp"];

/// Driver names, in priority order, considered to be the AMD GPU sensor.
const GPU_DRIVERS: &[&str] = &["amdgpu", "radeon"];

const HWMON_ROOT: &str = "/sys/class/hwmon";

/// amdgpu exposes VRAM usage in sysfs; try `card0`, then `card1`.
const VRAM_SYSFS_CANDIDATES: &[&str] = &[
    "/sys/class/drm/card0/device/mem_info_vram_used",
    "/sys/class/drm/card1/device/mem_info_vram_used",
];

/// amdgpu exposes GPU utilization in sysfs; try `card0`, then `card1`.
const GPU_BUSY_SYSFS_CANDIDATES: &[&str] = &[
    "/sys/class/drm/card0/device/gpu_busy_percent",
    "/sys/class/drm/card1/device/gpu_busy_percent",
];

/// Read an unsigned integer from a sysfs attribute file.
fn read_sysfs_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// List hwmon device directories in a stable (sorted) order.
fn hwmon_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(HWMON_ROOT)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// Return the driver name of a hwmon device (contents of its `name` file).
fn hwmon_driver(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("name")).ok().map(|s| s.trim().to_string())
}

/// Read the first plausible temperature of a hwmon device, in °C.
///
/// Prefers `temp1_input`, then falls back to any other `tempN_input` file.
/// Zeroed (uninitialized) or physically implausible readings are skipped.
fn read_hwmon_temp_celsius(dir: &Path) -> Option<f32> {
    let mut sensors: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            // NB: not `?` here — that would abort the whole function on the
            // first non-sensor file (e.g. `uevent`); skip it instead.
            let Some(index) = file_name
                .strip_prefix("temp")
                .and_then(|rest| rest.strip_suffix("_input"))
                .and_then(|idx| idx.parse::<u32>().ok())
            else {
                continue;
            };
            sensors.push((index, entry.path()));
        }
    }
    // `temp1` first, then remaining sensors by index.
    sensors.sort_by_key(|(index, _)| *index);

    for (_, path) in sensors {
        if let Some(milli) = read_sysfs_u64(&path) {
            let celsius = milli as f32 / 1000.0;
            if (1.0..125.0).contains(&celsius) {
                return Some(celsius);
            }
        }
    }
    None
}

/// Find the first hwmon sensor, in driver priority order, that reports a
/// plausible temperature.
fn read_sensor_temperature(driver_names: &[&str]) -> Option<f32> {
    let dirs = hwmon_dirs();
    for name in driver_names {
        for dir in &dirs {
            if hwmon_driver(dir).as_deref() == Some(name) {
                if let Some(temp) = read_hwmon_temp_celsius(dir) {
                    return Some(temp);
                }
            }
        }
    }
    None
}

/// Read amdgpu's VRAM usage from sysfs, in GiB.
///
/// Tries `card0` then `card1`; returns `0.0` if neither counter exists
/// (non-amdgpu GPU, driver not loaded, or file unreadable).
fn read_amdgpu_vram_used_gib() -> f32 {
    for path in VRAM_SYSFS_CANDIDATES {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(bytes) = raw.trim().parse::<f32>() {
                return bytes / 1024.0_f32.powi(3);
            }
        }
    }
    0.0
}

/// Read amdgpu's GPU busy percentage (0–100) from sysfs.
///
/// Tries `card0` then `card1` and returns the **maximum** valid reading —
/// the machine has an iGPU plus a discrete GPU, so we want the load of the
/// active one. Returns `0.0` if neither counter can be read (non-amdgpu
/// GPU, driver not loaded, or file unreadable).
fn read_amdgpu_busy_percent() -> f32 {
    let mut max_busy = 0.0f32;
    for path in GPU_BUSY_SYSFS_CANDIDATES {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(percent) = raw.trim().parse::<f32>() {
                max_busy = max_busy.max(percent);
            }
        }
    }
    max_busy
}

/// Read the NVIDIA GPU utilization (0–100) via `nvidia-smi`.
///
/// NVIDIA drivers do not expose a `gpu_busy_percent` sysfs attribute the way
/// `amdgpu` does, so this shells out to `nvidia-smi --query-gpu=utilization.gpu`.
/// Returns `0.0` when `nvidia-smi` is missing, errors, or prints nothing
/// parseable (i.e. no NVIDIA GPU in the system).
fn read_nvidia_gpu_busy() -> f32 {
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args(&["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
    {
        if let Ok(s) = String::from_utf8(output.stdout) {
            if let Ok(val) = s.trim().parse::<f32>() {
                return val;
            }
        }
    }
    0.0
}

/// Collect a full snapshot: CPU/GPU temperatures, CPU frequency, VRAM usage,
/// GPU utilization and RAM usage.
///
/// RAM and CPU frequency come from `sysinfo`; temperatures come from the
/// kernel hwmon interface, matched by driver name (`k10temp` for the CPU,
/// `amdgpu` for the GPU) rather than by the arbitrary `hwmonN` index; VRAM
/// usage and GPU utilization come from the amdgpu sysfs counters.
///
/// `sys` is a caller-owned, cached [`sysinfo::System`]: this function only
/// refreshes its CPU (usage + frequency) and memory counters instead of
/// rebuilding the whole object (which scans every process) — the caller owns
/// the cadence, so the render loop can throttle updates to once per second.
pub fn get_stats(sys: &mut sysinfo::System) -> SystemStats {
    let cpu_temp = read_sensor_temperature(CPU_DRIVERS).unwrap_or(f32::NAN);
    let gpu_temp = read_sensor_temperature(GPU_DRIVERS).unwrap_or(f32::NAN);
    let gpu_vram_used_gb = read_amdgpu_vram_used_gib();
    let amd = read_amdgpu_busy_percent();
    let nvidia = read_nvidia_gpu_busy();
    let gpu_busy_percent = amd.max(nvidia);

    // `refresh_cpu_all()` = `CpuRefreshKind::everything()` — in sysinfo 0.39
    // that is exactly { cpu_usage, frequency }; the separate
    // `refresh_cpu_usage()` call skips frequency, which left `cpu_freq_mhz`
    // stuck at 0 / stale.
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let gib: f32 = 1024.0 * 1024.0 * 1024.0;

    // Average frequency across all cores (sysinfo reports MHz per core).
    let cpus = sys.cpus();
    let cpu_freq_mhz: f32 = if cpus.is_empty() {
        0.0
    } else {
        (cpus.iter().map(|c| c.frequency()).sum::<u64>() / cpus.len() as u64) as f32
    };

    SystemStats {
        cpu_temp,
        gpu_temp,
        cpu_freq_mhz,
        gpu_vram_used_gb,
        gpu_busy_percent,
        ram_used_gb: sys.used_memory() as f32 / gib,
        ram_total_gb: sys.total_memory() as f32 / gib,
    }
}
