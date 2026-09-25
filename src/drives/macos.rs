//! macOS: diskutil lists the drives and mounts them again after a copy.
//!
//! `diskutil list -plist` gives every whole disk with its partitions. APFS complicates it:
//! each APFS container shows up as a disk of its own ("synthesized"), whose volumes live on
//! a partition of the real disk (its "physical store"). Those containers aren't drives to
//! offer; their volumes (and mount points, "/" among them) belong to the disk that stores
//! them. Disk images (hdiutil attach) are listed, as virtual drives.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::{Drive, DriveKind, flag, items, number, text, tidy};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::{process::Command, time::Duration};

#[cfg(target_os = "macos")]
const DISKUTIL: &str = "/usr/sbin/diskutil";

/// Mount points that mean "this disk runs the system".
fn is_system_mount(mount: &str) -> bool {
    mount == "/" || mount.starts_with("/System/Volumes/") || mount == "/private/var/vm"
}

#[cfg(target_os = "macos")]
pub fn list() -> Result<Vec<Drive>, String> {
    let listing = diskutil(&["list", "-plist"], Duration::from_secs(30))?;
    Ok(drives(&listing, &|id| diskutil(&["info", "-plist", id], Duration::from_secs(15)).ok()))
}

#[cfg(target_os = "macos")]
fn diskutil(args: &[&str], limit: Duration) -> Result<Value, String> {
    let out = super::run(Command::new(DISKUTIL).args(args), "diskutil", limit)?;
    if !out.status.success() {
        return Err(format!("diskutil failed: {}", super::first_line(&out.stderr)));
    }
    let plist: plist::Value = plist::from_bytes(&out.stdout).map_err(|e| format!("couldn't read diskutil's output: {e}"))?;
    Ok(to_json(&plist))
}

/// diskutil's property lists as JSON, which the rest of this file reads. Dates and raw
/// data (which no key used here holds) become null.
#[cfg(target_os = "macos")]
fn to_json(v: &plist::Value) -> Value {
    match v {
        plist::Value::Array(list) => Value::Array(list.iter().map(to_json).collect()),
        plist::Value::Dictionary(dict) => Value::Object(dict.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        plist::Value::Boolean(b) => Value::Bool(*b),
        plist::Value::Integer(i) => i.as_unsigned().map(Value::from).or_else(|| i.as_signed().map(Value::from)).unwrap_or(Value::Null),
        plist::Value::Real(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        plist::Value::String(s) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

/// An APFS container, as the volumes it adds to the disk storing it.
#[derive(Default)]
struct Container {
    id: String,
    mountpoints: Vec<String>,
    volumes: Vec<String>,
}

/// "disk4s2" → "disk4"; "disk4" stays.
fn whole_disk(id: &str) -> &str {
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    match id.strip_prefix("disk") {
        Some(rest) => &id[..4 + digits(rest)],
        None => id,
    }
}

/// The drives in `diskutil list -plist` (as JSON). `info` gives `diskutil info -plist`
/// for a disk; disks it has nothing for are left out.
fn drives(listing: &Value, info: &dyn Fn(&str) -> Option<Value>) -> Vec<Drive> {
    let entries = items(listing, "AllDisksAndPartitions");
    // APFS containers by the disk that stores them.
    let mut containers: HashMap<String, Vec<Container>> = HashMap::new();
    for entry in &entries {
        let stores = items(entry, "APFSPhysicalStores");
        if stores.is_empty() {
            continue;
        }
        let mut container = Container { id: text(entry, "DeviceIdentifier").unwrap_or_default(), ..Default::default() };
        for volume in items(entry, "APFSVolumes") {
            container.mountpoints.extend(text(volume, "MountPoint"));
            for snapshot in items(volume, "MountedSnapshots") {
                container.mountpoints.extend(text(snapshot, "SnapshotMountPoint"));
            }
            if let Some(name) = text(volume, "VolumeName") {
                container.volumes.push(format!("{name} (APFS)"));
            }
        }
        let mut disks: Vec<&str> = stores.iter().filter_map(|s| s["DeviceIdentifier"].as_str()).map(whole_disk).collect();
        disks.dedup();
        for disk in disks {
            containers.entry(disk.to_owned()).or_default().push(Container {
                id: container.id.clone(),
                mountpoints: container.mountpoints.clone(),
                volumes: container.volumes.clone(),
            });
        }
    }
    entries
        .iter()
        .filter(|entry| items(entry, "APFSPhysicalStores").is_empty())
        .filter_map(|entry| {
            let id = text(entry, "DeviceIdentifier")?;
            let details = info(&id)?;
            drive(entry, &details, containers.get(&id).map_or(&[][..], Vec::as_slice))
        })
        .collect()
}

fn drive(entry: &Value, info: &Value, containers: &[Container]) -> Option<Drive> {
    let id = text(entry, "DeviceIdentifier")?;
    let size = [number(info, "TotalSize"), number(info, "Size"), number(entry, "Size")].into_iter().find(|&n| n > 0)?;

    let internal = flag(info, "Internal");
    let removable_media = flag(info, "RemovableMedia") || flag(info, "Removable");
    let ejectable = flag(info, "Ejectable");
    let protocol = text(info, "BusProtocol").unwrap_or_default();
    let virtual_disk = text(info, "VirtualOrPhysical").is_some_and(|v| v == "Virtual") || protocol == "Disk Image";
    let media_name = text(info, "MediaName").or_else(|| text(info, "IORegistryEntryName")).unwrap_or_default();
    // Built-in card readers are internal; some are on USB or PCIe inside the Mac.
    let card_reader = ["SD Card", "SDXC", "Card Reader"].iter().any(|n| media_name.contains(n));
    let (kind, bus) = match protocol.as_str() {
        _ if virtual_disk => (DriveKind::Virtual, if protocol == "Disk Image" { "Disk image" } else { "Virtual" }.to_owned()),
        "Secure Digital" | "SD" => (DriveKind::Sd, "SD".to_owned()),
        _ if card_reader => (DriveKind::Sd, "SD".to_owned()),
        "USB" => (DriveKind::Usb, protocol),
        "PCI-Express" | "PCI" | "NVMe" | "Apple Fabric" => (DriveKind::Ssd, protocol),
        _ if flag(info, "SolidState") => (DriveKind::Ssd, protocol),
        _ => (DriveKind::Hdd, protocol),
    };

    let mut mountpoints = Vec::new();
    let mut volumes = Vec::new();
    let mut collect = |d: &Value| {
        mountpoints.extend(text(d, "MountPoint"));
        let content = text(d, "Content").map(|c| content_name(&c));
        match (text(d, "VolumeName"), content) {
            (Some(name), Some(content)) if !content.is_empty() && content != name => volumes.push(format!("{name} ({content})")),
            (Some(name), _) => volumes.push(name),
            (None, _) => {}
        }
    };
    collect(entry);
    for part in items(entry, "Partitions") {
        collect(part);
    }
    for container in containers {
        mountpoints.extend(container.mountpoints.iter().cloned());
        volumes.extend(container.volumes.iter().cloned());
    }
    mountpoints.dedup();

    let name = tidy(media_name.trim_end_matches(" Media"));
    let path = format!("/dev/{id}");
    // Unmounting the APFS containers first, then the disk, covers every volume on it.
    let mut unmount: Vec<String> = containers.iter().filter(|c| !c.id.is_empty()).map(|c| format!("/dev/{}", c.id)).collect();
    unmount.push(path.clone());

    Some(Drive {
        // The raw device skips the buffer cache and is many times faster.
        io_path: format!("/dev/r{id}"),
        name: if name.is_empty() { id.clone() } else { name },
        size,
        bus,
        kind,
        // Disk images are listed like Linux's loop devices: there, but not in the way.
        removable: !virtual_disk && (!internal || removable_media || ejectable),
        // Macs run from their built-in disk, which is also off-limits when started from
        // another one. Cards in built-in readers are internal too, but removable.
        system: mountpoints.iter().any(|m| is_system_mount(m)) || (internal && !virtual_disk && !removable_media && !ejectable),
        read_only: info.get("WritableMedia").and_then(Value::as_bool) == Some(false),
        mountpoints,
        volumes,
        unmount,
        path,
        lock: Vec::new(),
    })
}

/// A partition's content type as people call it: "Apple_HFS" → "HFS+".
fn content_name(content: &str) -> String {
    match content {
        "Apple_APFS" => "APFS",
        "Apple_HFS" | "Apple_HFSX" => "HFS+",
        "DOS_FAT_32" | "Windows_FAT_32" => "FAT32",
        "DOS_FAT_16" | "Windows_FAT_16" => "FAT16",
        "DOS_FAT_12" => "FAT12",
        "Windows_NTFS" => "NTFS",
        "Linux" | "Linux Filesystem" => "Linux",
        "Linux_Swap" => "swap",
        // The partition table itself, or a GUID nobody names.
        c if c.ends_with("_partition_scheme") || c.len() == 36 && c.matches('-').count() == 4 => "",
        c => c,
    }
    .to_owned()
}

/// `diskutil mountDisk` for the disk and the APFS containers on it, as the user (who may
/// mount external disks without a password). Fine as long as what was mounted before the
/// copy is mounted again: some volumes (Linux file systems, say) never were.
#[cfg(target_os = "macos")]
pub fn remount(drive: &Drive) -> Result<(), String> {
    if drive.mountpoints.is_empty() {
        return Ok(());
    }
    let mut failure = None;
    // The disk first, then its APFS containers.
    let disks = std::iter::once(&drive.path).chain(drive.unmount.iter().filter(|d| **d != drive.path));
    for disk in disks {
        match super::run(Command::new(DISKUTIL).args(["mountDisk", disk]), "diskutil", Duration::from_secs(90)) {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                failure.get_or_insert_with(|| {
                    let text = [out.stderr.as_slice(), out.stdout.as_slice()].concat();
                    super::first_line(&text)
                });
            }
            Err(err) => return Err(err),
        }
    }
    let Some(failure) = failure else { return Ok(()) };
    let now = list()?.into_iter().find(|d| d.path == drive.path).map_or(0, |d| d.mountpoints.len());
    if now >= drive.mountpoints.len() {
        return Ok(());
    }
    Err(if failure.is_empty() { "diskutil couldn't mount it".to_owned() } else { failure })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `diskutil list -plist` of an Apple Silicon Mac with a USB stick (FAT32), an
    /// external SSD with APFS, and an SD card in the built-in reader.
    fn listing() -> Value {
        json!({"AllDisksAndPartitions": [
            {"Content": "GUID_partition_scheme", "DeviceIdentifier": "disk0", "OSInternal": true, "Size": 500277790720u64,
             "Partitions": [
                {"Content": "Apple_APFS_ISC", "DeviceIdentifier": "disk0s1", "Size": 524288000},
                {"Content": "Apple_APFS", "DeviceIdentifier": "disk0s2", "Size": 494384795648u64},
                {"Content": "Apple_APFS_Recovery", "DeviceIdentifier": "disk0s3", "Size": 5368664064u64}]},
            {"APFSPhysicalStores": [{"DeviceIdentifier": "disk0s2"}], "Content": "EF57347C-0000-11AA-AA11-00306543ECAC",
             "DeviceIdentifier": "disk3", "OSInternal": false, "Partitions": [], "Size": 494384795648u64,
             "APFSVolumes": [
                {"DeviceIdentifier": "disk3s1", "VolumeName": "Macintosh HD",
                 "MountedSnapshots": [{"SnapshotBSD": "disk3s1s1", "SnapshotMountPoint": "/", "Sealed": "Yes"}]},
                {"DeviceIdentifier": "disk3s5", "VolumeName": "Macintosh HD - Data", "MountPoint": "/System/Volumes/Data"},
                {"DeviceIdentifier": "disk3s6", "VolumeName": "VM", "MountPoint": "/System/Volumes/VM"}]},
            {"Content": "FDisk_partition_scheme", "DeviceIdentifier": "disk4", "OSInternal": false, "Size": 31914983424u64,
             "Partitions": [{"Content": "DOS_FAT_32", "DeviceIdentifier": "disk4s1", "MountPoint": "/Volumes/STICK",
                             "Size": 31913934848u64, "VolumeName": "STICK"}]},
            {"Content": "GUID_partition_scheme", "DeviceIdentifier": "disk5", "OSInternal": false, "Size": 1000204886016u64,
             "Partitions": [
                {"Content": "EFI", "DeviceIdentifier": "disk5s1", "Size": 209715200, "VolumeName": "EFI"},
                {"Content": "Apple_APFS", "DeviceIdentifier": "disk5s2", "Size": 999995129856u64}]},
            {"APFSPhysicalStores": [{"DeviceIdentifier": "disk5s2"}], "Content": "EF57347C-0000-11AA-AA11-00306543ECAC",
             "DeviceIdentifier": "disk6", "Partitions": [], "Size": 999995129856u64,
             "APFSVolumes": [{"DeviceIdentifier": "disk6s1", "VolumeName": "Backup", "MountPoint": "/Volumes/Backup"}]},
            {"Content": "FDisk_partition_scheme", "DeviceIdentifier": "disk7", "Size": 63864569856u64,
             "Partitions": [{"Content": "Windows_FAT_32", "DeviceIdentifier": "disk7s1", "Size": 268435456, "VolumeName": "bootfs",
                             "MountPoint": "/Volumes/bootfs"},
                            {"Content": "Linux", "DeviceIdentifier": "disk7s2", "Size": 63594037248u64}]},
            {"Content": "GUID_partition_scheme", "DeviceIdentifier": "disk8", "Size": 268435456,
             "Partitions": [{"Content": "Microsoft Basic Data", "DeviceIdentifier": "disk8s1", "Size": 266338304, "VolumeName": "IMG",
                             "MountPoint": "/Volumes/IMG"}]}
        ]})
    }

    fn info(id: &str) -> Option<Value> {
        Some(match id {
            "disk0" => json!({"BusProtocol": "Apple Fabric", "Internal": true, "RemovableMedia": false, "Ejectable": false,
                              "SolidState": true, "VirtualOrPhysical": "Physical", "WritableMedia": true,
                              "MediaName": "APPLE SSD AP0512Q", "TotalSize": 500277790720u64}),
            "disk4" => json!({"BusProtocol": "USB", "Internal": false, "RemovableMedia": true, "Ejectable": true,
                              "VirtualOrPhysical": "Physical", "WritableMedia": true, "MediaName": "SanDisk Cruzer Blade Media",
                              "TotalSize": 31914983424u64}),
            "disk5" => json!({"BusProtocol": "USB", "Internal": false, "RemovableMedia": false, "Ejectable": true,
                              "SolidState": true, "VirtualOrPhysical": "Physical", "WritableMedia": true,
                              "MediaName": "Samsung Portable SSD T7", "TotalSize": 1000204886016u64}),
            "disk7" => json!({"BusProtocol": "Secure Digital", "Internal": true, "RemovableMedia": true, "Ejectable": true,
                              "VirtualOrPhysical": "Physical", "WritableMedia": true, "MediaName": "SD Card Reader",
                              "TotalSize": 63864569856u64}),
            "disk8" => json!({"BusProtocol": "Disk Image", "Internal": false, "RemovableMedia": true, "Ejectable": true,
                              "VirtualOrPhysical": "Virtual", "WritableMedia": true, "MediaName": "Disk Image",
                              "TotalSize": 268435456}),
            _ => return None,
        })
    }

    #[test]
    fn lists_disks_with_their_apfs_containers() {
        let list = drives(&listing(), &info);
        let paths: Vec<&str> = list.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/dev/disk0", "/dev/disk4", "/dev/disk5", "/dev/disk7", "/dev/disk8"]);

        let internal = &list[0];
        assert!(internal.system && !internal.removable);
        assert_eq!(internal.io_path, "/dev/rdisk0");
        assert!(internal.mountpoints.contains(&"/".to_owned()), "{:?}", internal.mountpoints);
        assert_eq!(internal.unmount, ["/dev/disk3", "/dev/disk0"]);

        let stick = &list[1];
        assert_eq!((stick.name.as_str(), stick.kind, stick.bus.as_str()), ("SanDisk Cruzer Blade", DriveKind::Usb, "USB"));
        assert!(stick.removable && !stick.system && !stick.read_only);
        assert_eq!((stick.mountpoints.clone(), stick.volumes.clone()), (vec!["/Volumes/STICK".to_owned()], vec!["STICK (FAT32)".to_owned()]));
        assert_eq!(stick.unmount, ["/dev/disk4"]);

        let ssd = &list[2];
        assert_eq!((ssd.kind, ssd.removable, ssd.system), (DriveKind::Usb, true, false));
        assert_eq!(ssd.mountpoints, ["/Volumes/Backup"]);
        assert_eq!(ssd.volumes, ["EFI", "Backup (APFS)"]);
        assert_eq!(ssd.unmount, ["/dev/disk6", "/dev/disk5"]);

        // Internal, but a card: never the system disk.
        let card = &list[3];
        assert_eq!((card.kind, card.bus.as_str(), card.removable, card.system), (DriveKind::Sd, "SD", true, false));
        assert_eq!(card.volumes, ["bootfs (FAT32)"]);

        let image = &list[4];
        assert_eq!((image.kind, image.bus.as_str(), image.removable, image.system), (DriveKind::Virtual, "Disk image", false, false));
        assert_eq!(image.volumes, ["IMG (Microsoft Basic Data)"]);
    }

    #[test]
    fn an_external_disk_running_macos_is_the_system_disk() {
        let mut listing = listing();
        // Started from the external SSD: its container holds "/".
        listing["AllDisksAndPartitions"][4]["APFSVolumes"][0]["MountPoint"] = json!("/");
        let list = drives(&listing, &info);
        let ssd = list.iter().find(|d| d.path == "/dev/disk5").unwrap();
        assert!(ssd.system && ssd.removable);
    }

    #[test]
    fn names_whole_disks_and_contents() {
        assert_eq!(whole_disk("disk4s2"), "disk4");
        assert_eq!(whole_disk("disk12s1s3"), "disk12");
        assert_eq!(whole_disk("disk10"), "disk10");
        assert_eq!(content_name("Apple_HFS"), "HFS+");
        assert_eq!(content_name("GUID_partition_scheme"), "");
        assert_eq!(content_name("EF57347C-0000-11AA-AA11-00306543ECAC"), "");
        assert_eq!(content_name("Microsoft Basic Data"), "Microsoft Basic Data");
    }

    #[test]
    fn disks_without_info_or_size_are_left_out() {
        let empty = json!({"AllDisksAndPartitions": [{"DeviceIdentifier": "disk9", "Size": 0}]});
        assert!(drives(&empty, &|_| Some(json!({"TotalSize": 0}))).is_empty());
        assert!(drives(&listing(), &|_| None).is_empty());
    }
}
