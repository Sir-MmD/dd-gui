use super::{Drive, DriveKind, tidy};
use serde_json::Value;
use std::process::Command;

const COLUMNS: &str = "NAME,PATH,TYPE,SIZE,TRAN,RM,HOTPLUG,RO,ROTA,MODEL,VENDOR,LABEL,PARTLABEL,FSTYPE";

/// Mounts that mean "this disk runs the system".
const SYSTEM_MOUNTS: [&str; 8] = ["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home", "[SWAP]"];

pub fn list() -> Result<Vec<Drive>, String> {
    // MOUNTPOINTS needs util-linux 2.37+; fall back to MOUNTPOINT on older systems.
    let json = lsblk(&format!("{COLUMNS},MOUNTPOINTS")).or_else(|_| lsblk(&format!("{COLUMNS},MOUNTPOINT")))?;
    let root: Value = serde_json::from_slice(&json).map_err(|e| format!("couldn't read lsblk output: {e}"))?;
    let devices = root["blockdevices"].as_array().cloned().unwrap_or_default();
    Ok(devices.iter().filter_map(drive).collect())
}

fn lsblk(columns: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("lsblk")
        .args(["--json", "--bytes", "--output", columns])
        .output()
        .map_err(|e| format!("couldn't run lsblk: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(out.stdout)
}

fn drive(dev: &Value) -> Option<Drive> {
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

    let path = text(dev, "path").unwrap_or_else(|| format!("/dev/{name}"));
    let tran = text(dev, "tran").unwrap_or_default().to_lowercase();
    let is_sd = name.starts_with("mmcblk") || tran == "mmc";

    let (kind, bus) = if is_loop {
        (DriveKind::Virtual, "Loop")
    } else if tran == "usb" {
        (DriveKind::Usb, "USB")
    } else if is_sd {
        (DriveKind::Sd, "SD")
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
            volumes.push(match label {
                Some(label) => format!("{label} ({fs})"),
                None => fs,
            });
        }
    });
    let system = mounts.iter().any(|m| SYSTEM_MOUNTS.contains(&m.as_str()));
    mounts.retain(|m| m != "[SWAP]");

    let vendor = text(dev, "vendor").map(|v| tidy(&v)).unwrap_or_default();
    let model = text(dev, "model").map(|m| tidy(&m)).unwrap_or_default();
    let name_text = if is_loop {
        "Loop device".to_owned()
    } else if model.is_empty() {
        if is_sd { "SD card".to_owned() } else { name.clone() }
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
        removable: flag(dev, "rm") || flag(dev, "hotplug") || tran == "usb" || is_sd,
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

// lsblk's JSON types changed over the years (strings vs numbers vs booleans).
fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn number(v: &Value, key: &str) -> u64 {
    match &v[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn flag(v: &Value, key: &str) -> bool {
    match &v[key] {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64() == Some(1),
        Value::String(s) => s.trim() == "1",
        _ => false,
    }
}
