//! `dd-gui copy …`: DD-GUI's own copier, for what plain dd doesn't do well: smart copies
//! (only the used blocks), compressed images and virtual disks, and zeros where there's no
//! /dev/zero (Windows).
//!
//! ```text
//! dd-gui copy [PLUMBING…] --mode=smart   --from=PATH --to=PATH [--ask] [--block-size=N] [-- DD_OPERANDS…]
//! dd-gui copy [PLUMBING…] --mode=restore --from=PATH --to=PATH [--block-size=N]
//! dd-gui copy [PLUMBING…] --mode=zeros   --to=PATH [--size=N] [--block-size=N]
//! ```
//!
//! PLUMBING are the `--ddgui-*` options shared with `dd-gui dd` (see worker.rs):
//! `--ddgui-worker`, `--ddgui-watch-stdin`, `--ddgui-cancel-file=PATH`,
//! `--ddgui-answer-file=PATH`, `--ddgui-parent=PID`, `--ddgui-unmount=DEV`,
//! `--ddgui-lock=VOLUME`, `--ddgui-sync=DEV`.
//!
//! * `--mode=smart`: `--from` is a drive (or a raw disk image file). Only the extents
//!   from `crate::smart::analyze` are read. If `--to` is a drive (a block or character
//!   device), they are written in place and free space on the target is left alone.
//!   Otherwise `--to` becomes a zstd "smart image" of the whole drive (named `.img.zst`
//!   by the GUI): free space costs next to nothing, a map of the used space comes first
//!   and a seek table last, so DD-GUI restores only the used parts. Plain `zstd -d` still
//!   gives the exact raw image. (Format: see image.rs.)
//! * `--mode=restore`: `--from` is an image, decompressed on the fly and written to `--to`,
//!   a drive or a file: raw; a DD-GUI smart image (zstd, or gzip from older versions),
//!   whose free space is skipped on the target; gzip, xz, zstd or zip (its first file
//!   with anything in it), with a tar inside any of those (its first file); and what
//!   `formats` reads: bzip2, lz4, lzma, 7z, tar, DMG, VHD, VHDX, VMDK and QCOW2. The
//!   format comes from the file's content, not its name. A drive counts as raw.
//! * `--mode=zeros`: writes zeros to `--to` from its start: all of a drive, or `--size`
//!   bytes, which a file needs (it's created, or emptied first). Drives are written
//!   unbuffered (O_DIRECT, raw devices, FILE_FLAG_NO_BUFFERING). No `--from`.
//! * `--ask` (with `--mode=smart` only): after a first analysis (`smart::estimate`, as
//!   file systems may still be mounted), print `@layout <json>` and wait for an answer
//!   line. "smart" continues. "full" runs the bundled dd with DD_OPERANDS instead, sector
//!   by sector, reporting like `dd-gui dd`. Anything else, or EOF, cancels (exit 130).
//!   The answer comes on stdin (with `--ddgui-watch-stdin`, where a later EOF still means
//!   cancel) or through `--ddgui-answer-file`.
//! * `--block-size=N`: bytes per read and write (default 4 MiB; `K`, `M`, `G` suffixes;
//!   a multiple of 4096). Smart images always compress 4 MiB at a time.
//! * `--size=N` (with `--mode=zeros` only): bytes to write (`K`, `M`, `G`, `T` suffixes
//!   work too). Needn't be a multiple of anything.
//!
//! Drives, in every mode, are read and written unbuffered where the OS allows it (O_DIRECT
//! on Linux, /dev/rdiskN on macOS, FILE_FLAG_NO_BUFFERING and FILE_FLAG_WRITE_THROUGH on
//! Windows), so the progress follows the drive. Every transfer is whole logical sectors at
//! sector offsets; a partial sector is read, patched and written back (see disk.rs).
//!
//! Order: `@ready` → [analysis → `@layout` → answer] → unmount and lock (PLUMBING) →
//! `@copy` → smart: analyze again, now unmounted → [`@total`] → `@progress`… → `@sync` →
//! exit 0. `@sync` flushes `--to`, then `--ddgui-sync`'s drive. The last `@progress`
//! comes before it.
//!
//! Lines on stdout with `--ddgui-worker`:
//!
//! | line | meaning |
//! |---|---|
//! | `@ready` | the worker runs (any password prompt is over) |
//! | `@layout {json}` | `smart::Layout` as JSON, only with `--ask` |
//! | `@copy` | drives are unmounted, copying starts |
//! | `@total N` | what DONE counts toward; not sent when that isn't known upfront |
//! | `@progress DONE WRITTEN` | about every 0.5 s: DONE in the units below (never past `@total`), WRITTEN = bytes written to `--to` |
//! | `@sync` | flushing the target |
//!
//! Progress units, for DONE and `@total`:
//!
//! * smart copies, and restores of smart images: used bytes (DONE = bytes of used data
//!   copied; WRITTEN is the image's size so far when writing a smart image);
//! * raw restores, and zeros: bytes (DONE = WRITTEN);
//! * restores of gzip, xz, zstd and zip images: compressed bytes read, out of the
//!   file's size, as the size once decompressed often isn't known upfront;
//! * restores of the formats from `formats` (bzip2, lz4, lzma, 7z, tar, virtual disks):
//!   bytes of the raw disk written (DONE = WRITTEN), out of the raw disk's size. When the
//!   format doesn't tell that size upfront (a .bz2, say), there's no `@total` line: the
//!   GUI can only show the bytes written.
//!
//! Errors: `dd-gui: <message>` lines on stderr and a non-zero exit code.

mod copy;
mod disk;
mod formats;
mod image;
mod progress;
mod restore;
#[cfg(all(test, unix))]
mod tests;

use crate::fmt;
use crate::smart::{self, Extent, Layout};
use crate::worker::{self, Plumbing};
use disk::{Disk, DiskReader};
use image::Map;
use progress::{Progress, Reporter};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

pub fn main() -> i32 {
    #[cfg(windows)]
    worker::attach_console();

    let args = match Args::parse(std::env::args_os().skip(2)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("dd-gui: {msg}");
            return 1;
        }
    };
    if args.plumbing.worker {
        worker::plain_english();
    }
    let answers = args.plumbing.watch(args.ask);
    args.say("ready");
    match run(&args, answers) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("dd-gui: {msg}");
            1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Smart,
    Restore,
    Zeros,
}

#[derive(Debug)]
struct Args {
    plumbing: Plumbing,
    mode: Mode,
    /// Empty for zeros.
    from: PathBuf,
    to: PathBuf,
    ask: bool,
    block_size: usize,
    /// Bytes of zeros.
    size: Option<u64>,
    /// dd's operands, for a full copy after `--ask`.
    dd: Vec<OsString>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
        let mut plumbing = Plumbing::default();
        let (mut mode, mut from, mut to, mut ask, mut block_size, mut size, mut dd) =
            (None, None, None, false, 4 << 20, None, vec![]);
        while let Some(arg) = args.next() {
            if arg == "--" {
                dd = args.collect();
                break;
            }
            // Paths may not be valid Unicode, so split `--name=value` on the raw bytes.
            let bytes = arg.as_encoded_bytes();
            let eq = bytes.iter().position(|&b| b == b'=').unwrap_or(bytes.len());
            let name = String::from_utf8_lossy(&bytes[..eq]).into_owned();
            // SAFETY: splitting right after an ASCII '=' keeps both halves valid.
            let value = bytes
                .get(eq + 1..)
                .map(|v| unsafe { OsStr::from_encoded_bytes_unchecked(v) });
            let text = || value.and_then(OsStr::to_str).unwrap_or_default();
            match name.as_str() {
                "--mode" => {
                    mode = Some(match text() {
                        "smart" => Mode::Smart,
                        "restore" => Mode::Restore,
                        "zeros" => Mode::Zeros,
                        other => {
                            return Err(format!(
                                "unknown mode \"{other}\" (use smart, restore or zeros)"
                            ));
                        }
                    })
                }
                "--from" => from = value.map(PathBuf::from),
                "--to" => to = value.map(PathBuf::from),
                "--ask" => ask = true,
                "--block-size" => {
                    block_size = parse_size(text())
                        .filter(|&n| n % disk::ALIGN as u64 == 0 && (4096..=256 << 20).contains(&n))
                        .ok_or("--block-size has to be a multiple of 4096 between 4K and 256M")?
                        as usize;
                }
                "--size" => {
                    size = Some(parse_size(text()).ok_or("--size has to be a number of bytes")?)
                }
                _ if name.starts_with("--ddgui-") => {
                    plumbing.parse(arg.to_str().ok_or("invalid option")?)?
                }
                _ => return Err(format!("unknown option {name}")),
            }
        }
        let mode = mode.ok_or("--mode is missing")?;
        let from = from.filter(|p| !p.as_os_str().is_empty());
        let from = match mode {
            Mode::Zeros if from.is_some() => {
                return Err("--from doesn't go with --mode=zeros".into());
            }
            Mode::Zeros => PathBuf::new(),
            _ => from.ok_or("--from is missing")?,
        };
        let to = to
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("--to is missing")?;
        if ask && mode != Mode::Smart {
            return Err("--ask only goes with --mode=smart".into());
        }
        if size.is_some() && mode != Mode::Zeros {
            return Err("--size only goes with --mode=zeros".into());
        }
        Ok(Args {
            plumbing,
            mode,
            from,
            to,
            ask,
            block_size,
            size,
            dd,
        })
    }

    /// A status line, for the GUI only.
    fn say(&self, line: &str) {
        if self.plumbing.worker {
            worker::status(line);
        }
    }
}

/// "4194304", "4M", "512K", "1G", "2T" → bytes.
fn parse_size(text: &str) -> Option<u64> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let unit: u64 = match &text[digits..] {
        "" => 1,
        "K" | "k" | "KiB" => 1 << 10,
        "M" | "MiB" => 1 << 20,
        "G" | "GiB" => 1 << 30,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    text[..digits].parse::<u64>().ok()?.checked_mul(unit)
}

fn run(args: &Args, answers: Option<Receiver<String>>) -> Result<i32, String> {
    if args.mode != Mode::Zeros && same_file(&args.from, &args.to) {
        return Err("--from and --to are the same".into());
    }
    if let Some(answers) = answers {
        // A first look, while file systems may still be mounted (only reading).
        let layout = {
            let src = open_source(&args.from)?;
            smart::estimate(&mut DiskReader::new(&src), src.size)
                .map_err(|e| analysis_error(&src, &e))?
        };
        let json = serde_json::to_string(&layout)
            .map_err(|e| format!("couldn't describe the drive: {e}"))?;
        worker::status(&format!("layout {json}"));
        match answers
            .recv()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "smart" => {}
            "full" => return full_copy(args),
            _ => worker::cancel_now(),
        }
    }
    let _held = args.plumbing.prepare()?;
    args.say("copy");
    let target = match args.mode {
        Mode::Smart => smart_copy(args)?,
        Mode::Restore => restore(args)?,
        Mode::Zeros => zeros(args)?,
    };
    args.say("sync");
    target
        .sync()
        .map_err(|e| format!("couldn't flush {}: {}", args.to.display(), why(&e)))?;
    args.plumbing.flush()?;
    Ok(0)
}

fn smart_copy(args: &Args) -> Result<Disk, String> {
    let src = open_source(&args.from)?;
    let layout = analyze(&src)?;
    let map = Map {
        size: src.size,
        extents: tidy(&layout.extents, src.size),
    };
    debug_assert_eq!(
        map.used(),
        layout.used(),
        "the analysis returned overlapping extents"
    );
    let progress = Arc::new(Progress::default());
    if disk::is_drive(&args.to) {
        let dst = Disk::open_drive(&args.to).map_err(|e| open_error(&args.to, &e))?;
        if dst.size < map.end() {
            return Err(format!(
                "{} is too small: the data on {} reaches {}, but the drive holds {}.",
                args.to.display(),
                args.from.display(),
                fmt::bytes(map.end()),
                fmt::bytes(dst.size)
            ));
        }
        args.say(&format!("total {}", map.used()));
        let _reporter = Reporter::start(progress.clone(), Some(map.used()), args.plumbing.worker);
        copy::to_drive(&src, &dst, &map.extents, args.block_size, &progress)?;
        Ok(dst)
    } else {
        let dst = Disk::create(&args.to)
            .map_err(|e| format!("couldn't create {}: {}", args.to.display(), why(&e)))?;
        args.say(&format!("total {}", map.used()));
        let _reporter = Reporter::start(progress.clone(), Some(map.used()), args.plumbing.worker);
        copy::to_image(&src, &map, &dst, &progress)?;
        Ok(dst)
    }
}

fn restore(args: &Args) -> Result<Disk, String> {
    let mut image = restore::inspect(&args.from).map_err(|e| open_error(&args.from, &e))?;
    // Formats from `formats` open now: that may tell the raw disk's size, for the total.
    image.open().map_err(|e| open_error(&args.from, &e))?;
    let dst = if disk::is_drive(&args.to) {
        Disk::open_drive(&args.to).map_err(|e| open_error(&args.to, &e))?
    } else {
        Disk::create(&args.to)
            .map_err(|e| format!("couldn't create {}: {}", args.to.display(), why(&e)))?
    };
    if let Some(needs) = image.needs()
        && dst.is_drive
        && needs > dst.size
    {
        return Err(format!(
            "The image doesn't fit: it needs {}, but {} holds {}.",
            fmt::bytes(needs),
            args.to.display(),
            fmt::bytes(dst.size)
        ));
    }
    let progress = Arc::new(Progress::default());
    let total = image.total();
    if let Some(total) = total {
        args.say(&format!("total {total}"));
    }
    let _reporter = Reporter::start(progress.clone(), total, args.plumbing.worker);
    restore::restore(&mut image, &dst, args.block_size, &progress)?;
    Ok(dst)
}

fn zeros(args: &Args) -> Result<Disk, String> {
    let (dst, size) = if disk::is_drive(&args.to) {
        let dst = Disk::open_drive(&args.to).map_err(|e| open_error(&args.to, &e))?;
        let size = args.size.unwrap_or(dst.size);
        if size > dst.size {
            return Err(format!(
                "{} is too small: it holds {}, not {}.",
                args.to.display(),
                fmt::bytes(dst.size),
                fmt::bytes(size)
            ));
        }
        (dst, size)
    } else {
        let size = args
            .size
            .ok_or("--size is needed to write zeros to a file")?;
        let dst = Disk::create(&args.to)
            .map_err(|e| format!("couldn't create {}: {}", args.to.display(), why(&e)))?;
        (dst, size)
    };
    let progress = Arc::new(Progress::default());
    args.say(&format!("total {size}"));
    let _reporter = Reporter::start(progress.clone(), Some(size), args.plumbing.worker);
    copy::zeros(&dst, size, args.block_size, &progress)?;
    Ok(dst)
}

/// The answer was "full": the bundled dd copies everything, reporting as `dd-gui dd`
/// does (its progress and messages go straight to our stderr).
fn full_copy(args: &Args) -> Result<i32, String> {
    if args.dd.is_empty() {
        return Err("a full copy needs dd's operands after --".into());
    }
    let _held = args.plumbing.prepare()?;
    args.say("copy");
    let mut dd = Command::new(own_executable()?);
    // `--ddgui-parent`: dd quits too if we're killed.
    dd.arg("dd")
        .arg(format!("--ddgui-parent={}", std::process::id()))
        .args(&args.dd)
        .stdin(Stdio::null());
    let status =
        worker::run_child(&mut dd).map_err(|e| format!("couldn't start dd: {}", why(&e)))?;
    let code = status.code().unwrap_or(1);
    if code == 0 && args.plumbing.sync.is_some() {
        args.say("sync");
        args.plumbing.flush()?;
    }
    Ok(code)
}

fn own_executable() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("can't find the DD-GUI executable: {e}"))?;
    // Linux reports "<path> (deleted)" once the binary was replaced while running.
    if !exe.exists()
        && let Some(path) = exe.to_str().and_then(|p| p.strip_suffix(" (deleted)"))
        && Path::new(path).exists()
    {
        return Ok(path.into());
    }
    Ok(exe)
}

fn open_source(path: &Path) -> Result<Disk, String> {
    Disk::open_read(path).map_err(|e| open_error(path, &e))
}

fn analyze(src: &Disk) -> Result<Layout, String> {
    smart::analyze(&mut DiskReader::new(src), src.size).map_err(|e| analysis_error(src, &e))
}

fn analysis_error(src: &Disk, err: &io::Error) -> String {
    format!("couldn't analyze {}: {}", src.path.display(), why(err))
}

fn open_error(path: &Path, err: &io::Error) -> String {
    format!("couldn't open {}: {}", path.display(), why(err))
}

/// An I/O error in plain words: "No space left on device", without "(os error 28)".
pub fn why(err: &io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The extents to copy: sorted, merged and within the drive. (The analysis promises
/// that already; this just makes sure.)
fn tidy(extents: &[Extent], size: u64) -> Vec<Extent> {
    let mut sorted: Vec<Extent> = extents
        .iter()
        .map(|e| Extent {
            start: e.start,
            len: e
                .start
                .saturating_add(e.len)
                .min(size)
                .saturating_sub(e.start),
        })
        .filter(|e| e.len > 0)
        .collect();
    sorted.sort_by_key(|e| e.start);
    let mut merged: Vec<Extent> = Vec::with_capacity(sorted.len());
    for e in sorted {
        match merged.last_mut() {
            Some(last) if e.start <= last.start + last.len => {
                last.len = last.len.max(e.start + e.len - last.start);
            }
            _ => merged.push(e),
        }
    }
    merged
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Raw,
    Gzip,
    Xz,
    Zstd,
    Zip,
    // Handled by `formats`:
    Bzip2,
    Lz4,
    /// Legacy .lzma ("LZMA alone")
    Lzma,
    /// 7-Zip archive: its first file
    SevenZ,
    /// tar archive: its first file (inside any of the compressions above too)
    Tar,
    /// Apple disk image (UDIF)
    Dmg,
    Vhd,
    Vhdx,
    Vmdk,
    Qcow2,
}

/// What a DD-GUI smart image records about the drive it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmartInfo {
    pub disk_size: u64,
    /// Bytes holding data (what a restore writes).
    pub used: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: ImageFormat,
    /// Size of the file itself.
    pub size: u64,
    /// Size once decompressed, when the format tells. For a tar inside gzip, xz, zstd or
    /// zip: the size of the file in the tar that gets restored.
    pub uncompressed: Option<u64>,
    pub smart: Option<SmartInfo>,
}

/// Looks at an image file without reading all of it (fast, no privileges needed).
///
/// `uncompressed` is known for raw images, smart images (their map), xz (its index),
/// zstd (a seek table, or a size in the first frame's header), zip (its first file's
/// entry), a tar inside any of those (its first file's header), and whatever `formats`
/// learns cheaply (virtual disks record it).
pub fn probe(path: &Path) -> io::Result<ImageInfo> {
    restore::inspect(path).map(|image| image.info)
}
