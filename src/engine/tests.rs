//! Tests for the copy engine. No root and no GUI needed: "drives" are files, layouts are
//! built by hand, and the end-to-end tests run the real binary (built on first use).
//!
//! Big sparse files go to `$DDGUI_TEST_SCRATCH` (default: the temp folder). The heavy
//! tests are `#[ignore]`d: `cargo test --release --bin dd-gui engine -- --ignored`.

use super::disk::Disk;
use super::image::{self, Map, Zeros};
use super::progress::Progress;
use super::{Args, ImageFormat, Mode, copy, parse_size, restore};
use crate::smart::Extent;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;

/// A temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        Self::inside(std::env::temp_dir(), tag)
    }

    /// For big (sparse) files.
    fn scratch(tag: &str) -> TempDir {
        let base =
            std::env::var_os("DDGUI_TEST_SCRATCH").map_or_else(std::env::temp_dir, PathBuf::from);
        Self::inside(base, tag)
    }

    fn inside(base: PathBuf, tag: &str) -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Relaxed);
        let dir = base.join(format!("dd-gui-engine-{}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Reproducible pseudo-random bytes (xorshift64*).
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend(x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn ext(start: u64, len: u64) -> Extent {
    Extent { start, len }
}

/// A "drive" file full of junk, with random data (and a run of zeros) in the extents.
fn junk_drive(path: &Path, size: u64, extents: &[Extent]) -> Vec<u8> {
    let mut content: Vec<u8> = (0..size).map(|i| 0x5a ^ (i % 251) as u8).collect();
    for (n, e) in extents.iter().enumerate() {
        let data = noise(n as u64 + 7, e.len as usize);
        content[e.start as usize..(e.start + e.len) as usize].copy_from_slice(&data);
    }
    // Zeros inside used space are data too, and have to survive as such.
    if let Some(e) = extents.iter().find(|e| e.len >= 3 * 4096) {
        content[(e.start + 4096) as usize..(e.start + 8192) as usize].fill(0);
    }
    fs::write(path, &content).unwrap();
    content
}

/// `content` with everything outside the extents set to `fill`.
fn only_extents(content: &[u8], extents: &[Extent], fill: u8) -> Vec<u8> {
    let mut out = vec![fill; content.len()];
    for e in extents {
        let r = e.start as usize..(e.start + e.len) as usize;
        out[r.clone()].copy_from_slice(&content[r]);
    }
    out
}

fn gunzip(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(fs::File::open(path).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn have(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        eprintln!("note: {tool} isn't installed, skipping what needs it");
    }
    found
}

fn sh(cmd: &str) {
    let status = Command::new("sh").args(["-c", cmd]).status().unwrap();
    assert!(status.success(), "failed: {cmd}");
}

fn assert_same(what: &str, got: &[u8], want: &[u8]) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(at) = got.iter().zip(want).position(|(a, b)| a != b) {
        panic!("{what}: first difference at byte {at}");
    }
}

#[test]
fn map_comment_round_trip() {
    let size = 10 << 30;
    let map = Map {
        size,
        extents: vec![ext(0, MIB), ext(5 * MIB, 4096), ext(size - 12288, 12288)],
    };
    let comment = map.to_comment();
    assert!(comment.starts_with(
        "DD-GUI smart image v1\nsize=10737418240 used=1064960 extents=3 unit=4096\nmap="
    ));
    assert_eq!(Map::parse(comment.as_bytes()), Some(Ok(map.clone())));

    let damaged = comment.replace("used=1064960", "used=1064961");
    assert!(matches!(Map::parse(damaged.as_bytes()), Some(Err(_))));
    let truncated = &comment.as_bytes()[..comment.len() - 4];
    assert!(matches!(Map::parse(truncated), Some(Err(_))));
    assert_eq!(Map::parse(b"Created by some other tool"), None);
    assert_eq!(Map::parse(b"DD-GUI smart image v2\nsize=1"), None);

    let empty = Map {
        size: 4096,
        extents: vec![],
    };
    assert_eq!(Map::parse(empty.to_comment().as_bytes()), Some(Ok(empty)));

    // A drive whose size isn't a multiple of 4 KiB, and extents that aren't aligned at all.
    let odd = Map {
        size: size + 1000,
        extents: vec![ext(0, 4096), ext(size - 4096, 5096)],
    };
    assert!(odd.to_comment().contains(" unit=4096\n"));
    assert_eq!(Map::parse(odd.to_comment().as_bytes()), Some(Ok(odd)));
    let unaligned = Map {
        size: 5000,
        extents: vec![ext(100, 900), ext(1000, 1), ext(4999, 1)],
    };
    assert!(unaligned.to_comment().contains(" unit=1\n"));
    assert_eq!(
        Map::parse(unaligned.to_comment().as_bytes()),
        Some(Ok(unaligned))
    );

    // Many extents stay compact: under 3 characters each here.
    let many = Map {
        size: 1 << 40,
        extents: (0..100_000)
            .map(|i| ext(i * 64 * 4096, 4096 * (1 + i % 7)))
            .collect(),
    };
    let text = many.to_comment();
    assert!(text.len() < 100_000 * 3, "{} bytes", text.len());
    assert_eq!(Map::parse(text.as_bytes()), Some(Ok(many)));
}

#[test]
fn zero_members_decompress_to_zeros() {
    for len in [1u64, 4095, 4096, 12288, 64 * MIB + 4096 + 500, 3 * 64 * MIB] {
        let mut out = Vec::new();
        let written = Zeros::default().write(&mut out, len).unwrap();
        assert_eq!(written, out.len() as u64);
        // Each member says how big it is, in the file and unpacked.
        let mut r = BufReader::new(&out[..]);
        let (mut unpacked, mut offset) = (0, 0);
        while !r.fill_buf().unwrap().is_empty() {
            let header = image::read_header(&mut r).unwrap();
            let (csize, usize) = header.sizes.expect("a DG subfield");
            unpacked += usize;
            offset += csize;
            r = BufReader::new(&out[offset as usize..]);
        }
        assert_eq!(unpacked, len);
        let data = {
            let mut v = Vec::new();
            flate2::read::MultiGzDecoder::new(&out[..])
                .read_to_end(&mut v)
                .unwrap();
            v
        };
        assert_eq!(data.len() as u64, len);
        assert!(data.iter().all(|&b| b == 0));
    }
    // 64 MiB of zeros takes about 64 KiB.
    let mut out = Vec::new();
    Zeros::default().write(&mut out, 64 * MIB).unwrap();
    assert!(out.len() < 70_000, "{} bytes", out.len());
}

#[test]
fn parses_options() {
    let args = |words: &[&str]| Args::parse(words.iter().map(OsString::from));
    let a = args(&[
        "--ddgui-worker",
        "--ddgui-sync=/dev/sdz",
        "--mode=smart",
        "--from=/dev/sdy",
        "--to=/tmp/x.img.gz",
        "--ask",
        "--block-size=1048576",
        "--",
        "if=/dev/sdy",
        "of=/tmp/x.img",
    ])
    .unwrap();
    assert_eq!(a.mode, Mode::Smart);
    assert!(a.ask && a.plumbing.worker);
    assert_eq!(a.plumbing.sync, Some(PathBuf::from("/dev/sdz")));
    assert_eq!(a.block_size, 1 << 20);
    assert_eq!(a.dd, ["if=/dev/sdy", "of=/tmp/x.img"]);

    let a = args(&["--mode=restore", "--from=a b.xz", "--to=/dev/sdz"]).unwrap();
    assert_eq!(
        (a.mode, a.block_size, a.from),
        (Mode::Restore, 4 << 20, PathBuf::from("a b.xz"))
    );
    assert!(args(&["--mode=restore", "--from=a", "--to=b", "--ask"]).is_err());
    assert!(args(&["--mode=copy", "--from=a", "--to=b"]).is_err());
    assert!(args(&["--mode=smart", "--from=a"]).is_err());
    assert!(args(&["--mode=smart", "--from=a", "--to=b", "--block-size=1000"]).is_err());
    assert!(args(&["--mode=smart", "--from=a", "--to=b", "--nope"]).is_err());
    assert_eq!(parse_size("4M"), Some(4 << 20));
    assert_eq!(parse_size("512K"), Some(512 << 10));
    assert_eq!(parse_size("64MiB"), Some(64 << 20));
    assert_eq!(parse_size("4X"), None);
}

/// A 64 MiB (and a bit) drive whose used space starts, ends and crosses chunks oddly.
fn test_layout() -> (u64, Vec<Extent>) {
    let size = 64 * MIB + 1536;
    let extents = vec![
        ext(0, MIB),
        ext(3 * MIB + 4096, 8192),
        ext(8 * MIB, 9 * MIB + 4096),
        ext(40 * MIB, 4096),
        ext(size - 5632, 5632),
    ];
    (size, extents)
}

#[test]
fn smart_image_round_trip() {
    let dir = TempDir::new("image");
    let (size, extents) = test_layout();
    let content = junk_drive(&dir.path("drive"), size, &extents);
    let map = Map {
        size,
        extents: extents.clone(),
    };
    let src = Disk::open_read(&dir.path("drive")).unwrap();

    let progress = Progress::default();
    let out = Disk::create(&dir.path("smart.img.gz")).unwrap();
    copy::to_image(&src, &map, &out, &progress).unwrap();
    let file_size = fs::metadata(dir.path("smart.img.gz")).unwrap().len();
    assert_eq!(progress.done.load(Relaxed), map.used());
    assert_eq!(progress.written.load(Relaxed), file_size);

    // (a) Any gunzip gives the whole drive: the used data, zeros elsewhere.
    let expected = only_extents(&content, &extents, 0);
    assert_same(
        "MultiGzDecoder",
        &gunzip(&dir.path("smart.img.gz")),
        &expected,
    );
    if have("gzip") {
        let out = Command::new("gzip")
            .arg("-dc")
            .arg(dir.path("smart.img.gz"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "gzip: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_same("gzip -dc", &out.stdout, &expected);
    }

    // probe() reads the map from the header.
    let info = super::probe(&dir.path("smart.img.gz")).unwrap();
    assert_eq!(info.format, ImageFormat::Gzip);
    assert_eq!(info.uncompressed, Some(size));
    assert_eq!(
        info.smart,
        Some(super::SmartInfo {
            disk_size: size,
            used: map.used()
        })
    );

    // (b) Restored onto a drive, free space keeps what was there.
    fs::write(dir.path("target"), vec![0xaa; size as usize]).unwrap();
    let image = restore::inspect(&dir.path("smart.img.gz")).unwrap();
    assert_eq!(image.map.as_ref(), Some(&map));
    let target = Disk::open_drive(&dir.path("target")).unwrap();
    let progress = Progress::default();
    restore::restore(&image, &target, 4 << 20, &progress).unwrap();
    assert_eq!(progress.done.load(Relaxed), map.used());
    assert_same(
        "restored onto 0xAA",
        &fs::read(dir.path("target")).unwrap(),
        &only_extents(&content, &extents, 0xaa),
    );

    // Restored into a new file: the complete image.
    let file = Disk::create(&dir.path("restored.img")).unwrap();
    restore::restore(&image, &file, 1 << 20, &Progress::default()).unwrap();
    assert_same(
        "restored into a file",
        &fs::read(dir.path("restored.img")).unwrap(),
        &expected,
    );
}

#[test]
fn damaged_smart_image_is_refused() {
    let dir = TempDir::new("damaged");
    let (size, extents) = test_layout();
    junk_drive(&dir.path("drive"), size, &extents);
    let map = Map { size, extents };
    let src = Disk::open_read(&dir.path("drive")).unwrap();
    copy::to_image(
        &src,
        &map,
        &Disk::create(&dir.path("a.gz")).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    let mut bytes = fs::read(dir.path("a.gz")).unwrap();

    // Cut short: a member of zeros at the end is gone.
    fs::write(dir.path("short.gz"), &bytes[..bytes.len() - 200]).unwrap();
    let image = restore::inspect(&dir.path("short.gz")).unwrap();
    let err = restore::restore(
        &image,
        &Disk::create(&dir.path("x")).unwrap(),
        4 << 20,
        &Progress::default(),
    );
    assert!(err.unwrap_err().contains("damaged"));

    // A flipped byte in the data.
    let at = bytes.len() / 3;
    bytes[at] ^= 0x40;
    fs::write(dir.path("flipped.gz"), &bytes).unwrap();
    let image = restore::inspect(&dir.path("flipped.gz")).unwrap();
    let err = restore::restore(
        &image,
        &Disk::create(&dir.path("y")).unwrap(),
        4 << 20,
        &Progress::default(),
    );
    assert!(err.is_err());
}

#[test]
fn smart_copy_to_drive_leaves_free_space() {
    let dir = TempDir::new("todrive");
    let (size, extents) = test_layout();
    let content = junk_drive(&dir.path("drive"), size, &extents);
    let src = Disk::open_read(&dir.path("drive")).unwrap();
    fs::write(dir.path("target"), vec![0xaa; size as usize + 4096]).unwrap();
    let dst = Disk::open_drive(&dir.path("target")).unwrap();
    let progress = Progress::default();
    // Small chunks, so extents split into several pieces.
    copy::to_drive(&src, &dst, &extents, 1 << 20, &progress).unwrap();
    let used: u64 = extents.iter().map(|e| e.len).sum();
    assert_eq!(
        (progress.done.load(Relaxed), progress.written.load(Relaxed)),
        (used, used)
    );
    let mut want = only_extents(&content, &extents, 0xaa);
    want.extend([0xaa; 4096]);
    assert_same("target", &fs::read(dir.path("target")).unwrap(), &want);
}

#[test]
fn analysis_reads_through_disk_reader() {
    // The analysis gets small reads anywhere; DiskReader serves them from aligned reads.
    let dir = TempDir::new("reader");
    let data = noise(3, 3 * MIB as usize + 777);
    fs::write(dir.path("d"), &data).unwrap();
    let disk = Disk::open_read(&dir.path("d")).unwrap();
    let mut r = super::DiskReader::new(&disk);
    use std::io::{Seek, SeekFrom};
    for (at, len) in [
        (0u64, 512usize),
        (446, 66),
        (MIB - 10, 20),
        (2 * MIB + 5, 300_000),
        (3 * MIB + 700, 77),
    ] {
        r.seek(SeekFrom::Start(at)).unwrap();
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, &data[at as usize..at as usize + len]);
    }
    let mut rest = Vec::new();
    r.seek(SeekFrom::End(-100)).unwrap();
    r.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, &data[data.len() - 100..]);
}

/// 16 MiB and a bit of mixed data: noise, text, zeros.
fn mixed_data() -> Vec<u8> {
    let mut data = noise(11, 4 << 20);
    let text = b"The quick brown fox jumps over the lazy dog. DD-GUI restores images. ";
    data.extend(text.iter().cycle().take(4 << 20));
    data.extend(vec![0u8; 4 << 20]);
    data.extend(noise(12, (4 << 20) + 1000));
    data
}

#[test]
fn restores_images_made_by_other_tools() {
    let dir = TempDir::new("formats");
    let data = mixed_data();
    let raw = dir.path("disk.img");
    fs::write(&raw, &data).unwrap();
    let len = data.len() as u64;
    let q = |p: &Path| format!("'{}'", p.display());

    let mut cases: Vec<(&str, ImageFormat, Option<u64>)> =
        vec![("disk.img", ImageFormat::Raw, Some(len))];
    if have("gzip") {
        sh(&format!(
            "gzip -c {} > {}",
            q(&raw),
            q(&dir.path("disk.img.gz"))
        ));
        // Several members, like `cat a.gz b.gz`.
        sh(&format!(
            "head -c 5000000 {r} | gzip -1 > {o}; tail -c +5000001 {r} | gzip >> {o}",
            r = q(&raw),
            o = q(&dir.path("multi.gz"))
        ));
        cases.extend([
            ("disk.img.gz", ImageFormat::Gzip, None),
            ("multi.gz", ImageFormat::Gzip, None),
        ]);
    }
    if have("xz") {
        sh(&format!(
            "xz -1 -T1 -c {} > {}",
            q(&raw),
            q(&dir.path("disk.img.xz"))
        ));
        // Several blocks: decoded by the threaded decoder.
        sh(&format!(
            "xz -1 -T2 --block-size=1MiB -c {} > {}",
            q(&raw),
            q(&dir.path("blocks.xz"))
        ));
        // Two streams.
        sh(&format!(
            "head -c 3000000 {r} | xz -1 > {o}; tail -c +3000001 {r} | xz -1 >> {o}",
            r = q(&raw),
            o = q(&dir.path("streams.xz"))
        ));
        cases.extend([
            ("disk.img.xz", ImageFormat::Xz, Some(len)),
            ("blocks.xz", ImageFormat::Xz, Some(len)),
            ("streams.xz", ImageFormat::Xz, Some(len)),
        ]);
    }
    if have("zstd") {
        sh(&format!(
            "zstd -q -c {} > {}",
            q(&raw),
            q(&dir.path("disk.img.zst"))
        ));
        // From a pipe the size isn't in the header.
        sh(&format!(
            "zstd -q -c < {} > {}",
            q(&raw),
            q(&dir.path("piped.zst"))
        ));
        // Several frames (from pipes, so without sizes), with a skippable frame in between.
        sh(&format!(
            "head -c 5000000 {r} | zstd -q > {o}; printf '\\x50\\x2a\\x4d\\x18\\x03\\x00\\x00\\x00abc' >> {o}; tail -c +5000001 {r} | zstd -q --long=24 >> {o}",
            r = q(&raw),
            o = q(&dir.path("frames.zst"))
        ));
        cases.extend([
            ("disk.img.zst", ImageFormat::Zstd, Some(len)),
            ("piped.zst", ImageFormat::Zstd, None),
            ("frames.zst", ImageFormat::Zstd, None),
        ]);
    }
    if have("python3") {
        for (name, method) in [("disk.zip", "ZIP_DEFLATED"), ("stored.zip", "ZIP_STORED")] {
            // A folder entry first, and another file after the image.
            let script = format!(
                "import zipfile,sys\nwith zipfile.ZipFile(sys.argv[1],'w',zipfile.{method}) as z:\n z.writestr(zipfile.ZipInfo('images/'),'')\n z.write(sys.argv[2],'images/disk.img')\n z.writestr('README.txt','hello')\n"
            );
            let status = Command::new("python3")
                .args(["-c", &script])
                .arg(dir.path(name))
                .arg(&raw)
                .status()
                .unwrap();
            assert!(status.success());
            cases.push((name, ImageFormat::Zip, Some(len)));
        }
    }

    for (name, format, uncompressed) in cases {
        let path = dir.path(name);
        let info = super::probe(&path).unwrap();
        assert_eq!(info.format, format, "{name}: format");
        assert_eq!(info.uncompressed, uncompressed, "{name}: size");
        assert_eq!(info.smart, None);

        let image = restore::inspect(&path).unwrap();
        let out = dir.path(&format!("{name}.out"));
        let progress = Progress::default();
        let started = Instant::now();
        restore::restore(&image, &Disk::create(&out).unwrap(), 4 << 20, &progress).unwrap();
        eprintln!(
            "{name}: restored in {:.2} s",
            started.elapsed().as_secs_f64()
        );
        assert_same(name, &fs::read(&out).unwrap(), &data);
        assert_eq!(
            progress.done.load(Relaxed),
            image.total(),
            "{name}: progress"
        );
        assert_eq!(progress.written.load(Relaxed), len, "{name}: written");
    }
}

#[test]
fn writing_past_the_end_of_a_drive_fails_cleanly() {
    let dir = TempDir::new("small");
    fs::write(dir.path("img"), noise(1, 3 * MIB as usize)).unwrap();
    fs::write(dir.path("target"), vec![0xaa; 2 * MIB as usize]).unwrap();
    // A regular file stands in for a drive here, so check the writer's own limit.
    let mut target = Disk::open_drive(&dir.path("target")).unwrap();
    target.is_drive = true;
    let image = restore::inspect(&dir.path("img")).unwrap();
    let err = restore::restore(&image, &target, MIB as usize, &Progress::default()).unwrap_err();
    assert!(err.contains("doesn't fit"), "{err}");
}

// ---- The real binary ------------------------------------------------------------------

/// The DD-GUI executable, built once with the same profile as these tests.
fn binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
        let profile = profile_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut cargo = Command::new(env!("CARGO"));
        cargo
            .args([
                "build",
                "--bin",
                "dd-gui",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ])
            .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
        if profile != "debug" {
            cargo.args(["--profile", &profile]);
        }
        assert!(cargo.status().unwrap().success(), "couldn't build dd-gui");
        profile_dir.join(format!("dd-gui{}", std::env::consts::EXE_SUFFIX))
    })
}

/// A running `DD-GUI …`, with its stdout lines as they come.
struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    seen: Vec<String>,
    stderr: std::thread::JoinHandle<String>,
}

impl Worker {
    fn start(args: &[OsString]) -> Worker {
        let mut child = Command::new(binary())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut stderr = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Worker {
            stdin: child.stdin.take(),
            child,
            lines,
            seen: Vec::new(),
            stderr,
        }
    }

    /// Waits for a line starting with `prefix`.
    fn wait_for(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.starts_with(prefix) {
                        return line;
                    }
                }
                Err(_) => panic!("no {prefix} line; got {:?}", self.seen),
            }
        }
    }

    fn say(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// Waits for the exit (stdin stays open until then); returns (code, stdout lines, stderr).
    fn finish(mut self) -> (Option<i32>, Vec<String>, String) {
        let deadline = Instant::now() + Duration::from_secs(300);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the worker didn't finish; lines so far: {:?}", self.seen);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(self.stdin.take());
        let stderr = self.stderr.join().unwrap();
        self.seen.extend(self.lines.iter());
        (status.code(), self.seen, stderr)
    }
}

fn copy_args(extra: &[&str], from: &Path, to: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "copy".into(),
        "--ddgui-worker".into(),
        "--ddgui-watch-stdin".into(),
    ];
    args.extend(extra.iter().map(OsString::from));
    let mut from_arg = OsString::from("--from=");
    from_arg.push(from);
    let mut to_arg = OsString::from("--to=");
    to_arg.push(to);
    args.extend([from_arg, to_arg]);
    args
}

/// The `@` words in order, without `@progress` (which may come any number of times).
fn protocol(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !l.starts_with("@progress"))
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect()
}

fn progress_lines(lines: &[String]) -> Vec<(u64, u64)> {
    lines
        .iter()
        .filter_map(|l| l.strip_prefix("@progress "))
        .map(|l| {
            let mut n = l.split(' ').map(|n| n.parse().unwrap());
            (n.next().unwrap(), n.next().unwrap())
        })
        .collect()
}

fn total(lines: &[String]) -> u64 {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("@total "))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn e2e_smart_image_then_restore() {
    let dir = TempDir::new("e2e");
    // Junk without a partition table: the analysis keeps all of it.
    let data = noise(5, 24 * MIB as usize + 512);
    fs::write(dir.path("drive"), &data).unwrap();

    let w = Worker::start(&copy_args(
        &["--mode=smart"],
        &dir.path("drive"),
        &dir.path("drive.img.gz"),
    ));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        protocol(&lines),
        ["@ready", "@copy", "@total", "@sync"],
        "{lines:?}"
    );
    assert_eq!(total(&lines), data.len() as u64);
    let last = *progress_lines(&lines).last().expect("a final @progress");
    assert_eq!(
        last,
        (
            data.len() as u64,
            fs::metadata(dir.path("drive.img.gz")).unwrap().len()
        )
    );
    assert_same("gunzip", &gunzip(&dir.path("drive.img.gz")), &data);

    let w = Worker::start(&copy_args(
        &["--mode=restore", "--block-size=1M"],
        &dir.path("drive.img.gz"),
        &dir.path("back"),
    ));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(protocol(&lines), ["@ready", "@copy", "@total", "@sync"]);
    assert_eq!(
        *progress_lines(&lines).last().unwrap(),
        (data.len() as u64, data.len() as u64)
    );
    assert_same("restored", &fs::read(dir.path("back")).unwrap(), &data);

    // Errors: one plain line, and a non-zero exit.
    let w = Worker::start(&copy_args(
        &["--mode=restore"],
        &dir.path("missing.img"),
        &dir.path("x"),
    ));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(1));
    assert_eq!(protocol(&lines), ["@ready", "@copy"]);
    assert!(
        stderr.starts_with("dd-gui: couldn't open ")
            && stderr.ends_with("No such file or directory\n"),
        "{stderr}"
    );
}

/// A 64 MiB drive image with an MBR and a FAT32 partition holding a few files.
fn fat_drive(dir: &TempDir) -> Option<PathBuf> {
    if !(have("sfdisk") && have("mkfs.fat") && have("mcopy")) {
        return None;
    }
    let drive = dir.path("fat.img");
    fs::File::create(&drive).unwrap().set_len(64 * MIB).unwrap();
    // Leftovers from an earlier life in the free space, which a smart copy leaves out.
    let file = fs::OpenOptions::new().write(true).open(&drive).unwrap();
    use std::os::unix::fs::FileExt;
    for n in 0..60 {
        file.write_at(&noise(100 + n, 65536), n * MIB + 12345)
            .unwrap();
    }
    let d = drive.display();
    sh(&format!("echo 'start=2048, type=c' | sfdisk -q '{d}'"));
    // The size is in KiB.
    sh(&format!(
        "mkfs.fat -F 32 --offset 2048 '{d}' $((63 * 1024)) >/dev/null"
    ));
    for (name, len) in [
        ("a.bin", 3 * MIB as usize),
        ("b.txt", 100_000),
        ("c.bin", 5 * MIB as usize + 3),
    ] {
        fs::write(dir.path(name), noise(len as u64, len)).unwrap();
        sh(&format!(
            "mcopy -i '{d}'@@1M '{}' ::/",
            dir.path(name).display()
        ));
    }
    Some(drive)
}

#[test]
fn e2e_ask_smart_full_and_cancel() {
    let dir = TempDir::new("ask");
    let Some(drive) = fat_drive(&dir) else { return };
    let size = 64 * MIB;
    let dd_ops = |to: &Path| {
        vec![
            format!("if={}", drive.display()),
            format!("of={}", to.display()),
            "bs=1M".into(),
            "status=progress".into(),
        ]
    };

    // "smart": layout first, then a smart image of the used parts only.
    let mut w = Worker::start(&copy_args(
        &["--mode=smart", "--ask"],
        &drive,
        &dir.path("smart.gz"),
    ));
    let layout: serde_json::Value =
        serde_json::from_str(w.wait_for("@layout ").strip_prefix("@layout ").unwrap()).unwrap();
    assert_eq!(layout["size"], size);
    assert_eq!(layout["table"], "mbr");
    assert_eq!(layout["partitions"][0]["fs"], "vfat");
    w.say("smart");
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        protocol(&lines),
        ["@ready", "@layout", "@copy", "@total", "@sync"]
    );
    let used = total(&lines);
    assert!(used < 20 * MIB && used > 8 * MIB, "used {used}");
    let info = super::probe(&dir.path("smart.gz")).unwrap();
    assert_eq!(
        info.smart,
        Some(super::SmartInfo {
            disk_size: size,
            used
        })
    );

    // The files come back from the restored image, and the file system checks out.
    let restored = dir.path("restored.img");
    let w = Worker::start(&copy_args(
        &["--mode=restore"],
        &dir.path("smart.gz"),
        &restored,
    ));
    let (code, _, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    for name in ["a.bin", "b.txt", "c.bin"] {
        let out = Command::new("mtype")
            .arg("-i")
            .arg(format!("{}@@1M", restored.display()))
            .arg(format!("::/{name}"))
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, fs::read(dir.path(name)).unwrap(), "{name}");
    }
    if have("fsck.fat") {
        sh(&format!(
            "dd if='{}' of='{}' bs=1M skip=1 status=none",
            restored.display(),
            dir.path("part").display()
        ));
        sh(&format!(
            "fsck.fat -n '{}' >/dev/null",
            dir.path("part").display()
        ));
    }

    // "full": the bundled dd copies everything and reports like `DD-GUI dd`.
    let mut args = copy_args(&["--mode=smart", "--ask"], &drive, &dir.path("unused.gz"));
    let mut sync = OsString::from("--ddgui-sync=");
    sync.push(dir.path("full.img"));
    args.insert(1, sync);
    args.push("--".into());
    args.extend(
        dd_ops(&dir.path("full.img"))
            .into_iter()
            .map(OsString::from),
    );
    let mut w = Worker::start(&args);
    w.wait_for("@layout ");
    w.say("full");
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(protocol(&lines), ["@ready", "@layout", "@copy", "@sync"]);
    assert!(
        stderr.contains("64+0 records in") && stderr.contains("67108864 bytes"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(dir.path("full.img")).unwrap(),
        fs::read(&drive).unwrap()
    );
    assert!(!dir.path("unused.gz").exists());

    // dd's own errors come through as dd's, with its exit code.
    let mut args = copy_args(&["--mode=smart", "--ask"], &drive, &dir.path("unused.gz"));
    args.extend([
        "--".into(),
        format!("if={}", dir.path("nothing").display()).into(),
        "of=/dev/null".into(),
    ]);
    let mut w = Worker::start(&args);
    w.wait_for("@layout ");
    w.say("full");
    let (code, _, stderr) = w.finish();
    assert_eq!(code, Some(1));
    assert!(stderr.starts_with("dd: failed to open"), "{stderr}");

    // Anything else, or no answer at all, cancels.
    let mut w = Worker::start(&copy_args(
        &["--mode=smart", "--ask"],
        &drive,
        &dir.path("no.gz"),
    ));
    w.wait_for("@layout ");
    w.say("nope");
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(130), "{stderr}");
    assert_eq!(protocol(&lines), ["@ready", "@layout"]);
    assert!(stderr.ends_with("dd-gui: cancelled\n"));

    let mut w = Worker::start(&copy_args(
        &["--mode=smart", "--ask"],
        &drive,
        &dir.path("eof.gz"),
    ));
    w.wait_for("@layout ");
    drop(w.stdin.take());
    let (code, lines, _) = w.finish();
    assert_eq!(code, Some(130));
    assert_eq!(protocol(&lines), ["@ready", "@layout"]);

    // The answer file (macOS), with stdin unused.
    let answer = dir.path("answer");
    let mut args = copy_args(&["--mode=smart", "--ask"], &drive, &dir.path("file.gz"));
    args.retain(|a| a != "--ddgui-watch-stdin");
    let mut answer_arg = OsString::from("--ddgui-answer-file=");
    answer_arg.push(&answer);
    args.insert(1, answer_arg);
    let mut w = Worker::start(&args);
    drop(w.stdin.take());
    w.wait_for("@layout ");
    fs::write(dir.path("answer.partial"), "smart\n").unwrap();
    fs::rename(dir.path("answer.partial"), &answer).unwrap();
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(total(&lines), used);
}

/// A big, sparse, file-system-less "drive": a smart image of it takes a while.
fn slow_job(dir: &TempDir) -> PathBuf {
    let drive = dir.path("big");
    fs::File::create(&drive).unwrap().set_len(8 << 30).unwrap();
    drive
}

#[test]
fn e2e_cancel_mid_copy() {
    let dir = TempDir::scratch("cancel");
    let drive = slow_job(&dir);

    // Through stdin closing.
    let mut w = Worker::start(&copy_args(&["--mode=smart"], &drive, &dir.path("a.gz")));
    w.wait_for("@progress ");
    w.wait_for("@progress ");
    let asked = Instant::now();
    drop(w.stdin.take());
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(130), "{stderr}");
    assert!(asked.elapsed() < Duration::from_secs(5));
    assert!(stderr.ends_with("dd-gui: cancelled\n"));
    assert!(!lines.iter().any(|l| l == "@sync"));
    let progress = progress_lines(&lines);
    assert!(
        progress
            .windows(2)
            .all(|p| p[0].0 <= p[1].0 && p[0].1 <= p[1].1),
        "{progress:?}"
    );

    // Through the cancel file, with stdin left alone.
    let cancel = dir.path("cancel");
    let mut args = copy_args(&["--mode=smart"], &drive, &dir.path("b.gz"));
    args.retain(|a| a != "--ddgui-watch-stdin");
    let mut cancel_arg = OsString::from("--ddgui-cancel-file=");
    cancel_arg.push(&cancel);
    args.insert(1, cancel_arg);
    let mut w = Worker::start(&args);
    w.wait_for("@progress ");
    fs::write(&cancel, "").unwrap();
    let (code, _, stderr) = w.finish();
    assert_eq!(code, Some(130), "{stderr}");
}

#[test]
fn e2e_bundled_dd_still_works() {
    let dir = TempDir::new("dd");
    let data = noise(9, 3 * MIB as usize + 100);
    fs::write(dir.path("in"), &data).unwrap();
    let out = Command::new(binary())
        .args(["dd", "--ddgui-worker", "bs=1M", "status=progress"])
        .arg(format!("if={}", dir.path("in").display()))
        .arg(format!("of={}", dir.path("out").display()))
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "@ready\n@copy\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("3+1 records in"));
    assert_eq!(fs::read(dir.path("out")).unwrap(), data);
}

// ---- Heavy tests ------------------------------------------------------------------------

/// Drives of 16 and 64 GiB with a few MB used: their smart images take almost no time.
#[test]
#[ignore]
fn gaps_cost_almost_nothing() {
    for gib in [16u64, 64] {
        gaps(gib << 30);
    }
}

fn gaps(size: u64) {
    let dir = TempDir::scratch("gaps");
    let drive = dir.path("sparse");
    fs::File::create(&drive).unwrap().set_len(size).unwrap();
    // 1 MiB at both ends and 64 KiB every 256 MiB, like a file system's metadata.
    let mut extents = vec![ext(0, MIB)];
    extents.extend((1..size / (256 * MIB)).map(|i| ext(i * 256 * MIB, 64 << 10)));
    extents.push(ext(size - MIB, MIB));
    let file = fs::OpenOptions::new().write(true).open(&drive).unwrap();
    use std::os::unix::fs::FileExt;
    for (n, e) in extents.iter().enumerate() {
        file.write_at(&noise(n as u64, e.len as usize), e.start)
            .unwrap();
    }
    let map = Map {
        size,
        extents: extents.clone(),
    };
    let src = Disk::open_read(&drive).unwrap();
    let out = dir.path("sparse.img.gz");
    let started = Instant::now();
    copy::to_image(
        &src,
        &map,
        &Disk::create(&out).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    let took = started.elapsed();
    let bytes = fs::metadata(&out).unwrap().len();
    eprintln!(
        "{} GiB drive, {} KiB used: smart image of {:.1} MB in {:.2} s",
        size >> 30,
        map.used() >> 10,
        bytes as f64 / 1e6,
        took.as_secs_f64()
    );
    assert!(took < Duration::from_secs(10));

    // Restoring skips the free space too.
    let image = restore::inspect(&out).unwrap();
    let back = dir.path("back");
    let started = Instant::now();
    restore::restore(
        &image,
        &Disk::create(&back).unwrap(),
        4 << 20,
        &Progress::default(),
    )
    .unwrap();
    eprintln!("restored in {:.2} s", started.elapsed().as_secs_f64());
    let back = fs::File::open(&back).unwrap();
    assert_eq!(back.metadata().unwrap().len(), size);
    for (n, e) in extents.iter().enumerate() {
        let mut buf = vec![0u8; e.len as usize];
        back.read_exact_at(&mut buf, e.start).unwrap();
        assert_eq!(buf, noise(n as u64, e.len as usize));
    }
}

/// Throughput on real data (set DDGUI_TEST_DATA to a big file, e.g. a tar of /usr/lib).
#[test]
#[ignore]
fn throughput() {
    let dir = TempDir::scratch("speed");
    let mut sources = vec![("random", {
        let path = dir.path("random");
        let mut f = fs::File::create(&path).unwrap();
        for i in 0..64 {
            f.write_all(&noise(i, 4 << 20)).unwrap();
        }
        path
    })];
    if let Some(path) = std::env::var_os("DDGUI_TEST_DATA") {
        sources.push(("real", PathBuf::from(path)));
    }
    for (name, path) in sources {
        let src = Disk::open_read(&path).unwrap();
        let map = Map {
            size: src.size,
            extents: vec![ext(0, src.size)],
        };
        let out = dir.path(&format!("{name}.gz"));
        let started = Instant::now();
        copy::to_image(
            &src,
            &map,
            &Disk::create(&out).unwrap(),
            &Progress::default(),
        )
        .unwrap();
        let secs = started.elapsed().as_secs_f64();
        let packed = fs::metadata(&out).unwrap().len();
        eprintln!(
            "{name}: compressed {:.0} MB at {:.0} MB/s, ratio {:.3}",
            src.size as f64 / 1e6,
            src.size as f64 / 1e6 / secs,
            packed as f64 / src.size as f64
        );
        let image = restore::inspect(&out).unwrap();
        let back = dir.path(&format!("{name}.back"));
        let started = Instant::now();
        restore::restore(
            &image,
            &Disk::create(&back).unwrap(),
            4 << 20,
            &Progress::default(),
        )
        .unwrap();
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "{name}: restored at {:.0} MB/s",
            src.size as f64 / 1e6 / secs
        );
        assert_eq!(fs::metadata(&back).unwrap().len(), src.size);
        let _ = fs::remove_file(&back);
    }
}
