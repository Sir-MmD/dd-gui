//! Windows: PowerShell's Storage cmdlets (Get-Disk, Get-Partition, Get-Volume) list the drives.
//!
//! The script's output is JSON made of plain ASCII (other characters as `\uXXXX`), so
//! the console's code page can't garble names and labels.

#![cfg_attr(not(windows), allow(dead_code))]

use super::{Drive, DriveKind, flag, items, text, tidy};
use serde_json::Value;
#[cfg(windows)]
use std::{os::windows::process::CommandExt, process::Command, time::Duration};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Per disk: what it is, whether it runs Windows (it holds the Windows, boot or system
/// partition, or a page file), and every volume on it, with or without a drive letter.
/// Volumes without one still get locked before writing: Windows refuses raw writes over
/// any mounted file system, and Get-Volume itself mounts them all. Dynamic disks (LDM)
/// are told apart: their volumes can't be found (or locked) this way.
const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$WarningPreference = 'SilentlyContinue'
try {
  $volumes = @{}
  foreach ($v in @(Get-Volume -ErrorAction SilentlyContinue)) {
    if ($v.Path) { $volumes[[string]$v.Path] = $v }
  }
  $systemLetters = @{}
  $windowsDrive = [string]$env:SystemDrive
  if ($windowsDrive -match '^[A-Za-z]:') { $systemLetters[$windowsDrive.Substring(0, 1).ToUpper()] = $true }
  foreach ($f in @(Get-CimInstance -ClassName Win32_PageFileUsage -ErrorAction SilentlyContinue)) {
    $name = [string]$f.Name
    if ($name -match '^[A-Za-z]:') { $systemLetters[$name.Substring(0, 1).ToUpper()] = $true }
  }
  $external = @{}
  foreach ($d in @(Get-CimInstance -ClassName Win32_DiskDrive -ErrorAction SilentlyContinue)) {
    if ([string]$d.MediaType -match 'External|Removable') { $external[[string]$d.Index] = $true }
  }
  $partitions = @(Get-Partition -ErrorAction SilentlyContinue)
  $disks = @(foreach ($d in @(Get-Disk)) {
    if ($null -eq $d.Number) { continue }
    $n = [int]$d.Number
    $system = [bool]$d.IsBoot -or [bool]$d.IsSystem
    $dynamic = $false
    $found = @(foreach ($p in @($partitions | Where-Object { $_.DiskNumber -eq $n })) {
      if ([bool]$p.IsBoot -or [bool]$p.IsSystem) { $system = $true }
      if ($p.MbrType -eq 0x42 -or [string]$p.GptType -in '{5808c8aa-7e8f-42e0-85d2-e1e90434cfb3}', '{af9b60a0-1431-4f62-bc68-3311714a69ad}') {
        $dynamic = $true
      }
      $letter = [string]$p.DriveLetter
      if ($letter -notmatch '^[A-Za-z]$') { $letter = '' }
      if ($letter -and $systemLetters.ContainsKey($letter.ToUpper())) { $system = $true }
      $guid = ''
      $folders = @()
      foreach ($a in @($p.AccessPaths)) {
        $a = [string]$a
        if ($a -like '\\?\Volume{*') { $guid = $a }
        elseif ($a -and $a -notmatch '^[A-Za-z]:\\?$') { $folders += $a }
      }
      if (-not $letter -and -not $guid) { continue }
      $label = ''
      $fs = ''
      if ($guid -and $volumes.ContainsKey($guid)) {
        $label = [string]$volumes[$guid].FileSystemLabel
        $fs = [string]$volumes[$guid].FileSystem
      }
      [pscustomobject]@{ Letter = $letter; Guid = $guid; Folders = @($folders); Label = $label; FileSystem = $fs }
    })
    [pscustomobject]@{
      Number = $n; Name = [string]$d.FriendlyName; Size = [uint64]$d.Size; Bus = [string]$d.BusType
      External = $external.ContainsKey([string]$n); System = $system
      ReadOnly = [bool]$d.IsReadOnly; Offline = [bool]$d.IsOffline; Dynamic = $dynamic; Volumes = @($found)
    }
  })
  $json = ConvertTo-Json -InputObject $disks -Depth 8 -Compress
  try { $json = [regex]::Replace($json, '[^\x20-\x7e]', { param($m) '\u{0:x4}' -f [int][char]$m.Value }) } catch { }
  $json
} catch {
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 2
}
"#;

/// Where PowerShell is. Not looked up through PATH: DD-GUI runs as administrator, so a
/// powershell.exe lying next to it (in Downloads, say) must not get to run instead.
#[cfg(windows)]
fn powershell() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
}

#[cfg(windows)]
pub fn list() -> Result<Vec<Drive>, String> {
    let out = super::run(
        Command::new(powershell())
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"])
            .args(["-EncodedCommand", &encoded_command(SCRIPT)])
            .creation_flags(CREATE_NO_WINDOW),
        "PowerShell",
        // Its first start after boot, loading the Storage module, can take a while.
        Duration::from_secs(60),
    )?;
    if !out.status.success() {
        return Err(format!("PowerShell couldn't list the drives: {}", error_text(&out.stderr)));
    }
    parse(&out.stdout)
}

/// A script for `powershell -EncodedCommand`: base64 of its UTF-16LE text, which nothing
/// on the way can misquote.
fn encoded_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// PowerShell's error output in a line. Errors it doesn't handle itself may come as
/// CLIXML (`#< CLIXML <Objs …><S S="Error">…</S>`).
fn error_text(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    if let Some(start) = text.find(r#"<S S="Error">"#) {
        let rest = &text[start + 13..];
        let message = rest[..rest.find("</S>").unwrap_or(rest.len())].replace("_x000D__x000A_", " ");
        let message = message.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&");
        return message.trim().to_owned();
    }
    match super::first_line(stderr) {
        line if line.is_empty() => "it stopped without saying why".to_owned(),
        line => line,
    }
}

fn parse(stdout: &[u8]) -> Result<Vec<Drive>, String> {
    // The JSON starts a line; anything else PowerShell may print around it is ignored.
    let text = String::from_utf8_lossy(stdout);
    let json = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .map(|i| text[i..].trim_start_matches(['\u{feff}', ' ', '\t', '\r']))
        .filter(|rest| rest.starts_with(['[', '{']))
        .find_map(|rest| serde_json::Deserializer::from_str(rest).into_iter::<Value>().next()?.ok())
        .ok_or_else(|| {
            let what = super::first_line(stdout);
            if what.is_empty() {
                "PowerShell listed no drives (is PowerShell working on this system?)".to_owned()
            } else {
                format!("couldn't read PowerShell's list of drives: {what}")
            }
        })?;
    let disks: Vec<&Value> = match &json {
        Value::Array(list) => list.iter().collect(),
        // Windows PowerShell may write an array as {"value": […], "Count": n}…
        Value::Object(map) if map.contains_key("value") && map.contains_key("Count") => items(&json, "value"),
        // …or a lone disk without its array.
        Value::Object(_) => vec![&json],
        _ => Vec::new(),
    };
    Ok(disks.into_iter().filter_map(drive).collect())
}

/// Get-Disk's BusType names, by number (STORAGE_BUS_TYPE), in case it comes as a number.
const BUS_TYPES: [&str; 20] = [
    "Unknown", "SCSI", "ATAPI", "ATA", "1394", "SSA", "Fibre Channel", "USB", "RAID", "iSCSI", "SAS", "SATA", "SD",
    "MMC", "Virtual", "File Backed Virtual", "Storage Spaces", "NVMe", "SCM", "UFS",
];

fn drive(d: &Value) -> Option<Drive> {
    let number = match &d["Number"] {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let size = super::number(d, "Size");
    if size == 0 {
        return None;
    }
    let bus = text(d, "Bus").unwrap_or_default();
    let bus = match bus.parse::<usize>() {
        Ok(n) => BUS_TYPES.get(n).copied().unwrap_or("Unknown").to_owned(),
        Err(_) => bus,
    };
    let (kind, bus, removable) = match bus.as_str() {
        "USB" => (DriveKind::Usb, bus, true),
        "SD" | "MMC" => (DriveKind::Sd, "SD".to_owned(), true),
        "NVMe" | "SCM" | "UFS" => (DriveKind::Ssd, bus, false),
        "File Backed Virtual" => (DriveKind::Virtual, "VHD".to_owned(), false),
        "Virtual" => (DriveKind::Virtual, bus, false),
        "Unknown" | "" => (DriveKind::Hdd, String::new(), false),
        _ => (DriveKind::Hdd, bus, false),
    };
    // eSATA, Thunderbolt and some USB enclosures (UAS) report SATA, NVMe or SCSI;
    // Windows still calls their media external.
    let removable = removable || (flag(d, "External") && kind != DriveKind::Virtual);

    let mut mountpoints = Vec::new();
    let mut volumes = Vec::new();
    let mut lock = Vec::new();
    for v in items(d, "Volumes") {
        let letter = text(v, "Letter").filter(|l| l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()));
        let guid = text(v, "Guid").and_then(|g| volume_name(&g));
        match (&letter, guid) {
            (Some(letter), _) => {
                let letter = letter.to_ascii_uppercase();
                mountpoints.push(format!(r"{letter}:\"));
                lock.push(format!("{letter}:"));
            }
            (None, Some(guid)) => lock.push(guid),
            (None, None) => continue,
        }
        mountpoints.extend(items(v, "Folders").into_iter().filter_map(Value::as_str).map(str::to_owned));
        match (text(v, "Label"), text(v, "FileSystem")) {
            (Some(label), Some(fs)) => volumes.push(format!("{label} ({fs})")),
            (Some(label), None) => volumes.push(label),
            (None, Some(fs)) => volumes.push(fs),
            (None, None) => {}
        }
    }

    let path = format!(r"\\.\PhysicalDrive{number}");
    let name = tidy(text(d, "Name").unwrap_or_default().trim_end_matches(" USB Device").trim_end_matches(" SCSI Disk Device"));
    Some(Drive {
        io_path: path.clone(),
        name: if name.is_empty() { format!("Disk {number}") } else { name },
        path,
        size,
        bus,
        kind,
        removable,
        system: flag(d, "System"),
        // A dynamic disk's volumes can span disks and aren't locked here: only read it.
        read_only: flag(d, "ReadOnly") || flag(d, "Dynamic"),
        mountpoints,
        volumes,
        unmount: Vec::new(),
        lock,
    })
}

/// `\\?\Volume{…}\` → `\\?\Volume{…}`: the volume itself (with the backslash, it would
/// be its root folder), as `--ddgui-lock` takes it.
fn volume_name(path: &str) -> Option<String> {
    let guid = path.strip_prefix(r"\\?\")?.trim_end_matches('\\');
    (guid.starts_with("Volume{") && guid.ends_with('}') && !guid.contains(['\\', '/'])).then(|| format!(r"\\?\{guid}"))
}

/// The worker's volume locks are gone once it exits, and Windows mounts a dismounted
/// volume again the next time anything uses it. Using each one now makes it show up
/// in Explorer right away.
#[cfg(windows)]
pub fn remount(drive: &Drive) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    for volume in &drive.lock {
        let root = format!(r"{volume}\");
        let root: Vec<u16> = root.encode_utf16().chain([0]).collect();
        let mut name = [0u16; 261];
        // SAFETY: `root` is NUL-terminated; `name` is as long as we say; the other outputs may be null.
        unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the script prints on a PC with a system NVMe, a USB stick with a lettered
    /// FAT32 volume and an EFI partition without a letter, and an attached VHDX.
    const LISTING: &str = r#"[
      {"Number":0,"Name":"Samsung SSD 980 PRO 1TB","Size":1000204886016,"Bus":"NVMe","External":false,"System":true,
       "ReadOnly":false,"Offline":false,"Volumes":[
         {"Letter":"","Guid":"\\\\?\\Volume{11111111-1111-1111-1111-111111111111}\\","Folders":[],"Label":"","FileSystem":"FAT32"},
         {"Letter":"C","Guid":"\\\\?\\Volume{22222222-2222-2222-2222-222222222222}\\","Folders":[],"Label":"Windows","FileSystem":"NTFS"}]},
      {"Number":2,"Name":"SanDisk Cruzer Blade USB Device","Size":31914983424,"Bus":"USB","External":true,"System":false,
       "ReadOnly":false,"Offline":false,"Volumes":[
         {"Letter":"E","Guid":"\\\\?\\Volume{33333333-3333-3333-3333-333333333333}\\","Folders":["C:\\mnt\\stick\\"],"Label":"Cl\u00e9 USB","FileSystem":"FAT32"},
         {"Letter":"\u0000","Guid":"\\\\?\\Volume{44444444-4444-4444-4444-444444444444}\\","Folders":[],"Label":"","FileSystem":"FAT"}]},
      {"Number":3,"Name":"Msft Virtual Disk","Size":268435456,"Bus":"File Backed Virtual","External":false,"System":false,
       "ReadOnly":false,"Offline":false,"Volumes":{"value":[{"Letter":"V","Guid":"","Folders":[],"Label":"VHD","FileSystem":"NTFS"}],"Count":1}},
      {"Number":4,"Name":"Generic STORAGE DEVICE USB Device","Size":0,"Bus":"USB","Volumes":[]},
      {"Number":null,"Name":"ghost","Size":1000,"Bus":"SATA"}
    ]"#;

    #[test]
    fn lists_disks_and_every_volume() {
        let drives = parse(LISTING.as_bytes()).unwrap();
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, [r"\\.\PhysicalDrive0", r"\\.\PhysicalDrive2", r"\\.\PhysicalDrive3"]);

        let system = &drives[0];
        assert!(system.system && !system.removable);
        assert_eq!((system.kind, system.bus.as_str()), (DriveKind::Ssd, "NVMe"));
        assert_eq!(system.lock, [r"\\?\Volume{11111111-1111-1111-1111-111111111111}", "C:"]);

        let stick = &drives[1];
        assert_eq!((stick.name.as_str(), stick.kind, stick.removable, stick.system), ("SanDisk Cruzer Blade", DriveKind::Usb, true, false));
        assert_eq!(stick.mountpoints, [r"E:\", r"C:\mnt\stick\"]);
        // A NUL "letter" (no letter) is no letter: the volume is locked by its GUID.
        assert_eq!(stick.lock, ["E:", r"\\?\Volume{44444444-4444-4444-4444-444444444444}"]);
        assert_eq!(stick.volumes, ["Clé USB (FAT32)", "FAT"]);

        let vhd = &drives[2];
        assert_eq!((vhd.kind, vhd.bus.as_str(), vhd.removable), (DriveKind::Virtual, "VHD", false));
        assert_eq!((vhd.lock.clone(), vhd.volumes.clone()), (vec!["V:".to_owned()], vec!["VHD (NTFS)".to_owned()]));
    }

    /// The script's real output, from PowerShell 7 with stand-ins for Get-Disk,
    /// Get-Partition, Get-Volume and Get-CimInstance (a page file on D:, a NUL drive letter
    /// for partitions without one, a label with an é, a VHD without volumes, a dynamic
    /// disk whose BusType came as a number).
    const FROM_POWERSHELL: &str = r#"[{"Number":0,"Name":"Samsung SSD 980 PRO 1TB","Size":1000204886016,"Bus":"NVMe","External":false,"System":true,"ReadOnly":false,"Offline":false,"Dynamic":false,"Volumes":[{"Letter":"","Guid":"\\\\?\\Volume{11111111-0000-0000-0000-000000000001}\\","Folders":[],"Label":"","FileSystem":"FAT32"},{"Letter":"C","Guid":"\\\\?\\Volume{11111111-0000-0000-0000-000000000002}\\","Folders":[],"Label":"Windows","FileSystem":"NTFS"}]},{"Number":1,"Name":"SanDisk Cruzer Blade","Size":31914983424,"Bus":"USB","External":true,"System":false,"ReadOnly":false,"Offline":false,"Dynamic":false,"Volumes":[{"Letter":"E","Guid":"\\\\?\\Volume{22222222-0000-0000-0000-000000000001}\\","Folders":["C:\\mnt\\stick\\"],"Label":"Cl\u00e9 USB","FileSystem":"FAT32"},{"Letter":"","Guid":"\\\\?\\Volume{22222222-0000-0000-0000-000000000002}\\","Folders":[],"Label":"VTOYEFI","FileSystem":"FAT"}]},{"Number":2,"Name":"WDC WD20EZRZ","Size":2000398934016,"Bus":"SATA","External":false,"System":true,"ReadOnly":false,"Offline":false,"Dynamic":false,"Volumes":[{"Letter":"D","Guid":"\\\\?\\Volume{33333333-0000-0000-0000-000000000001}\\","Folders":[],"Label":"Data","FileSystem":"NTFS"}]},{"Number":3,"Name":"Msft Virtual Disk","Size":1073741824,"Bus":"File Backed Virtual","External":false,"System":false,"ReadOnly":false,"Offline":false,"Dynamic":false,"Volumes":[]},{"Number":4,"Name":"ST1000DM010-2EP102","Size":1000204886016,"Bus":"11","External":false,"System":false,"ReadOnly":false,"Offline":false,"Dynamic":true,"Volumes":[]}]"#;

    #[test]
    fn reads_what_the_script_prints() {
        let drives = parse(FROM_POWERSHELL.as_bytes()).unwrap();
        let summary: Vec<(&str, DriveKind, bool, bool)> =
            drives.iter().map(|d| (d.path.as_str(), d.kind, d.removable, d.system)).collect();
        assert_eq!(
            summary,
            [
                (r"\\.\PhysicalDrive0", DriveKind::Ssd, false, true),
                (r"\\.\PhysicalDrive1", DriveKind::Usb, true, false),
                // Holds the page file.
                (r"\\.\PhysicalDrive2", DriveKind::Hdd, false, true),
                (r"\\.\PhysicalDrive3", DriveKind::Virtual, false, false),
                (r"\\.\PhysicalDrive4", DriveKind::Hdd, false, false),
            ]
        );
        let dynamic = &drives[4];
        assert_eq!((dynamic.bus.as_str(), dynamic.read_only), ("SATA", true));
        let stick = &drives[1];
        assert_eq!(stick.lock, ["E:", r"\\?\Volume{22222222-0000-0000-0000-000000000002}"]);
        assert_eq!(stick.mountpoints, [r"E:\", r"C:\mnt\stick\"]);
        assert_eq!(stick.volumes, ["Clé USB (FAT32)", "VTOYEFI (FAT)"]);
        assert!(drives[3].lock.is_empty() && drives[3].volumes.is_empty());
    }

    #[test]
    fn copes_with_what_powershell_does_to_json() {
        // One disk (PowerShell may drop the array), a BusType as a number, a byte order
        // mark and a warning before the JSON.
        let out = "\u{feff}WARNING: something [odd]\r\n{\"Number\":\"1\",\"Name\":\"\",\"Size\":\"8004304896\",\"Bus\":\"7\",\"Volumes\":null}\r\n";
        let drives = parse(out.as_bytes()).unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!((drives[0].name.as_str(), drives[0].bus.as_str(), drives[0].kind), ("Disk 1", "USB", DriveKind::Usb));
        assert!(drives[0].lock.is_empty());
        assert!(parse(b"[]").unwrap().is_empty());
    }

    #[test]
    fn says_what_went_wrong() {
        assert!(parse(b"").unwrap_err().contains("PowerShell listed no drives"));
        assert!(parse(b"Get-Disk : Access denied\r\n").unwrap_err().contains("Access denied"));
        let clixml = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\"><S S=\"Error\">Get-Disk : Invalid class _x000D__x000A_</S></Objs>";
        assert_eq!(error_text(clixml.as_bytes()), "Get-Disk : Invalid class");
        assert_eq!(error_text(b"The Storage module isn't there\r\n"), "The Storage module isn't there");
        assert_eq!(error_text(b""), "it stopped without saying why");
    }

    #[test]
    fn encodes_commands_as_powershell_wants() {
        // [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('dir'))
        assert_eq!(encoded_command("dir"), "ZABpAHIA");
        assert_eq!(encoded_command("ab"), "YQBiAA==");
        assert_eq!(encoded_command("é"), "6QA=");
    }

    #[test]
    fn volume_names() {
        assert_eq!(volume_name(r"\\?\Volume{abc}\").as_deref(), Some(r"\\?\Volume{abc}"));
        assert_eq!(volume_name(r"E:\"), None);
        assert_eq!(volume_name(r"\\?\Volume{abc}\..\x"), None);
    }

    #[test]
    fn the_script_fits_a_command_line() {
        // Windows allows 32767 characters in all.
        assert!(encoded_command(SCRIPT).len() < 16_000);
    }
}
