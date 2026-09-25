use super::{Drive, DriveKind, tidy};
use serde::{Deserialize, Deserializer};
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$parts = @(Get-Partition -ErrorAction SilentlyContinue | Where-Object { $_.DriveLetter } |
  ForEach-Object { [pscustomobject]@{ Disk = [int]$_.DiskNumber; Letter = "$($_.DriveLetter)" } })
$labels = @{}
foreach ($v in @(Get-Volume -ErrorAction SilentlyContinue)) {
  if ($v.DriveLetter) { $labels["$($v.DriveLetter)"] = "$($v.FileSystemLabel) ($($v.FileSystem))" }
}
$disks = @(Get-Disk | ForEach-Object {
  $n = [int]$_.Number
  $letters = @($parts | Where-Object { $_.Disk -eq $n } | ForEach-Object { $_.Letter })
  [pscustomobject]@{
    Number = $n; Name = "$($_.FriendlyName)"; Size = [uint64]$_.Size; Bus = "$($_.BusType)"
    IsBoot = [bool]$_.IsBoot; IsSystem = [bool]$_.IsSystem; ReadOnly = [bool]$_.IsReadOnly
    Letters = $letters; Labels = @($letters | ForEach-Object { $labels[$_] } | Where-Object { $_ })
  }
})
ConvertTo-Json -InputObject $disks -Depth 3 -Compress
"#;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WinDisk {
    number: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    bus: String,
    #[serde(default)]
    is_boot: bool,
    #[serde(default)]
    is_system: bool,
    #[serde(default)]
    read_only: bool,
    #[serde(default, deserialize_with = "one_or_many")]
    letters: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    labels: Vec<String>,
}

/// PowerShell 5 sometimes turns one-element arrays into plain values.
fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
        Nothing(()),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
        OneOrMany::Nothing(()) => Vec::new(),
    })
}

pub fn list() -> Result<Vec<Drive>, String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("couldn't run PowerShell: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let disks: Vec<WinDisk> =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("couldn't read the disk list: {e}"))?;
    Ok(disks.into_iter().filter(|d| d.size > 0).map(drive).collect())
}

fn drive(d: WinDisk) -> Drive {
    let (kind, removable) = match d.bus.as_str() {
        "USB" => (DriveKind::Usb, true),
        "SD" | "MMC" => (DriveKind::Sd, true),
        "NVMe" => (DriveKind::Ssd, false),
        "File Backed Virtual" | "Virtual" => (DriveKind::Virtual, false),
        _ => (DriveKind::Hdd, false),
    };
    let path = format!(r"\\.\PhysicalDrive{}", d.number);
    Drive {
        io_path: path.clone(),
        path,
        name: tidy(&d.name),
        size: d.size,
        bus: d.bus,
        kind,
        removable,
        system: d.is_boot || d.is_system,
        read_only: d.read_only,
        mountpoints: d.letters.iter().map(|l| format!("{l}:\\")).collect(),
        volumes: d.labels,
        unmount: Vec::new(),
        lock: d.letters.iter().map(|l| format!("{l}:")).collect(),
    }
}
