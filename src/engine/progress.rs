//! Progress: `@progress DONE WRITTEN` lines for the GUI about every half second, or a
//! plain line on stderr when run from a terminal.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Progress {
    /// In the units of `@total`.
    pub done: AtomicU64,
    /// Bytes written to the target.
    pub written: AtomicU64,
}

impl Progress {
    pub fn add(&self, done: u64, written: u64) {
        self.done.fetch_add(done, Relaxed);
        self.written.fetch_add(written, Relaxed);
    }
}

/// Prints progress until dropped, and once more then.
pub struct Reporter {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Reporter {
    /// `total` is what `done` counts toward, when that's known (DONE never goes past it).
    pub fn start(progress: Arc<Progress>, total: Option<u64>, worker: bool) -> Reporter {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            loop {
                let last = stopped.recv_timeout(Duration::from_millis(500))
                    != Err(RecvTimeoutError::Timeout);
                let done = progress.done.load(Relaxed);
                let done = total.map_or(done, |total| done.min(total));
                let written = progress.written.load(Relaxed);
                if worker {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "@progress {done} {written}");
                    let _ = out.flush();
                } else {
                    let percent = match total {
                        Some(total) => {
                            format!("{:>3}% · ", (done * 100).checked_div(total).unwrap_or(100))
                        }
                        None => String::new(),
                    };
                    let rate = written as f64 / started.elapsed().as_secs_f64().max(0.001);
                    let end = if last { "\n" } else { "" };
                    eprint!(
                        "\r{percent}{} written · {}    {end}",
                        crate::fmt::bytes(written),
                        crate::fmt::speed(rate)
                    );
                }
                if last {
                    break;
                }
            }
        });
        Reporter {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
