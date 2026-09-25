//! Listing the drives dd can read from or write to, and mounting a drive again after a copy.
//!
//! Each OS asks its own tools: lsblk on Linux, diskutil on macOS, the Storage cmdlets of
//! PowerShell on Windows. Those can hang on a failing drive, so every call has a time limit.
//!
//! The parsing of each tool's output is plain code over JSON, so all three are compiled
//! and tested on every OS (`cargo test` runs them everywhere); only running the tools is
//! OS-specific.

use serde_json::Value;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(any(windows, test))]
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
    /// What the worker unmounts before copying: the mounted partitions on Linux; on macOS
    /// the APFS containers stored on the disk, then the disk itself.
    pub unmount: Vec<String>,
    /// Volumes the worker locks and dismounts before copying (Windows): `E:` for volumes
    /// with a letter, `\\?\Volume{GUID}` for the others.
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

/// Mounts again what the worker unmounted before copying from `drive`, as the user, the
/// way the OS would have mounted it: udisks on Linux, `diskutil mountDisk` on macOS. On
/// Windows, volumes come back by themselves once the worker lets go of its locks; this
/// only touches them so they show up again right away.
///
/// Blocks for a while (the OS may check the file systems first), so call it off the UI
/// thread. Volumes that can't be mounted (no file system, encrypted) are skipped
/// quietly; errors are short sentences for the GUI.
pub fn remount(drive: &Drive) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    return platform::remount(drive);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = drive;
        Ok(())
    }
}

/// Sorts "disk10" after "disk2".
fn natural_key(s: &str) -> (String, u64) {
    let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (head, tail) = s.split_at(s.len() - digits);
    (head.to_owned(), tail.parse().unwrap_or(0))
}

/// "SanDisk  Cruzer_Blade" → "SanDisk Cruzer Blade"
fn tidy(s: &str) -> String {
    s.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Runs `cmd` (stdin closed, output collected) and gives up on it after `limit`, killing
/// it: a drive that stops answering can make lsblk, diskutil or PowerShell hang for
/// minutes. `tool` names it in errors.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos", windows)), allow(dead_code))]
fn run(cmd: &mut Command, tool: &str, limit: Duration) -> Result<Output, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{tool} isn't installed"),
            _ => format!("couldn't run {tool}: {e}"),
        })?;
    // Both pipes are drained while waiting, so a chatty tool can't block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut data = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut data);
            }
            let _ = tx.send(data);
        });
        rx
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{tool} didn't answer within {} seconds", limit.as_secs()));
            }
            Err(e) => return Err(format!("couldn't run {tool}: {e}")),
        }
    };
    // Something the tool started could still hold the pipes open; don't wait on that.
    let collect = |rx: mpsc::Receiver<Vec<u8>>| rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    Ok(Output { status, stdout: collect(stdout), stderr: collect(stderr) })
}

/// The first line of a tool's error output that says something, for error messages.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos", windows)), allow(dead_code))]
fn first_line(text: &[u8]) -> String {
    String::from_utf8_lossy(text)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

// The tools' JSON is loose about types (lsblk changed them over the years; PowerShell
// writes whatever the object held), so these accept every spelling seen.

/// A non-empty string.
fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A number, possibly written as a string. 0 when missing.
fn number(v: &Value, key: &str) -> u64 {
    match &v[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A flag: true, 1 or "1" (also "true").
fn flag(v: &Value, key: &str) -> bool {
    match &v[key] {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64() == Some(1),
        Value::String(s) => matches!(s.trim(), "1" | "true" | "True"),
        _ => false,
    }
}

/// A list that may come as an array, a single value, nothing at all, or (Windows
/// PowerShell 5.1 does this to some arrays) an object `{"value": [...], "Count": n}`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn items<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    match &v[key] {
        Value::Array(list) => list.iter().filter(|x| !x.is_null()).collect(),
        Value::Null => Vec::new(),
        Value::Object(map) if map.contains_key("value") && map.contains_key("Count") => items(&v[key], "value"),
        other => vec![other],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_naturally() {
        let mut names = vec!["/dev/disk10", "/dev/disk2", "/dev/disk1"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["/dev/disk1", "/dev/disk2", "/dev/disk10"]);
    }

    #[test]
    fn tidies_names() {
        assert_eq!(tidy("  SanDisk  Cruzer_Blade "), "SanDisk Cruzer Blade");
    }

    #[test]
    fn reads_loose_json() {
        let v = json!({"a": " x ", "b": "", "n": 5, "s": "7", "t": true, "one": 1, "f": "0",
                       "list": ["p", null], "single": "q", "wrapped": {"value": ["r", "s"], "Count": 2}});
        assert_eq!(text(&v, "a").as_deref(), Some("x"));
        assert_eq!(text(&v, "b"), None);
        assert_eq!(text(&v, "missing"), None);
        assert_eq!((number(&v, "n"), number(&v, "s"), number(&v, "a")), (5, 7, 0));
        assert!(flag(&v, "t") && flag(&v, "one") && !flag(&v, "f") && !flag(&v, "missing"));
        assert_eq!(items(&v, "list"), [&json!("p")]);
        assert_eq!(items(&v, "single"), [&json!("q")]);
        assert_eq!(items(&v, "wrapped"), [&json!("r"), &json!("s")]);
        assert!(items(&v, "missing").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn run_collects_output_and_gives_up_in_time() {
        let out = run(Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]), "sh", Duration::from_secs(10)).unwrap();
        assert_eq!((out.stdout.as_slice(), first_line(&out.stderr).as_str(), out.status.code()), (&b"out\n"[..], "err", Some(3)));

        let started = Instant::now();
        let err = run(Command::new("sleep").arg("30"), "sleep", Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("didn't answer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let err = run(&mut Command::new("dd-gui-no-such-tool"), "the tool", Duration::from_secs(1)).unwrap_err();
        assert_eq!(err, "the tool isn't installed");
    }
}
