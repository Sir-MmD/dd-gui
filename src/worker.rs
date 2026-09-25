//! `DD-GUI dd …` runs the bundled uutils dd.
//!
//! The GUI starts this same mode as its worker (elevated when a drive is involved).
//! Worker-only options come right after `dd` and all start with `--ddgui-`. `DD-GUI copy`
//! (see engine/mod.rs) takes the same ones, through [`Plumbing`]:
//!
//! * `--ddgui-worker`            report status to the GUI as `@…` lines on stdout
//! * `--ddgui-watch-stdin`       quit as soon as stdin closes or receives anything. This is
//!   how the GUI cancels, and it means an elevated worker never outlives a crashed GUI.
//!   (`copy --ask` first takes its answer line from stdin.)
//! * `--ddgui-cancel-file=PATH`  quit once PATH exists (macOS, where stdin isn't usable)
//! * `--ddgui-answer-file=PATH`  `copy --ask` only: the answer, once PATH appears
//! * `--ddgui-parent=PID`        quit once process PID is gone
//! * `--ddgui-unmount=DEV`       unmount DEV before copying (repeatable)
//! * `--ddgui-lock=VOLUME`       lock and dismount a volume before copying, and keep it so
//!   until the end (Windows): `X:`, or `\\?\Volume{GUID}` for one without a drive letter.
//!   A volume that no longer exists is skipped.
//! * `--ddgui-sync=DEV`          flush DEV to the hardware after dd succeeds (Windows also
//!   re-reads its partition table then)

use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// The `--ddgui-*` options.
#[derive(Debug, Default)]
pub struct Plumbing {
    pub worker: bool,
    pub watch_stdin: bool,
    pub cancel_file: Option<PathBuf>,
    pub answer_file: Option<PathBuf>,
    pub parent: Option<u32>,
    pub unmount: Vec<String>,
    pub lock: Vec<String>,
    pub sync: Option<PathBuf>,
}

/// Whatever keeps drives unmounted or locked; drop it only when done.
pub type Held = Vec<Box<dyn std::any::Any>>;

impl Plumbing {
    /// Takes one `--ddgui-…` option.
    pub fn parse(&mut self, arg: &str) -> Result<(), String> {
        let (name, value) = arg.split_once('=').unwrap_or((arg, ""));
        match name {
            "--ddgui-worker" => self.worker = true,
            "--ddgui-watch-stdin" => self.watch_stdin = true,
            "--ddgui-cancel-file" => self.cancel_file = Some(value.into()),
            "--ddgui-answer-file" => self.answer_file = Some(value.into()),
            "--ddgui-parent" => self.parent = value.parse().ok(),
            "--ddgui-unmount" => self.unmount.push(value.to_owned()),
            "--ddgui-lock" => self.lock.push(value.to_owned()),
            "--ddgui-sync" => self.sync = Some(value.into()),
            _ => return Err(format!("unknown option {name}")),
        }
        Ok(())
    }

    /// Starts the watchdogs that cancel the job (see the options above).
    ///
    /// With `ask`, the answer for `copy --ask` arrives on the returned channel: the first
    /// line on stdin, or the answer file's first line. Stdin closing before that line
    /// cancels; with `--ddgui-watch-stdin`, so does anything that comes after it.
    pub fn watch(&self, ask: bool) -> Option<Receiver<String>> {
        let (tx, rx) = mpsc::channel();
        let answer_on_stdin = ask && (self.watch_stdin || self.answer_file.is_none());
        if self.watch_stdin || answer_on_stdin {
            let watch = self.watch_stdin;
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin().lock();
                if answer_on_stdin {
                    let mut line = String::new();
                    match stdin.read_line(&mut line) {
                        Ok(_) if line.ends_with('\n') => {
                            let _ = tx.send(line.trim().to_owned());
                        }
                        // Closed (or unreadable) before a whole line came.
                        _ => cancel_now(),
                    }
                    if !watch {
                        return;
                    }
                }
                // EOF, an error, or any byte at all means "stop".
                let mut buf = [0u8; 64];
                let _ = stdin.read(&mut buf);
                cancel_now();
            });
        }

        let cancel_file = self.cancel_file.clone();
        let parent = self.parent;
        let mut answer_file = if ask { self.answer_file.clone() } else { None };
        if cancel_file.is_some() || parent.is_some() || answer_file.is_some() {
            std::thread::spawn(move || {
                let mut seen = None;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let cancelled = cancel_file.as_ref().is_some_and(|f| f.exists());
                    if cancelled || parent.is_some_and(|pid| !process_alive(pid)) {
                        cancel_now();
                    }
                    if let Some(path) = &answer_file
                        && let Ok(text) = std::fs::read_to_string(path)
                    {
                        // Taken once it holds a whole line, or stops changing.
                        if text.contains('\n')
                            || (!text.trim().is_empty() && seen.as_ref() == Some(&text))
                        {
                            let _ =
                                tx.send(text.lines().next().unwrap_or_default().trim().to_owned());
                            answer_file = None;
                        } else {
                            seen = Some(text);
                        }
                    }
                }
            });
        }
        ask.then_some(rx)
    }

    /// Unmounts and locks what the options name.
    pub fn prepare(&self) -> Result<Held, String> {
        for dev in &self.unmount {
            unmount(dev)?;
        }
        #[allow(unused_mut)]
        let mut held: Held = Vec::new();
        #[cfg(windows)]
        for volume in &self.lock {
            if let Some(lock) = win::lock_and_dismount(volume)? {
                held.push(Box::new(lock));
            }
        }
        #[cfg(not(windows))]
        if !self.lock.is_empty() {
            return Err("volume locking is only available on Windows".into());
        }
        Ok(held)
    }

    /// Flushes the `--ddgui-sync` drive, if any.
    pub fn flush(&self) -> Result<(), String> {
        let Some(dev) = &self.sync else { return Ok(()) };
        // Windows' FlushFileBuffers needs write access (this never creates or truncates).
        let _drive = std::fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(dev)
            .and_then(|f| sync(&f).map(|()| f))
            .map_err(|err| {
                format!(
                    "couldn't flush {}: {}",
                    dev.display(),
                    crate::engine::why(&err)
                )
            })?;
        // Windows only reads a drive's partition table now and then: have it read the new
        // one, so the volumes on it show up once our locks go (as Rufus does).
        #[cfg(windows)]
        win::update_properties(&_drive);
        Ok(())
    }
}

/// Flushes what was written to a drive or file all the way to the hardware. (fsync on a
/// Linux block device also flushes the drive's own write cache; FlushFileBuffers on a
/// Windows drive does too.)
pub fn sync(file: &std::fs::File) -> std::io::Result<()> {
    let result = file.sync_all();
    #[cfg(target_os = "macos")]
    if let Err(err) = &result
        && matches!(
            err.raw_os_error(),
            Some(libc::ENOTTY | libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL)
        )
    {
        // sync_all() is F_FULLFSYNC, which file systems implement but raw devices
        // (/dev/rdiskN) don't. Then: plain fsync, and ask the drive to empty its cache.
        // (Real I/O errors still count.)
        use std::os::unix::io::AsRawFd;
        const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416; // _IO('d', 22)
        // SAFETY: calls on our own descriptor; this ioctl takes no argument.
        unsafe {
            if libc::fsync(file.as_raw_fd()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Files (on file systems without F_FULLFSYNC) say ENOTTY here; that's fine.
            libc::ioctl(file.as_raw_fd(), DKIOCSYNCHRONIZECACHE);
        }
        return Ok(());
    }
    result
}

pub fn main() -> i32 {
    #[cfg(windows)]
    attach_console();

    // Makes uucore call itself "dd" (taken from argv[1]) instead of "DD-GUI".
    uucore::set_utility_is_second_arg();

    let mut rest = std::env::args_os().skip(2).peekable();
    let mut plumbing = Plumbing::default();
    while let Some(arg) = rest
        .peek()
        .and_then(|a| a.to_str())
        .filter(|a| a.starts_with("--ddgui-"))
    {
        let arg = arg.to_owned();
        rest.next();
        if let Err(msg) = plumbing.parse(&arg) {
            eprintln!("dd-gui: {msg}");
            return 1;
        }
    }
    let dd_args: Vec<OsString> = std::iter::once(OsString::from("dd")).chain(rest).collect();

    // Held until exit: on Windows, dropping a volume lock would let the OS remount it.
    let mut _held = Held::new();
    if plumbing.worker {
        plain_english();
    }
    plumbing.watch(false);
    if plumbing.worker {
        status("ready");
        match plumbing.prepare() {
            Ok(held) => _held = held,
            Err(msg) => {
                eprintln!("dd-gui: {msg}");
                return 1;
            }
        }
        status("copy");
    }

    uucore::panic::mute_sigpipe_panic();
    if let Err(err) = uucore::locale::setup_localization("dd") {
        eprintln!("dd-gui: could not load dd's messages: {err}");
        return 99;
    }
    let code = uu_dd::uumain(dd_args.into_iter());
    let _ = std::io::stdout().flush();
    if code == 0 && plumbing.sync.is_some() {
        if plumbing.worker {
            status("sync");
        }
        if let Err(msg) = plumbing.flush() {
            eprintln!("dd-gui: {msg}");
            return 1;
        }
    }
    code
}

/// Plain English messages, so the GUI can read dd's progress lines.
/// Call it before starting any threads.
pub fn plain_english() {
    for key in ["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"] {
        // SAFETY: callers run this before any other threads exist.
        unsafe { std::env::remove_var(key) };
    }
}

/// One status line for the GUI, e.g. `@ready`.
pub fn status(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "@{line}");
    let _ = out.flush();
}

/// A child process that has to go down with us (the dd that `copy --ask` runs).
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

pub fn cancel_now() -> ! {
    if let Some(child) = CHILD.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    eprintln!("\ndd-gui: cancelled");
    std::process::exit(130);
}

/// Runs `cmd` to the end. Cancelling kills it too.
pub fn run_child(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    // Spawned under the lock, so a cancel can't slip in before we know the child.
    *CHILD.lock().unwrap_or_else(|e| e.into_inner()) = Some(cmd.spawn()?);
    loop {
        {
            let mut guard = CHILD.lock().unwrap_or_else(|e| e.into_inner());
            let child = guard.as_mut().expect("the child is stored");
            if let Some(status) = child.try_wait()? {
                *guard = None;
                return Ok(status);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Gone, unless we just may not look at it.
            return std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_ACCESS_DENIED as i32);
        }
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn unmount(dev: &str) -> Result<(), String> {
    let out = std::process::Command::new("umount")
        .args(["--all-targets", dev])
        .output()
        .map_err(|e| format!("couldn't run umount: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || err.contains("not mounted") {
        return Ok(());
    }
    let reason = err
        .trim()
        .rsplit(": ")
        .next()
        .unwrap_or("unknown error")
        .to_owned();
    Err(format!(
        "couldn't unmount {dev} ({reason}). Close any apps using the drive and try again."
    ))
}

#[cfg(target_os = "macos")]
fn unmount(dev: &str) -> Result<(), String> {
    let out = std::process::Command::new("diskutil")
        .args(["unmountDisk", "force", dev])
        .output()
        .map_err(|e| format!("couldn't run diskutil: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(format!("couldn't unmount {dev}: {text}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unmount(dev: &str) -> Result<(), String> {
    Err(format!("unmounting {dev} isn't supported here"))
}

#[cfg(windows)]
pub use win::attach_parent_console as attach_console;

#[cfg(windows)]
mod win {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_WRITE_PROTECT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_DISK_UPDATE_PROPERTIES,
    };

    /// The GUI build has no console; when run as `DD-GUI dd …` from a terminal,
    /// borrow the terminal's console so dd's output shows up.
    pub fn attach_parent_console() {
        // SAFETY: plain Win32 calls without pointers.
        unsafe {
            let handle = GetStdHandle(STD_ERROR_HANDLE);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }

    /// Keeps a locked, dismounted volume that way until dropped.
    pub struct VolumeLock(HANDLE);

    impl Drop for VolumeLock {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn ioctl(handle: HANDLE, code: u32) -> bool {
        let mut returned = 0u32;
        // SAFETY: no input or output buffers for these control codes.
        unsafe {
            DeviceIoControl(
                handle,
                code,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        }
    }

    /// Windows refuses raw writes over a mounted volume, so lock and dismount it first.
    /// The lock holds until dropped; Windows would mount the volume again after that. A
    /// volume that's gone already (the drive letter is free) needs nothing: None.
    pub fn lock_and_dismount(volume: &str) -> Result<Option<VolumeLock>, String> {
        let why = |err: std::io::Error| crate::engine::why(&err);
        // "E:", or a volume's own path (`\\?\Volume{…}`, for volumes without a letter).
        // Without a trailing backslash, that opens the volume rather than its root folder.
        let name = volume.trim_end_matches('\\');
        let path = if name.starts_with(r"\\") {
            name.to_owned()
        } else {
            format!(r"\\.\{name}")
        };
        let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let open = |access| {
            // SAFETY: `path` is NUL-terminated and outlives the call.
            unsafe {
                CreateFileW(
                    path.as_ptr(),
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            }
        };
        let mut handle = open(GENERIC_READ | GENERIC_WRITE);
        // Write-protected media (a locked SD card being backed up, say) only open for
        // reading, which is all a lock needs.
        if handle == INVALID_HANDLE_VALUE {
            let code = std::io::Error::last_os_error().raw_os_error();
            if code == Some(ERROR_WRITE_PROTECT as i32) || code == Some(ERROR_ACCESS_DENIED as i32)
            {
                handle = open(GENERIC_READ);
            }
        }
        if handle == INVALID_HANDLE_VALUE {
            let err = std::io::Error::last_os_error();
            let gone = [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND];
            if gone
                .iter()
                .any(|&code| err.raw_os_error() == Some(code as i32))
            {
                return Ok(None);
            }
            return Err(format!("couldn't open volume {name} ({})", why(err)));
        }
        let lock = VolumeLock(handle);
        // Another program may be holding files open for a moment, so retry a bit.
        let mut failure = None;
        for _ in 0..20 {
            if ioctl(handle, FSCTL_LOCK_VOLUME) {
                failure = None;
                break;
            }
            failure = Some(std::io::Error::last_os_error());
            std::thread::sleep(Duration::from_millis(250));
        }
        if let Some(err) = failure {
            return Err(format!(
                "couldn't lock volume {name} ({}). Close any programs using it and try again.",
                why(err)
            ));
        }
        if !ioctl(handle, FSCTL_DISMOUNT_VOLUME) {
            return Err(format!(
                "couldn't dismount volume {name} ({})",
                why(std::io::Error::last_os_error())
            ));
        }
        Ok(Some(lock))
    }

    /// Has Windows read the drive's partition table again. Best effort: a drive that
    /// doesn't take it keeps the old view until it's plugged in again.
    pub fn update_properties(drive: &std::fs::File) {
        use std::os::windows::io::AsRawHandle;
        ioctl(
            drive.as_raw_handle() as HANDLE,
            IOCTL_DISK_UPDATE_PROPERTIES,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plumbing() {
        let mut p = Plumbing::default();
        for arg in [
            "--ddgui-worker",
            "--ddgui-answer-file=/tmp/a",
            "--ddgui-unmount=/dev/sdb1",
            "--ddgui-parent=42",
        ] {
            p.parse(arg).unwrap();
        }
        assert!(p.worker && !p.watch_stdin);
        assert_eq!(p.answer_file, Some(PathBuf::from("/tmp/a")));
        assert_eq!(p.unmount, ["/dev/sdb1"]);
        assert_eq!(p.parent, Some(42));
        assert!(p.parse("--ddgui-nope").is_err());
    }
}
