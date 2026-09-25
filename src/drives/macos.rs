use super::{Drive, DriveKind, tidy};
use plist::{Dictionary, Value};
use std::process::Command;

pub fn list() -> Result<Vec<Drive>, String> {
    let listing = diskutil(&["list", "-plist", "physical"])?;
    let disks = listing
        .as_dictionary()
        .and_then(|d| d.get("AllDisksAndPartitions"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(disks.iter().filter_map(|d| d.as_dictionary()).filter_map(drive).collect())
}

fn diskutil(args: &[&str]) -> Result<Value, String> {
    let out = Command::new("diskutil").args(args).output().map_err(|e| format!("couldn't run diskutil: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    plist::from_bytes(&out.stdout).map_err(|e| format!("couldn't read diskutil output: {e}"))
}

fn drive(disk: &Dictionary) -> Option<Drive> {
    let id = string(disk, "DeviceIdentifier")?;
    let info = diskutil(&["info", "-plist", &id]).ok()?;
    let info = info.as_dictionary()?;
    let size = number(info, "TotalSize").or_else(|| number(info, "Size")).or_else(|| number(disk, "Size"))?;
    if size == 0 {
        return None;
    }

    let internal = boolean(info, "Internal").unwrap_or(false);
    let bus = string(info, "BusProtocol").unwrap_or_default();
    let virtual_disk = string(info, "VirtualOrPhysical").is_some_and(|v| v == "Virtual") || bus == "Disk Image";
    let (kind, bus) = match bus.as_str() {
        _ if virtual_disk => (DriveKind::Virtual, "Virtual".to_owned()),
        "USB" => (DriveKind::Usb, bus),
        "Secure Digital" | "SD" => (DriveKind::Sd, "SD".to_owned()),
        "PCI-Express" | "PCI" | "NVMe" | "Apple Fabric" => (DriveKind::Ssd, bus),
        _ if boolean(info, "SolidState").unwrap_or(false) => (DriveKind::Ssd, bus),
        _ => (DriveKind::Hdd, bus),
    };

    let mut mountpoints = Vec::new();
    let mut volumes = Vec::new();
    let mut collect = |d: &Dictionary| {
        if let Some(mount) = string(d, "MountPoint") {
            mountpoints.push(mount);
        }
        let content = string(d, "Content").unwrap_or_default();
        match string(d, "VolumeName") {
            Some(name) if !content.is_empty() => volumes.push(format!("{name} ({content})")),
            Some(name) => volumes.push(name),
            None => {}
        }
    };
    collect(disk);
    for part in disk.get("Partitions").and_then(Value::as_array).into_iter().flatten() {
        if let Some(part) = part.as_dictionary() {
            collect(part);
        }
    }

    let name = string(info, "MediaName")
        .or_else(|| string(info, "IORegistryEntryName"))
        .map(|n| tidy(n.trim_end_matches(" Media")))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.clone());

    Some(Drive {
        path: format!("/dev/{id}"),
        // The raw device skips the buffer cache and is many times faster.
        io_path: format!("/dev/r{id}"),
        name,
        size,
        bus,
        kind,
        removable: !internal
            || boolean(info, "RemovableMedia").unwrap_or(false)
            || boolean(info, "Ejectable").unwrap_or(false),
        // Internal disks hold macOS on nearly every Mac; treat them as off-limits.
        system: internal || mountpoints.iter().any(|m| m == "/"),
        read_only: boolean(info, "WritableMedia") == Some(false),
        mountpoints,
        volumes,
        unmount: vec![format!("/dev/{id}")],
        lock: Vec::new(),
    })
}

fn string(d: &Dictionary, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_string).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn number(d: &Dictionary, key: &str) -> Option<u64> {
    d.get(key).and_then(Value::as_unsigned_integer)
}

fn boolean(d: &Dictionary, key: &str) -> Option<bool> {
    d.get(key).and_then(Value::as_boolean)
}
