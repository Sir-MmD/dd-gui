//! Linux: lsblk lists the drives; udisks mounts them again after a copy.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::{Drive, DriveKind, flag, number, text, tidy};
use crate::fmt;
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::{process::Command, time::Duration};

const COLUMNS: &str = "NAME,PATH,TYPE,SIZE,TRAN,RM,HOTPLUG,RO,ROTA,MODEL,VENDOR,LABEL,PARTLABEL,FSTYPE";

/// Mounts that mean "this disk runs the system".
const SYSTEM_MOUNTS: [&str; 8] = ["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home", "[SWAP]"];

#[cfg(target_os = "linux")]
pub fn list() -> Result<Vec<Drive>, String> {
    // MOUNTPOINTS needs util-linux 2.37+; fall back to MOUNTPOINT on older systems.
    let json = lsblk(&format!("{COLUMNS},MOUNTPOINTS")).or_else(|_| lsblk(&format!("{COLUMNS},MOUNTPOINT")))?;
    parse(&json, &mmc_type)
}

#[cfg(target_os = "linux")]
fn lsblk(columns: &str) -> Result<Vec<u8>, String> {
    let out = super::run(
        Command::new("lsblk").args(["--json", "--bytes", "--output", columns]),
        "lsblk",
        Duration::from_secs(20),
    )?;
    if !out.status.success() {
        return Err(format!("lsblk failed: {}", super::first_line(&out.stderr)));
    }
    Ok(out.stdout)
}

/// "SD" or "MMC" (eMMC, soldered on), as the kernel tells for an mmcblk device.
#[cfg(target_os = "linux")]
fn mmc_type(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/block/{name}/device/type")).ok().map(|t| t.trim().to_owned())
}

/// The drives in lsblk's JSON. `mmc_type` tells SD cards from eMMC.
fn parse(json: &[u8], mmc_type: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Drive>, String> {
    let root: Value = serde_json::from_slice(json).map_err(|e| format!("couldn't read lsblk's output: {e}"))?;
    let devices = root["blockdevices"].as_array().cloned().unwrap_or_default();
    Ok(devices.iter().filter_map(|dev| drive(dev, mmc_type)).collect())
}

fn drive(dev: &Value, mmc_type: &dyn Fn(&str) -> Option<String>) -> Option<Drive> {
    let name = text(dev, "name")?;
    let size = number(dev, "size");
    let kind = text(dev, "type").unwrap_or_default();
    let read_only = flag(dev, "ro");
    let is_loop = kind == "loop";
    // Loop devices (image files attached as disks) are handy for testing; skip the
    // read-only ones, which are typically snaps and the like.
    if size == 0 || !(kind == "disk" || (is_loop && !read_only)) || name.starts_with("zram") {
        return None;
    }
    // eMMC boot areas (mmcblk0boot0, mmcblk0boot1): small, read-only, and not disks anyone
    // means to copy.
    if name.starts_with("mmcblk") && name.contains("boot") {
        return None;
    }

    let path = text(dev, "path").unwrap_or_else(|| format!("/dev/{name}"));
    let tran = text(dev, "tran").unwrap_or_default().to_lowercase();
    let is_mmc = name.starts_with("mmcblk") || tran == "mmc";
    // Soldered-on eMMC is a built-in disk, not a card.
    let is_emmc = is_mmc && mmc_type(&name).is_some_and(|t| t.eq_ignore_ascii_case("MMC"));
    let is_sd = is_mmc && !is_emmc;

    let (kind, bus) = if is_loop {
        (DriveKind::Virtual, "Loop")
    } else if tran == "usb" {
        (DriveKind::Usb, "USB")
    } else if is_sd {
        (DriveKind::Sd, "SD")
    } else if is_emmc {
        (DriveKind::Ssd, "eMMC")
    } else if tran == "nvme" || name.starts_with("nvme") {
        (DriveKind::Ssd, "NVMe")
    } else if name.starts_with("vd") || tran == "virtio" {
        (DriveKind::Virtual, "Virtual")
    } else {
        let bus = match tran.as_str() {
            "sata" | "ata" => "SATA",
            "sas" => "SAS",
            "scsi" | "iscsi" => "SCSI",
            _ => "",
        };
        (if flag(dev, "rota") { DriveKind::Hdd } else { DriveKind::Ssd }, bus)
    };

    let mut mounts = Vec::new();
    let mut volumes = Vec::new();
    let mut unmount = Vec::new();
    walk(dev, &mut |node| {
        let node_mounts = mountpoints(node);
        if node_mounts.iter().any(|m| m != "[SWAP]") {
            unmount.push(text(node, "path").unwrap_or_else(|| format!("/dev/{}", text(node, "name").unwrap_or_default())));
        }
        mounts.extend(node_mounts);
        let label = text(node, "label").or_else(|| text(node, "partlabel"));
        if let Some(fs) = text(node, "fstype") {
            let fs = fmt::fs_name(&fs);
            volumes.push(match label {
                Some(label) => format!("{label} ({fs})"),
                None => fs,
            });
        }
    });
    let system = mounts.iter().any(|m| SYSTEM_MOUNTS.contains(&m.as_str()));
    mounts.retain(|m| m != "[SWAP]");
    mounts.dedup();

    let vendor = text(dev, "vendor").map(|v| tidy(&v)).unwrap_or_default();
    let model = text(dev, "model").map(|m| tidy(&m)).unwrap_or_default();
    let name_text = if is_loop {
        "Loop device".to_owned()
    } else if model.is_empty() {
        match (is_sd, is_emmc) {
            (true, _) => "SD card".to_owned(),
            (_, true) => "eMMC".to_owned(),
            _ => name.clone(),
        }
    } else if vendor.is_empty() || vendor == "ATA" || model.to_lowercase().starts_with(&vendor.to_lowercase()) {
        model
    } else {
        format!("{vendor} {model}")
    };

    Some(Drive {
        io_path: path.clone(),
        path,
        name: name_text,
        size,
        bus: bus.to_owned(),
        kind,
        removable: !is_emmc && (flag(dev, "rm") || flag(dev, "hotplug") || tran == "usb" || is_sd),
        system,
        read_only,
        mountpoints: mounts,
        volumes,
        unmount,
        lock: Vec::new(),
    })
}

fn walk(node: &Value, visit: &mut dyn FnMut(&Value)) {
    visit(node);
    for child in node["children"].as_array().into_iter().flatten() {
        walk(child, visit);
    }
}

fn mountpoints(node: &Value) -> Vec<String> {
    match (&node["mountpoints"], &node["mountpoint"]) {
        (Value::Array(list), _) => list.iter().filter_map(|m| m.as_str()).map(str::to_owned).collect(),
        (_, Value::String(m)) => vec![m.clone()],
        _ => Vec::new(),
    }
}

/// Mounts the partitions the worker unmounted (`drive.unmount`), through udisks as the
/// user: the same as clicking them in a file manager, so they land where the desktop
/// puts them (or where /etc/fstab says).
#[cfg(target_os = "linux")]
pub fn remount(drive: &Drive) -> Result<(), String> {
    remount_with(drive, std::path::Path::new("udisksctl"), &is_mounted)
}

#[cfg(target_os = "linux")]
fn remount_with(drive: &Drive, udisksctl: &std::path::Path, is_mounted: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let mut problems = Vec::new();
    for dev in &drive.unmount {
        // Gone (unplugged), or back already (mounted by something else meanwhile).
        if !std::path::Path::new(dev).exists() || is_mounted(dev) {
            continue;
        }
        let out = super::run(
            Command::new(udisksctl).args(["mount", "--no-user-interaction", "--block-device", dev]),
            "udisksctl",
            // Mounting may replay a journal or check the file system first.
            Duration::from_secs(90),
        );
        match out {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
                if let Some(problem) = mount_problem(dev, &text) {
                    problems.push(problem);
                }
            }
            // Without udisks there's no way to mount as the user; nothing else will work either.
            Err(err) if err.ends_with("isn't installed") => {
                return Err("udisksctl isn't installed, so it has to be mounted by hand".to_owned());
            }
            Err(err) => problems.push(format!("{dev}: {err}")),
        }
    }
    match problems.len() {
        0 => Ok(()),
        _ => Err(problems.join("; ")),
    }
}

/// What went wrong mounting `dev`, in a few words, or None when it doesn't matter: it was
/// mounted meanwhile, or it isn't something that mounts.
fn mount_problem(dev: &str, udisksctl_output: &str) -> Option<String> {
    let text = udisksctl_output;
    if text.contains("AlreadyMounted") || text.contains("is not a mountable filesystem") || text.contains("NotSupported") {
        return None;
    }
    if text.contains("NotAuthorized") || text.contains("not authorized") || text.contains("must be superuser") {
        return Some(format!("{dev}: not allowed to mount it"));
    }
    if text.contains("Error looking up object") {
        return None;
    }
    // "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: <why>"
    let why = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("unknown error");
    let why = why.rsplit(": ").next().unwrap_or(why).trim_end_matches('.');
    Some(format!("{dev}: {why}"))
}

/// Is `dev` (or the device it links to) mounted anywhere right now?
#[cfg(target_os = "linux")]
fn is_mounted(dev: &str) -> bool {
    let real = std::fs::canonicalize(dev).unwrap_or_else(|_| dev.into());
    std::fs::read_to_string("/proc/self/mounts").is_ok_and(|mounts| {
        mounts.lines().filter_map(|line| line.split_whitespace().next()).any(|source| {
            source.starts_with('/') && std::fs::canonicalize(source).unwrap_or_else(|_| source.into()) == real
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mmc(_: &str) -> Option<String> {
        None
    }

    // Trimmed from `lsblk --json --bytes --output …,MOUNTPOINTS` (util-linux 2.40).
    const LISTING: &str = r#"{"blockdevices": [
      {"name":"sda","path":"/dev/sda","type":"disk","size":31914983424,"tran":"usb","rm":true,"hotplug":true,"ro":false,"rota":false,
       "model":"Cruzer_Blade","vendor":"SanDisk ","label":null,"partlabel":null,"fstype":null,"mountpoints":[null],
       "children":[
         {"name":"sda1","path":"/dev/sda1","type":"part","size":31913934848,"tran":null,"rm":true,"hotplug":true,"ro":false,"rota":false,
          "model":null,"vendor":null,"label":"STICK","partlabel":null,"fstype":"vfat","mountpoints":["/run/media/me/STICK"]}]},
      {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","size":512110190592,"tran":"nvme","rm":false,"hotplug":false,"ro":false,"rota":false,
       "model":"Samsung SSD 980 PRO 500GB","vendor":null,"label":null,"partlabel":null,"fstype":null,"mountpoints":[null],
       "children":[
         {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","size":1073741824,"fstype":"vfat","mountpoints":["/boot"]},
         {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","size":511035375616,"fstype":"crypto_LUKS","mountpoints":[null],
          "children":[{"name":"root","path":"/dev/mapper/root","type":"crypt","size":511018598400,"fstype":"ext4","mountpoints":["/", "/home"]}]}]},
      {"name":"loop0","path":"/dev/loop0","type":"loop","size":268435456,"ro":false,"rota":false,"fstype":null,"mountpoints":[null]},
      {"name":"loop1","path":"/dev/loop1","type":"loop","size":4096,"ro":true,"fstype":"squashfs","mountpoints":["/snap/core/1"]},
      {"name":"mmcblk0","path":"/dev/mmcblk0","type":"disk","size":15931539456,"tran":null,"rm":false,"hotplug":false,"ro":false,"rota":false,"mountpoints":[null]},
      {"name":"mmcblk1","path":"/dev/mmcblk1","type":"disk","size":63864569856,"tran":null,"rm":false,"hotplug":false,"ro":false,"rota":false,"mountpoints":[null]},
      {"name":"mmcblk1boot0","path":"/dev/mmcblk1boot0","type":"disk","size":4194304,"ro":true,"mountpoints":[null]},
      {"name":"zram0","path":"/dev/zram0","type":"disk","size":8589934592,"mountpoints":["[SWAP]"]},
      {"name":"sr0","path":"/dev/sr0","type":"rom","size":1073741312,"mountpoints":[null]}
    ]}"#;

    #[test]
    fn lists_drives() {
        let mmc = |name: &str| Some(if name == "mmcblk1" { "MMC" } else { "SD" }.to_owned());
        let drives = parse(LISTING.as_bytes(), &mmc).unwrap();
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/dev/sda", "/dev/nvme0n1", "/dev/loop0", "/dev/mmcblk0", "/dev/mmcblk1"]);

        let usb = &drives[0];
        assert_eq!((usb.name.as_str(), usb.bus.as_str(), usb.kind), ("SanDisk Cruzer Blade", "USB", DriveKind::Usb));
        assert!(usb.removable && !usb.system && !usb.read_only);
        assert_eq!(usb.mountpoints, ["/run/media/me/STICK"]);
        assert_eq!(usb.volumes, ["STICK (FAT)"]);
        assert_eq!(usb.unmount, ["/dev/sda1"]);

        let nvme = &drives[1];
        assert!(nvme.system && !nvme.removable);
        assert_eq!(nvme.volumes, ["FAT", "LUKS", "ext4"]);
        assert_eq!(nvme.unmount, ["/dev/nvme0n1p1", "/dev/mapper/root"]);

        let lo = &drives[2];
        assert_eq!((lo.kind, lo.bus.as_str(), lo.removable), (DriveKind::Virtual, "Loop", false));

        let (sd, emmc) = (&drives[3], &drives[4]);
        assert_eq!((sd.kind, sd.name.as_str(), sd.removable), (DriveKind::Sd, "SD card", true));
        assert_eq!((emmc.kind, emmc.bus.as_str(), emmc.name.as_str(), emmc.removable), (DriveKind::Ssd, "eMMC", "eMMC", false));
    }

    #[test]
    fn old_lsblk_has_one_mountpoint_and_numbers_as_strings() {
        let json = r#"{"blockdevices": [{"name":"sdb","type":"disk","size":"8004304896","tran":"usb","rm":"1","ro":"0",
            "mountpoint":null,"children":[{"name":"sdb1","type":"part","size":"8003256320","fstype":"exfat","label":"DATA","mountpoint":"/media/DATA"}]}]}"#;
        let drives = parse(json.as_bytes(), &no_mmc).unwrap();
        assert_eq!(drives.len(), 1);
        let d = &drives[0];
        assert_eq!((d.path.as_str(), d.size, d.removable), ("/dev/sdb", 8_004_304_896, true));
        assert_eq!((d.mountpoints.clone(), d.unmount.clone()), (vec!["/media/DATA".to_owned()], vec!["/dev/sdb1".to_owned()]));
        assert_eq!(d.volumes, ["DATA (exFAT)"]);
    }

    #[test]
    fn bad_output_is_an_error() {
        assert!(parse(b"lsblk: unknown column", &no_mmc).unwrap_err().starts_with("couldn't read lsblk's output"));
        assert!(parse(b"{}", &no_mmc).unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remounts_through_udisksctl() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("dd-gui-remount-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dev = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"").unwrap();
            path.to_string_lossy().into_owned()
        };
        let (good, again, broken) = (dev("sdz1"), dev("sdz2"), dev("sdz3"));
        let log = dir.join("log");
        // A stand-in for udisksctl that logs its arguments and answers like the real one.
        let fake = dir.join("udisksctl");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$4\" in\n  *sdz1) echo \"Mounted $4 at /run/media/me/X\" ;;\n  \
                 *sdz2) echo \"Error mounting $4: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device is already mounted\" >&2; exit 1 ;;\n  \
                 *) echo \"Error mounting $4: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: Error mounting $4 at /run/media/me/Y: wrong fs type\" >&2; exit 1 ;;\nesac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test thread may fork while the script is still open for writing, which
        // makes running it fail with "Text file busy" for a moment.
        for _ in 0..100 {
            match Command::new(&fake).arg("--version").output() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(20)),
                _ => break,
            }
        }
        let _ = std::fs::remove_file(&log);

        let mut drive = parse(LISTING.as_bytes(), &no_mmc).unwrap().remove(0);
        drive.unmount = vec![good.clone(), again.clone(), "/dev/gone".into(), broken.clone()];
        let err = remount_with(&drive, &fake, &|_| false).unwrap_err();
        assert_eq!(err, format!("{broken}: wrong fs type"));
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 3, "{calls}");
        assert!(calls.starts_with(&format!("mount --no-user-interaction --block-device {good}")), "{calls}");

        // Already mounted by the time we look: nothing to do.
        drive.unmount = vec![broken.clone()];
        assert_eq!(remount_with(&drive, &fake, &|_| true), Ok(()));
        let missing = dir.join("no-udisksctl");
        let err = remount_with(&drive, &missing, &|_| false).unwrap_err();
        assert_eq!(err, "udisksctl isn't installed, so it has to be mounted by hand");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_problems_in_plain_words() {
        let already = "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device /dev/sdb1 is already mounted at `/run/media/me/X'.";
        assert_eq!(mount_problem("/dev/sdb1", already), None);
        assert_eq!(mount_problem("/dev/sdb2", "Object /org/freedesktop/UDisks2/block_devices/sdb2 is not a mountable filesystem."), None);
        let denied = "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized to perform operation";
        assert_eq!(mount_problem("/dev/sdb1", denied).unwrap(), "/dev/sdb1: not allowed to mount it");
        let failed = "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: Error mounting /dev/sdb1 at /run/media/me/X: wrong fs type, bad option, bad superblock on /dev/sdb1.";
        assert_eq!(mount_problem("/dev/sdb1", failed).unwrap(), "/dev/sdb1: wrong fs type, bad option, bad superblock on /dev/sdb1");
    }
}
