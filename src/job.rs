//! Runs a job in a worker process (elevated when needed) and reports what it's doing.
//!
//! The worker is this same executable: `DD-GUI dd …` for dd itself, or
//! `DD-GUI copy …` for smart copies and image restores (see `engine`).

use std::io::Read;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::thread;

pub enum Event {
    /// The worker is running, so any password prompt is done.
    Started,
    /// The drive's layout (`smart::Layout` as JSON), sent before asking how to copy.
    Layout(String),
    /// Drives are unmounted and copying starts.
    Copying,
    /// What the progress counts toward (from DD-GUI's copier; dd doesn't say).
    Total(u64),
    /// How far along: `done` counts toward the total, `written` is bytes written
    /// when that differs (compressed images). dd's own lines only have `done`.
    Progress { done: u64, written: Option<u64> },
    /// Copying is done; the target is being flushed.
    Syncing,
    Finished(Outcome),
}

pub enum Outcome {
    Success,
    Cancelled,
    /// The password prompt was dismissed, or authorization failed.
    NotAuthorized(String),
    Failed(String),
}

pub enum Work {
    /// `dd-gui dd` with these operands.
    Dd(Vec<String>),
    /// `dd-gui copy`: `mode` is "smart", "restore" or "zeros". With `ask`, the worker
    /// reports the layout and waits for `Job::answer`; "full" then runs dd with `dd_args`.
    Copy {
        mode: &'static str,
        /// Empty for "zeros".
        from: String,
        to: String,
        ask: bool,
        block_size: Option<u64>,
        /// Bytes to write, for "zeros" into a file.
        size: Option<u64>,
        dd_args: Vec<String>,
    },
}

impl Work {
    #[cfg(target_os = "macos")]
    fn asks(&self) -> bool {
        matches!(self, Work::Copy { ask: true, .. })
    }

    /// The worker's command line after the executable, plumbing options first.
    fn args(&self, plumbing: Vec<String>) -> Vec<String> {
        match self {
            Work::Dd(args) => ["dd".to_owned()].into_iter().chain(plumbing).chain(args.iter().cloned()).collect(),
            Work::Copy { mode, from, to, ask, block_size, size, dd_args } => {
                let mut words = vec!["copy".to_owned()];
                words.extend(plumbing);
                words.push(format!("--mode={mode}"));
                if !from.is_empty() {
                    words.push(format!("--from={from}"));
                }
                words.push(format!("--to={to}"));
                if let Some(size) = size {
                    words.push(format!("--size={size}"));
                }
                if *ask {
                    words.push("--ask".to_owned());
                }
                if let Some(bs) = block_size {
                    words.push(format!("--block-size={bs}"));
                }
                if !dd_args.is_empty() {
                    words.push("--".to_owned());
                    words.extend(dd_args.iter().cloned());
                }
                words
            }
        }
    }
}

pub struct Spec {
    pub work: Work,
    pub elevate: bool,
    pub unmount: Vec<String>,
    pub lock: Vec<String>,
    /// Flush this drive after dd finishes (see `Plan::sync_after`).
    pub sync: Option<String>,
}

type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct Shared {
    cancelled: AtomicBool,
    started: AtomicBool,
    /// dd's last few non-progress lines, for error messages.
    messages: Mutex<Vec<String>>,
}

pub struct Job {
    shared: Arc<Shared>,
    stop: Stop,
}

impl Spec {
    fn plumbing(&self) -> Vec<String> {
        let mut words = vec!["--ddgui-worker".to_owned()];
        words.extend(self.unmount.iter().map(|d| format!("--ddgui-unmount={d}")));
        words.extend(self.lock.iter().map(|v| format!("--ddgui-lock={v}")));
        words.extend(self.sync.iter().map(|d| format!("--ddgui-sync={d}")));
        words
    }
}

enum Stop {
    /// Closing the worker's stdin makes it quit (see `--ddgui-watch-stdin`).
    Stdin { stdin: Mutex<Option<ChildStdin>>, pid: u32 },
    /// The worker polls for these files (see `--ddgui-cancel-file` and `--ddgui-answer-file`).
    #[cfg(target_os = "macos")]
    File { cancel: std::path::PathBuf, answer: std::path::PathBuf, pid: u32 },
}

impl Job {
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, SeqCst);
        let started = self.shared.started.load(SeqCst);
        match &self.stop {
            Stop::Stdin { stdin, pid } => {
                drop(stdin.lock().unwrap().take());
                // Still at the password prompt: the launcher runs as us, so stop it directly.
                if !started {
                    terminate(*pid);
                }
            }
            #[cfg(target_os = "macos")]
            Stop::File { cancel, pid, .. } => {
                let _ = std::fs::File::create(cancel);
                if !started {
                    terminate(*pid);
                }
            }
        }
    }

    /// Answers a worker that's waiting after `Event::Layout`: "smart" or "full".
    pub fn answer(&self, choice: &str) {
        match &self.stop {
            Stop::Stdin { stdin, .. } => {
                use std::io::Write;
                if let Some(stdin) = stdin.lock().unwrap().as_mut() {
                    let _ = writeln!(stdin, "{choice}").and_then(|()| stdin.flush());
                }
            }
            #[cfg(target_os = "macos")]
            Stop::File { answer, .. } => {
                // Write under another name first so the worker never reads half a line.
                let partial = answer.with_extension("partial");
                if std::fs::write(&partial, format!("{choice}\n")).is_ok() {
                    let _ = std::fs::rename(&partial, answer);
                }
            }
        }
    }
}

impl Drop for Job {
    /// A dropped job must not leave a worker writing a drive. On Linux and Windows the
    /// closed stdin does that on its own; the macOS worker needs its cancel file.
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Stop::File { cancel, .. } = &self.stop {
            let _ = std::fs::File::create(cancel);
        }
    }
}

#[cfg(unix)]
fn terminate(pid: u32) {
    // SAFETY: sending a signal has no memory-safety preconditions.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {}

pub fn start(spec: Spec, sink: impl Fn(Event) + Send + Sync + 'static) -> Result<Job, String> {
    let sink: Sink = Arc::new(sink);
    let exe = own_executable()?;
    #[cfg(target_os = "macos")]
    if spec.elevate {
        return macos::start(&exe, &spec, sink);
    }
    start_piped(&exe, &spec, sink)
}

fn own_executable() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find the DD-GUI executable: {e}"))?;
    // Linux reports "<path> (deleted)" once the binary was replaced while running,
    // e.g. by an upgrade; the new binary at the same path works just as well.
    if !exe.exists()
        && let Some(path) = exe.to_str().and_then(|p| p.strip_suffix(" (deleted)"))
        && Path::new(path).exists()
    {
        return Ok(path.into());
    }
    Ok(exe)
}

fn start_piped(exe: &Path, spec: &Spec, sink: Sink) -> Result<Job, String> {
    let mut cmd = launcher(exe, spec.elevate);
    let mut plumbing = spec.plumbing();
    plumbing.push("--ddgui-watch-stdin".to_owned());
    cmd.args(spec.work.args(plumbing));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound if spec.elevate => {
            "Drives need administrator rights, but pkexec (polkit) isn't installed. \
             Install polkit, or run dd-gui as root."
                .to_owned()
        }
        _ => format!("couldn't start the copy: {e}"),
    })?;

    let shared = Arc::new(Shared::default());
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stop = Stop::Stdin { stdin: Mutex::new(child.stdin.take()), pid: child.id() };

    let status_reader = {
        let (shared, sink) = (shared.clone(), sink.clone());
        read_lines(stdout, move |line| on_status_line(line, &shared, &sink))
    };
    let output_reader = {
        let (shared, sink) = (shared.clone(), sink.clone());
        read_lines(stderr, move |line| on_dd_line(line, &shared, &sink))
    };
    let elevated = spec.elevate;
    let waiter_shared = shared.clone();
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = status_reader.join();
        let _ = output_reader.join();
        sink(Event::Finished(outcome(code, &waiter_shared, elevated)));
    });
    Ok(Job { shared, stop })
}

/// The command that starts our own executable, elevated if asked.
fn launcher(exe: &Path, elevate: bool) -> Command {
    if !elevate || cfg!(windows) {
        return Command::new(exe);
    }
    // For setups without polkit: e.g. DD_GUI_ELEVATE="sudo -A" or "doas".
    if let Some(custom) = std::env::var("DD_GUI_ELEVATE").ok().filter(|s| !s.trim().is_empty()) {
        let mut parts = custom.split_whitespace();
        let mut cmd = Command::new(parts.next().unwrap_or("pkexec"));
        cmd.args(parts).arg(exe);
        return cmd;
    }
    let mut cmd = Command::new("pkexec");
    cmd.arg(exe);
    cmd
}

fn outcome(code: Option<i32>, shared: &Shared, elevated: bool) -> Outcome {
    if shared.cancelled.load(SeqCst) {
        return Outcome::Cancelled;
    }
    if code == Some(0) {
        return Outcome::Success;
    }
    let messages = shared.messages.lock().unwrap().clone();
    if elevated && !shared.started.load(SeqCst) {
        // pkexec exits with 126 when the prompt is dismissed, 127 when authorization fails.
        let last = messages.last().map(|m| m.trim_start_matches("Error executing command as another user: "));
        return Outcome::NotAuthorized(match (code, last) {
            (Some(126), _) => "The password prompt was dismissed.".to_owned(),
            (_, Some(text)) if text.contains("dismissed") => "The password prompt was dismissed.".to_owned(),
            (_, Some(text)) if !text.is_empty() => format!("Authorization failed: {text}"),
            _ => "Authorization failed.".to_owned(),
        });
    }
    let detail: Vec<_> = messages.iter().filter(|m| !m.ends_with(": cancelled")).cloned().collect();
    let detail = detail[detail.len().saturating_sub(3)..].join("\n");
    Outcome::Failed(if detail.is_empty() {
        match code {
            Some(code) => format!("dd stopped with exit code {code}."),
            None => "dd was stopped by a signal.".to_owned(),
        }
    } else {
        detail
    })
}

fn on_status_line(line: &str, shared: &Shared, sink: &Sink) {
    let line = line.trim();
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let mut numbers = rest.split_whitespace().map(|n| n.parse::<u64>().ok());
    match word {
        "@ready" => {
            shared.started.store(true, SeqCst);
            sink(Event::Started);
        }
        "@layout" => sink(Event::Layout(rest.to_owned())),
        "@copy" => sink(Event::Copying),
        "@total" => {
            if let Some(Some(total)) = numbers.next() {
                sink(Event::Total(total));
            }
        }
        "@progress" => {
            if let Some(Some(done)) = numbers.next() {
                sink(Event::Progress { done, written: numbers.next().flatten() });
            }
        }
        "@sync" => sink(Event::Syncing),
        _ => {}
    }
}

fn on_dd_line(line: &str, shared: &Shared, sink: &Sink) {
    let line = line.trim();
    if line.is_empty() || is_record_count(line) {
        return;
    }
    if let Some(bytes) = progress_bytes(line) {
        sink(Event::Progress { done: bytes, written: None });
        return;
    }
    let mut messages = shared.messages.lock().unwrap();
    messages.push(line.to_owned());
    if messages.len() > 20 {
        messages.remove(0);
    }
}

/// "5000000000 bytes (5.0 GB, 4.7 GiB) copied, 120 s, 41.7 MB/s" → 5000000000
fn progress_bytes(line: &str) -> Option<u64> {
    let (number, rest) = line.split_once(' ')?;
    let bytes = number.parse().ok()?;
    rest.starts_with("byte").then_some(bytes)
}

/// "1192+1 records in"
fn is_record_count(line: &str) -> bool {
    line.split_once(' ').is_some_and(|(n, rest)| {
        n.contains('+') && n.chars().all(|c| c.is_ascii_digit() || c == '+') && rest.starts_with("records")
    })
}

/// dd rewrites its progress line with `\r`, so both `\r` and `\n` end a line.
#[derive(Default)]
struct Splitter(Vec<u8>);

impl Splitter {
    fn feed(&mut self, data: &[u8], on_line: &mut impl FnMut(&str)) {
        for &b in data {
            if b == b'\r' || b == b'\n' {
                self.flush(on_line);
            } else {
                self.0.push(b);
            }
        }
    }

    fn flush(&mut self, on_line: &mut impl FnMut(&str)) {
        if !self.0.is_empty() {
            on_line(&String::from_utf8_lossy(&self.0));
            self.0.clear();
        }
    }
}

fn read_lines(
    mut source: impl Read + Send + 'static,
    mut on_line: impl FnMut(&str) + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut splitter = Splitter::default();
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = source.read(&mut buf) {
            splitter.feed(&buf[..n], &mut on_line);
        }
        splitter.flush(&mut on_line);
    })
}

/// macOS has no pkexec. `osascript … with administrator privileges` shows the
/// system password prompt, but only hands back output once the command ends, so
/// the worker writes to files that we follow instead.
#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;
    use std::time::Duration;

    pub fn start(exe: &Path, spec: &Spec, sink: Sink) -> Result<Job, String> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("dd-gui-{}-{}", std::process::id(), stamp.as_millis()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create a temporary folder: {e}"))?;
        let status_path = dir.join("status");
        let output_path = dir.join("output");
        let cancel_path = dir.join("cancel");
        let answer_path = dir.join("answer");
        for path in [&status_path, &output_path] {
            File::create(path).map_err(|e| format!("couldn't create {}: {e}", path.display()))?;
        }

        let mut plumbing = spec.plumbing();
        plumbing.push(format!("--ddgui-cancel-file={}", cancel_path.display()));
        if spec.work.asks() {
            plumbing.push(format!("--ddgui-answer-file={}", answer_path.display()));
        }
        plumbing.push(format!("--ddgui-parent={}", std::process::id()));
        let words: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
            .chain(spec.work.args(plumbing))
            .map(|w| quote(&w))
            .collect();
        let shell = format!(
            "{} </dev/null >>{} 2>>{}",
            words.join(" "),
            quote(&status_path.to_string_lossy()),
            quote(&output_path.to_string_lossy())
        );
        let script = format!(
            "do shell script \"{}\" with administrator privileges",
            shell.replace('\\', "\\\\").replace('"', "\\\"")
        );

        let mut child = Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't ask for administrator rights: {e}"))?;

        let shared = Arc::new(Shared::default());
        let done = Arc::new(AtomicBool::new(false));
        let status_tail = {
            let (shared, sink) = (shared.clone(), sink.clone());
            follow(status_path, done.clone(), move |line| on_status_line(line, &shared, &sink))
        };
        let output_tail = {
            let (shared, sink) = (shared.clone(), sink.clone());
            follow(output_path, done.clone(), move |line| on_dd_line(line, &shared, &sink))
        };
        let pid = child.id();
        let waiter_shared = shared.clone();
        thread::spawn(move || {
            let mut osascript_error = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut osascript_error);
            }
            let code = child.wait().ok().and_then(|s| s.code());
            done.store(true, SeqCst);
            let _ = status_tail.join();
            let _ = output_tail.join();
            // A cancelled worker may still be looking for its cancel file; leave it be.
            if !waiter_shared.cancelled.load(SeqCst) {
                let _ = std::fs::remove_dir_all(&dir);
            }

            let started = waiter_shared.started.load(SeqCst);
            let result = if !waiter_shared.cancelled.load(SeqCst) && code != Some(0) && !started {
                // -128 is "User canceled."
                Outcome::NotAuthorized(if osascript_error.contains("(-128)") {
                    "The password prompt was dismissed.".to_owned()
                } else {
                    format!("Authorization failed: {}", osascript_error.trim())
                })
            } else {
                outcome(code, &waiter_shared, false)
            };
            sink(Event::Finished(result));
        });
        Ok(Job { shared, stop: Stop::File { cancel: cancel_path, answer: answer_path, pid } })
    }

    fn follow(
        path: PathBuf,
        done: Arc<AtomicBool>,
        mut on_line: impl FnMut(&str) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let Ok(mut file) = File::open(&path) else { return };
            let mut splitter = Splitter::default();
            let mut buf = [0u8; 8192];
            loop {
                // Check before draining, so nothing written right before exit is missed.
                let finished = done.load(SeqCst);
                while let Ok(n @ 1..) = file.read(&mut buf) {
                    splitter.feed(&buf[..n], &mut on_line);
                }
                if finished {
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
            splitter.flush(&mut on_line);
        })
    }

    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dd_output() {
        assert_eq!(progress_bytes("5000000000 bytes (5.0 GB, 4.7 GiB) copied, 120 s, 41.7 MB/s"), Some(5_000_000_000));
        assert_eq!(progress_bytes("1 byte copied, 0.1 s, 10 B/s"), Some(1));
        assert_eq!(progress_bytes("dd: error writing '/dev/sdb': No space left on device"), None);
        assert!(is_record_count("1192+1 records in"));
        assert!(!is_record_count("dd: 12+0 records"));

        let mut lines = Vec::new();
        let mut splitter = Splitter::default();
        splitter.feed(b"\r1 bytes copied\r2 bytes", &mut |l| lines.push(l.to_owned()));
        splitter.feed(b" copied\n3+0 records in\n", &mut |l| lines.push(l.to_owned()));
        assert_eq!(lines, ["1 bytes copied", "2 bytes copied", "3+0 records in"]);
    }
}
