//! Listing the drives dd can read from or write to.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum DriveKind {
    Usb,
    Sd,
    Ssd,
    Hdd,
    Virtual,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Drive {
    /// The name people know it by: /dev/sdb, /dev/disk4, \\.\PhysicalDrive2.
    pub path: String,
    /// What dd reads or writes. Differs on macOS (/dev/rdiskN is much faster).
    pub io_path: String,
    pub name: String,
    pub size: u64,
    /// "USB", "SD", "NVMe", "SATA"… (may be empty)
    pub bus: String,
    pub kind: DriveKind,
    pub removable: bool,
    /// Holds the running OS. Never offered as a target.
    pub system: bool,
    pub read_only: bool,
    pub mountpoints: Vec<String>,
    /// Volume labels and file systems, shown before erasing.
    pub volumes: Vec<String>,
    /// What the worker unmounts before writing (partitions on Linux, the disk on macOS).
    pub unmount: Vec<String>,
    /// Drive letters the worker locks before writing (Windows).
    pub lock: Vec<String>,
}

pub fn list() -> Result<Vec<Drive>, String> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let mut drives = platform::list()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let mut drives: Vec<Drive> = Vec::new();

    drives.sort_by(|a, b| {
        b.removable.cmp(&a.removable).then_with(|| natural_key(&a.path).cmp(&natural_key(&b.path)))
    });
    Ok(drives)
}

/// Mounts again what the worker unmounted before copying from `drive`, as the user.
/// (Placeholder: filled in per platform.)
pub fn remount(drive: &Drive) -> Result<(), String> {
    let _ = drive;
    Ok(())
}

/// Sorts "disk10" after "disk2".
fn natural_key(s: &str) -> (String, u64) {
    let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (head, tail) = s.split_at(s.len() - digits);
    (head.to_owned(), tail.parse().unwrap_or(0))
}

/// "SanDisk  Cruzer_Blade" → "SanDisk Cruzer Blade"
#[allow(dead_code)]
fn tidy(s: &str) -> String {
    s.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}
