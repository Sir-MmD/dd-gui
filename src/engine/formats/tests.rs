//! Tests for the image formats. Fixtures are made at test time by the real tools
//! (qemu-img, 7z, bzip2, lz4, xz, zstd, tar) from one reference raw image: 48 MiB of
//! random data, zero runs and patterns, and an odd tail. What the decoders give back has
//! to equal the reference byte for byte (virtual disks: plus the zeros up to their size).
//! A test whose tool is missing is skipped, with a note.
//!
//! DMG: nothing on Linux makes one, so a small UDIF writer below makes them (every kind of
//! chunk, both kinds of tables). On macOS, `hdiutil` makes real ones too.
//!
//! Fixtures go to `$DDGUI_TEST_SCRATCH` (default: the temp folder). Decoding speeds are
//! printed (`-- --nocapture`); they mean something in release builds only.

use super::{ImageFormat, detect, open, untar};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Instant;

const MIB: usize = 1 << 20;
/// The reference image's size: 48 MiB and an odd tail.
const REF_LEN: usize = 48 * MIB + 12_345;

/// A temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base =
            std::env::var_os("DDGUI_TEST_SCRATCH").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = base.join(format!(
            "dd-gui-formats-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Relaxed)
        ));
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

/// The reference raw image.
fn reference() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut d = Vec::with_capacity(REF_LEN);
        let text =
            b"DD-GUI writes disk images to drives. The quick brown fox jumps over the lazy dog. ";
        // 0-4 MiB: noise (and something like a partition table at the start).
        d.extend(noise(1, 4 * MIB));
        d[510] = 0x55;
        d[511] = 0xaa;
        // 4-8: zeros.
        d.resize(8 * MIB, 0);
        // 8-12: text.
        d.extend(text.iter().cycle().take(4 * MIB));
        // 12-13: noise, then zeros to 16.
        d.extend(noise(2, MIB));
        d.resize(16 * MIB, 0);
        // 16-24: 64 KiB of noise, 64 KiB of zeros, and so on (cluster-sized holes).
        for i in 0..64 {
            if i % 2 == 0 {
                d.extend(noise(100 + i, 64 << 10));
            } else {
                d.resize(d.len() + (64 << 10), 0);
            }
        }
        // 24-28: a byte pattern, and a run of 0xff.
        d.extend((0..3 * MIB).map(|i| (i % 251) as u8));
        d.resize(28 * MIB, 0xff);
        // 28-40: noise.
        d.extend(noise(3, 12 * MIB));
        // 40-48: zeros with a few small islands of data at odd places.
        d.resize(48 * MIB, 0);
        for (n, at) in [
            40 * MIB + 12_345,
            42 * MIB + 4096,
            45 * MIB - 1000,
            48 * MIB - 700,
        ]
        .into_iter()
        .enumerate()
        {
            let island = noise(200 + n as u64, 600);
            d[at..at + 600].copy_from_slice(&island);
        }
        // The odd tail.
        d.extend(noise(4, REF_LEN - 48 * MIB));
        assert_eq!(d.len(), REF_LEN);
        d
    })
}

/// Part of the reference, for the slow cases: all of it in release builds (where speeds
/// are measured), 8 MiB and a bit in debug builds.
fn part() -> &'static [u8] {
    if cfg!(debug_assertions) {
        &reference()[..8 * MIB + 4321]
    } else {
        reference()
    }
}

fn write_reference(dir: &TempDir) -> PathBuf {
    let path = dir.path("ref.img");
    fs::write(&path, reference()).unwrap();
    path
}

fn have(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        eprintln!("note: {tool} isn't installed, skipping what needs it");
    }
    found
}

/// Runs a command, which has to succeed.
fn run(cmd: &mut Command) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("couldn't run {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs a command with its output going to (the end of) `to`.
fn run_to(cmd: &mut Command, to: &Path) {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(to)
        .unwrap();
    let out = cmd
        .stdout(file)
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("couldn't run {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn qemu_img(args: &[&str], from: &Path, to: &Path) {
    run(Command::new("qemu-img")
        .args(["convert", "-f", "raw"])
        .args(args)
        .arg(from)
        .arg(to));
}

/// The virtual size qemu-img reports.
fn virtual_size(path: &Path, format: &str) -> u64 {
    let out = Command::new("qemu-img")
        .args(["info", "--output=json", "-f", format])
        .arg(path)
        .output()
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    info["virtual-size"].as_u64().unwrap()
}

fn detect_file(path: &Path) -> io::Result<Option<super::Detected>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = Vec::new();
    (&mut file).take(4096).read_to_end(&mut head)?;
    detect(&mut file, size, &head)
}

/// What a check expects.
struct Want<'a> {
    format: ImageFormat,
    /// What `detect` should say about the raw size.
    raw_size: Option<u64>,
    /// The content: `data`, then zeros up to `len`.
    data: &'a [u8],
    len: u64,
    /// Whether `open` knows the size.
    sized: bool,
}

impl Want<'_> {
    fn reference(format: ImageFormat, raw_size: Option<u64>, len: u64) -> Want<'static> {
        Want {
            format,
            raw_size,
            data: reference(),
            len,
            sized: raw_size.is_some(),
        }
    }
}

/// Detects, opens and decodes `path`, and compares with what's expected. Prints the speed.
fn check(name: &str, path: &Path, want: Want) {
    let found = detect_file(path)
        .unwrap_or_else(|e| panic!("{name}: detect failed: {e}"))
        .unwrap_or_else(|| panic!("{name}: not detected"));
    assert_eq!(found.format, want.format, "{name}: format");
    assert_eq!(
        found.raw_size, want.raw_size,
        "{name}: raw size from detect"
    );
    let started = Instant::now();
    let opened = open(path, want.format).unwrap_or_else(|e| panic!("{name}: open failed: {e}"));
    if want.sized {
        assert_eq!(opened.size, Some(want.len), "{name}: size from open");
    } else {
        assert_eq!(opened.size, None, "{name}: size from open");
    }
    let mut reader = opened.reader;
    let mut buf = vec![0u8; 4 * MIB];
    let mut pos = 0usize;
    loop {
        let n = super::read_full(&mut reader, &mut buf)
            .unwrap_or_else(|e| panic!("{name}: read failed at byte {pos}: {e}"));
        if n == 0 {
            break;
        }
        assert!(
            (pos + n) as u64 <= want.len,
            "{name}: more data than expected ({} bytes past {})",
            pos + n,
            want.len
        );
        let got = &buf[..n];
        let in_data = want.data.len().saturating_sub(pos).min(n);
        if got[..in_data] != want.data[pos..pos + in_data] {
            let at = (0..in_data)
                .find(|&i| got[i] != want.data[pos + i])
                .unwrap_or(0);
            panic!("{name}: first difference at byte {}", pos + at);
        }
        if let Some(at) = got[in_data..].iter().position(|&b| b != 0) {
            panic!(
                "{name}: expected zeros, found data at byte {}",
                pos + in_data + at
            );
        }
        pos += n;
    }
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(pos as u64, want.len, "{name}: length");
    eprintln!(
        "formats: {name}: {:.1} MB in {:.3} s, {:.0} MB/s",
        pos as f64 / 1e6,
        secs,
        pos as f64 / 1e6 / secs.max(1e-9)
    );
}

/// Opening has to fail, with a message that mentions `words` (any of them).
fn check_refused(name: &str, path: &Path, format: ImageFormat, words: &[&str]) {
    let found = detect_file(path)
        .unwrap_or_else(|e| panic!("{name}: detect failed: {e}"))
        .unwrap_or_else(|| panic!("{name}: not detected"));
    assert_eq!(found.format, format, "{name}: format");
    let err = match open(path, format) {
        Ok(mut opened) => io::copy(&mut opened.reader, &mut io::sink())
            .err()
            .unwrap_or_else(|| panic!("{name}: opened and read fine")),
        Err(e) => e,
    };
    let text = err.to_string();
    assert!(
        words
            .iter()
            .any(|w| text.to_lowercase().contains(&w.to_lowercase())),
        "{name}: unexpected error: {text}"
    );
    eprintln!("formats: {name}: refused: {text}");
}

fn len() -> u64 {
    REF_LEN as u64
}

#[test]
fn bzip2_streams() {
    if !have("bzip2") {
        return;
    }
    let dir = TempDir::new("bzip2");
    let raw = write_reference(&dir);
    let bz = dir.path("ref.img.bz2");
    run_to(Command::new("bzip2").args(["-c", "-9"]).arg(&raw), &bz);
    check(
        "bzip2",
        &bz,
        Want::reference(ImageFormat::Bzip2, None, len()),
    );

    // An empty stream, then two (as pbzip2 or `cat` make them), and zeros after them.
    let (a, b) = (dir.path("a"), dir.path("b"));
    fs::write(&a, &reference()[..5_000_000]).unwrap();
    fs::write(&b, &reference()[5_000_000..]).unwrap();
    fs::write(dir.path("empty"), b"").unwrap();
    let multi = dir.path("multi.bz2");
    run_to(
        Command::new("bzip2").args(["-c"]).arg(dir.path("empty")),
        &multi,
    );
    run_to(Command::new("bzip2").args(["-c", "-1"]).arg(&a), &multi);
    run_to(Command::new("bzip2").args(["-c", "-1"]).arg(&b), &multi);
    fs::OpenOptions::new()
        .append(true)
        .open(&multi)
        .unwrap()
        .write_all(&[0; 100])
        .unwrap();
    check(
        "bzip2 (3 streams)",
        &multi,
        Want::reference(ImageFormat::Bzip2, None, len()),
    );

    for path in [&bz, &multi] {
        let src =
            std::sync::Arc::new(super::source::Source::new(File::open(path).unwrap()).unwrap());
        if let Some(mut parallel) = super::bz2::ParBz2::new(src) {
            let mut out = Vec::new();
            parallel.read_to_end(&mut out).unwrap();
            // Reading on at the end gives nothing more (and costs nothing).
            assert_eq!(parallel.read(&mut [0; 100]).unwrap(), 0);
            assert!(out == reference(), "{}: output differs", path.display());
            assert!(
                !parallel.fell_back(),
                "{}: the parallel decoder gave up",
                path.display()
            );
        }
    }

    // When a block doesn't decode in parallel (as when a magic number turns out to be
    // part of the data), the plain decoder takes over from the start of its stream: the
    // result is the same.
    let cases = [(&bz, 1), (&multi, 60), (&bz, 5), (&multi, 3), (&multi, 300)];
    // (Each case decodes everything; debug builds are slow at that.)
    let cases = if cfg!(debug_assertions) {
        &cases[..2]
    } else {
        &cases[..]
    };
    for &(path, n) in cases {
        let src =
            std::sync::Arc::new(super::source::Source::new(File::open(path).unwrap()).unwrap());
        let Some(parallel) = super::bz2::ParBz2::new(src) else {
            eprintln!("note: one CPU, no parallel bzip2 to test");
            break;
        };
        let mut out = Vec::new();
        parallel.failing_at(n).read_to_end(&mut out).unwrap();
        assert!(
            out == reference(),
            "bzip2 with block {n} failing: output differs"
        );
    }

    // Anything else after the end is an error, not a quiet end.
    let junk = dir.path("junk.bz2");
    fs::copy(&multi, &junk).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&junk)
        .unwrap()
        .write_all(b"junk")
        .unwrap();
    let mut r = open(&junk, ImageFormat::Bzip2).unwrap().reader;
    assert!(io::copy(&mut r, &mut io::sink()).is_err());
    // So is damage: bits flipped in a block (its CRC catches it).
    let mut damaged = fs::read(&bz).unwrap();
    damaged[3_000_000] ^= 0x10;
    fs::write(dir.path("damaged.bz2"), &damaged).unwrap();
    let mut r = open(&dir.path("damaged.bz2"), ImageFormat::Bzip2)
        .unwrap()
        .reader;
    assert!(io::copy(&mut r, &mut io::sink()).is_err());
}

#[test]
fn lz4_frames() {
    let dir = TempDir::new("lz4");
    // Linked blocks, with the size, block checksums and the content checksum, as
    // lz4_flex writes them (needs no tool).
    let encoded = |mode, size: Option<u64>, block, data: &[u8]| {
        let info = lz4_flex::frame::FrameInfo::new()
            .block_mode(mode)
            .block_size(block)
            .content_size(size)
            .block_checksums(true)
            .content_checksum(true);
        let mut enc = lz4_flex::frame::FrameEncoder::with_frame_info(info, Vec::new());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    };
    let linked = dir.path("linked.lz4");
    fs::write(
        &linked,
        encoded(
            lz4_flex::frame::BlockMode::Linked,
            Some(len()),
            lz4_flex::frame::BlockSize::Max4MB,
            reference(),
        ),
    )
    .unwrap();
    check(
        "lz4 (linked, sized, checksums)",
        &linked,
        Want::reference(ImageFormat::Lz4, Some(len()), len()),
    );
    // A skippable frame first (bigger than the 4 KiB detection gets to see), then two
    // frames with sizes: the size is their sum.
    let cut = 3_000_001;
    let multi = dir.path("multi.lz4");
    let mut data = vec![0x5a, 0x2a, 0x4d, 0x18];
    data.extend(10_000u32.to_le_bytes());
    data.extend(noise(15, 10_000));
    data.extend(encoded(
        lz4_flex::frame::BlockMode::Independent,
        Some(cut as u64),
        lz4_flex::frame::BlockSize::Max64KB,
        &reference()[..cut],
    ));
    data.extend(encoded(
        lz4_flex::frame::BlockMode::Linked,
        Some((REF_LEN - cut) as u64),
        lz4_flex::frame::BlockSize::Max256KB,
        &reference()[cut..],
    ));
    fs::write(&multi, &data).unwrap();
    check(
        "lz4 (skippable + 2 frames)",
        &multi,
        Want::reference(ImageFormat::Lz4, Some(len()), len()),
    );

    if !have("lz4") {
        return;
    }
    let raw = write_reference(&dir);
    let lz4 = |args: &[&str], from: &Path, to: &Path| {
        run_to(
            Command::new("lz4").args(["-q", "-c"]).args(args).arg(from),
            to,
        );
    };
    // Independent blocks with a content checksum (lz4's default).
    let plain = dir.path("plain.lz4");
    lz4(&[], &raw, &plain);
    check(
        "lz4",
        &plain,
        Want::reference(ImageFormat::Lz4, None, len()),
    );
    // Linked blocks.
    let linked = dir.path("linked-cli.lz4");
    lz4(&["-BD", "-9"], &raw, &linked);
    check(
        "lz4 (-BD)",
        &linked,
        Want::reference(ImageFormat::Lz4, None, len()),
    );
    // Small blocks with block checksums.
    let small = dir.path("small.lz4");
    lz4(&["-B4", "-BX"], &raw, &small);
    check(
        "lz4 (64 KiB blocks, checksums)",
        &small,
        Want::reference(ImageFormat::Lz4, None, len()),
    );
    // The legacy format.
    let legacy = dir.path("legacy.lz4");
    lz4(&["-l"], &raw, &legacy);
    check(
        "lz4 (legacy)",
        &legacy,
        Want::reference(ImageFormat::Lz4, None, len()),
    );
    // A damaged block is an error.
    let mut bad = fs::read(&plain).unwrap();
    bad[1_000_000] ^= 0x55;
    fs::write(dir.path("bad.lz4"), &bad).unwrap();
    let mut r = open(&dir.path("bad.lz4"), ImageFormat::Lz4).unwrap().reader;
    assert!(io::copy(&mut r, &mut io::sink()).is_err());
}

#[test]
fn lzma_alone() {
    if !have("xz") {
        return;
    }
    let dir = TempDir::new("lzma");
    let raw = write_reference(&dir);
    let lzma = dir.path("ref.lzma");
    run_to(
        Command::new("xz")
            .args(["--format=lzma", "-1", "-c"])
            .arg(&raw),
        &lzma,
    );
    check(
        "lzma",
        &lzma,
        Want::reference(ImageFormat::Lzma, None, len()),
    );
    // With the size in the header (as the LZMA SDK writes it).
    let mut sized = fs::read(&lzma).unwrap();
    sized[5..13].copy_from_slice(&len().to_le_bytes());
    fs::write(dir.path("sized.lzma"), &sized).unwrap();
    check(
        "lzma (sized)",
        &dir.path("sized.lzma"),
        Want::reference(ImageFormat::Lzma, Some(len()), len()),
    );
}

#[test]
fn sevenz_archives() {
    if !have("7z") {
        return;
    }
    let dir = TempDir::new("7z");
    // A folder and an empty file come first: the image is the first real file.
    fs::create_dir_all(dir.path("in/a-folder")).unwrap();
    fs::write(dir.path("in/a-empty.txt"), b"").unwrap();
    fs::write(dir.path("in/disk.img"), reference()).unwrap();
    fs::write(dir.path("in/part.img"), part()).unwrap();
    fs::write(dir.path("in/small.img"), &part()[..MIB + 99]).unwrap();
    for (name, method, whole) in [
        ("lzma2", "-m0=LZMA2", true),
        ("bzip2", "-m0=BZip2", true),
        ("lzma", "-m0=LZMA", false),
        ("deflate", "-m0=Deflate", false),
        ("ppmd", "-m0=PPMd", false),
        ("copy", "-m0=Copy", false),
        ("bcj+lzma2", "-mf=BCJ", false),
    ] {
        let archive = dir.path(&format!("{name}.7z"));
        // PPMd is very slow on random data: less of it.
        let (image, data) = match (whole, name) {
            (true, _) => ("disk.img", reference()),
            (false, "ppmd") => ("small.img", &part()[..MIB + 99]),
            (false, _) => ("part.img", part()),
        };
        run(Command::new("7z")
            .args(["a", "-bd", "-mx=1", method])
            .arg(&archive)
            .arg("a-folder")
            .arg("a-empty.txt")
            .arg(image)
            .current_dir(dir.path("in")));
        let len = data.len() as u64;
        check(
            &format!("7z ({name})"),
            &archive,
            Want {
                format: ImageFormat::SevenZ,
                raw_size: Some(len),
                data,
                len,
                sized: true,
            },
        );
    }
}

#[test]
fn sevenz_encrypted() {
    if !have("7z") {
        return;
    }
    let dir = TempDir::new("7z-enc");
    fs::write(dir.path("secret.img"), noise(9, 100_000)).unwrap();
    for (name, extra) in [("enc.7z", None), ("enc-headers.7z", Some("-mhe=on"))] {
        let mut cmd = Command::new("7z");
        cmd.args(["a", "-bd", "-pSECRET"]);
        if let Some(extra) = extra {
            cmd.arg(extra);
        }
        run(cmd.arg(dir.path(name)).arg(dir.path("secret.img")));
        check_refused(name, &dir.path(name), ImageFormat::SevenZ, &["password"]);
    }
}

#[test]
fn tar_archives() {
    if !have("tar") {
        return;
    }
    let dir = TempDir::new("tar");
    write_reference(&dir);
    // GNU tar has more formats (and sparse files) than bsdtar (macOS, Windows).
    let gnu = gnu_tar();
    // A folder first, and a long name (GNU: an 'L' entry; pax: a path record).
    let long = format!("images/{}/disk.img", "a-long-folder-name".repeat(6));
    fs::create_dir_all(dir.path("in").join(&long).parent().unwrap()).unwrap();
    fs::copy(dir.path("ref.img"), dir.path("in").join(&long)).unwrap();
    // ustar can't hold the long name: a short one.
    fs::create_dir_all(dir.path("short")).unwrap();
    fs::copy(dir.path("ref.img"), dir.path("short/disk.img")).unwrap();
    let mut cases = vec![
        ("default", vec![], "in", "images"),
        ("pax", vec!["--format=pax"], "in", "images"),
    ];
    cases.push(("ustar", vec!["--format=ustar"], "short", "disk.img"));
    if gnu {
        cases.push(("gnu", vec!["--format=gnu"], "in", "images"));
    }
    for (name, args, from, what) in cases {
        let tar = dir.path(&format!("{name}.tar"));
        run(Command::new("tar")
            .args(&args)
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(dir.path(from))
            .arg(what));
        check(
            &format!("tar ({name})"),
            &tar,
            Want::reference(ImageFormat::Tar, Some(len()), len()),
        );
    }

    // Sparse files: holes where the reference has zeros (GNU tar and cp).
    if !gnu {
        eprintln!("note: not GNU tar, skipping sparse tars");
        return;
    }
    let sparse = dir.path("sparse/disk.img");
    fs::create_dir_all(sparse.parent().unwrap()).unwrap();
    run(Command::new("cp")
        .arg("--sparse=always")
        .arg(dir.path("ref.img"))
        .arg(&sparse));
    for (name, args) in [
        ("gnu sparse", vec!["--format=gnu", "-S"]),
        (
            "pax sparse 0.0",
            vec!["--format=pax", "-S", "--sparse-version=0.0"],
        ),
        (
            "pax sparse 0.1",
            vec!["--format=pax", "-S", "--sparse-version=0.1"],
        ),
        (
            "pax sparse 1.0",
            vec!["--format=pax", "-S", "--sparse-version=1.0"],
        ),
    ] {
        let tar = dir.path(&format!("{}.tar", name.replace(' ', "-")));
        run(Command::new("tar")
            .args(&args)
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(dir.path("sparse"))
            .arg("disk.img"));
        check(
            &format!("tar ({name})"),
            &tar,
            Want::reference(ImageFormat::Tar, Some(len()), len()),
        );
    }
}

/// Is `tar` GNU tar?
fn gnu_tar() -> bool {
    Command::new("tar")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("GNU tar"))
}

/// A tar inside the engine's formats: `untar` gives its first file.
#[test]
fn untar_inside_compressed_streams() {
    if !have("tar") {
        return;
    }
    let dir = TempDir::new("untar");
    write_reference(&dir);
    fs::create_dir_all(dir.path("in/folder")).unwrap();
    fs::copy(dir.path("ref.img"), dir.path("in/disk.img")).unwrap();
    let tar = dir.path("disk.tar");
    run(Command::new("tar")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(dir.path("in"))
        .args(["folder", "disk.img"]));
    let check_untar = |name: &str, stream: Box<dyn Read + Send>| {
        let started = Instant::now();
        let mut out = Vec::new();
        untar(stream).unwrap().read_to_end(&mut out).unwrap();
        assert!(out == reference(), "{name}: untarred data differs");
        eprintln!(
            "formats: untar ({name}): {:.0} MB/s",
            out.len() as f64 / 1e6 / started.elapsed().as_secs_f64()
        );
    };
    check_untar("tar", Box::new(File::open(&tar).unwrap()));
    if have("gzip") {
        let gz = dir.path("disk.tar.gz");
        run_to(Command::new("gzip").args(["-1", "-c"]).arg(&tar), &gz);
        check_untar(
            "tar.gz",
            Box::new(flate2::read::MultiGzDecoder::new(File::open(&gz).unwrap())),
        );
    }
    if have("xz") {
        let xz = dir.path("disk.tar.xz");
        run_to(Command::new("xz").args(["-0", "-c"]).arg(&tar), &xz);
        check_untar(
            "tar.xz",
            Box::new(lzma_rust2::XzReader::new(
                io::BufReader::new(File::open(&xz).unwrap()),
                true,
            )),
        );
    }
    if have("zstd") {
        let zst = dir.path("disk.tar.zst");
        run_to(Command::new("zstd").args(["-q", "-c"]).arg(&tar), &zst);
        check_untar(
            "tar.zst",
            Box::new(zstd::stream::read::Decoder::new(File::open(&zst).unwrap()).unwrap()),
        );
    }
    // Not a tar: everything comes back, the peeked bytes included.
    for data in [reference(), &b"short"[..], &[][..]] {
        let mut out = Vec::new();
        untar(Box::new(io::Cursor::new(data.to_vec())))
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == data, "untar changed a stream that isn't a tar");
    }
}

/// A tar inside the formats decoded here (bzip2, lz4, lzma, 7z).
#[test]
fn tar_inside_other_formats() {
    if !have("tar") {
        return;
    }
    let dir = TempDir::new("tar-inside");
    write_reference(&dir);
    let tar = dir.path("disk.tar");
    run(Command::new("tar")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(&dir.0)
        .arg("ref.img"));
    let want = |format| Want::reference(format, Some(len()), len());
    if have("bzip2") {
        let out = dir.path("disk.tar.bz2");
        run_to(Command::new("bzip2").args(["-1", "-c"]).arg(&tar), &out);
        check("tar.bz2", &out, want(ImageFormat::Bzip2));
    }
    if have("lz4") {
        let out = dir.path("disk.tar.lz4");
        run_to(Command::new("lz4").args(["-q", "-c"]).arg(&tar), &out);
        check("tar.lz4", &out, want(ImageFormat::Lz4));
    }
    if have("xz") {
        let out = dir.path("disk.tar.lzma");
        run_to(
            Command::new("xz")
                .args(["--format=lzma", "-0", "-c"])
                .arg(&tar),
            &out,
        );
        check("tar.lzma", &out, want(ImageFormat::Lzma));
    }
    if have("7z") {
        let out = dir.path("disk.tar.7z");
        run(Command::new("7z")
            .args(["a", "-bd", "-mx=1"])
            .arg(&out)
            .arg(&tar));
        check("tar.7z", &out, want(ImageFormat::SevenZ));
    }
}

#[test]
fn vhd_fixed_and_dynamic() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("vhd");
    let raw = write_reference(&dir);
    for (name, sub) in [
        ("dynamic", "subformat=dynamic"),
        ("fixed", "subformat=fixed"),
    ] {
        let vhd = dir.path(&format!("{name}.vhd"));
        qemu_img(&["-O", "vpc", "-o", sub], &raw, &vhd);
        let size = virtual_size(&vhd, "vpc");
        assert!(size >= len());
        check(
            &format!("vhd ({name})"),
            &vhd,
            Want::reference(ImageFormat::Vhd, Some(size), size),
        );
    }
}

#[test]
fn vhdx_fixed_and_dynamic() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("vhdx");
    let raw = write_reference(&dir);
    for (name, opts) in [
        ("dynamic", "subformat=dynamic"),
        ("fixed", "subformat=fixed"),
        ("dynamic, 1 MiB blocks", "subformat=dynamic,block_size=1M"),
    ] {
        let vhdx = dir.path(&format!("{}.vhdx", name.replace([' ', ','], "-")));
        qemu_img(&["-O", "vhdx", "-o", opts], &raw, &vhdx);
        let size = virtual_size(&vhdx, "vhdx");
        check(
            &format!("vhdx ({name})"),
            &vhdx,
            Want::reference(ImageFormat::Vhdx, Some(size), size),
        );
    }
}

#[test]
fn vmdk_sparse_and_stream_optimized() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("vmdk");
    let raw = write_reference(&dir);
    for sub in ["monolithicSparse", "streamOptimized"] {
        let vmdk = dir.path(&format!("{sub}.vmdk"));
        qemu_img(
            &["-O", "vmdk", "-o", &format!("subformat={sub}")],
            &raw,
            &vmdk,
        );
        let size = virtual_size(&vmdk, "vmdk");
        check(
            &format!("vmdk ({sub})"),
            &vmdk,
            Want::reference(ImageFormat::Vmdk, Some(size), size),
        );
    }
}

#[test]
fn qcow2_variants() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("qcow2");
    let raw = write_reference(&dir);
    for (name, args) in [
        ("v3", vec!["-O", "qcow2"]),
        ("v3, zlib", vec!["-O", "qcow2", "-c"]),
        (
            "v3, zstd",
            vec!["-O", "qcow2", "-c", "-o", "compression_type=zstd"],
        ),
        ("v2", vec!["-O", "qcow2", "-o", "compat=0.10"]),
        ("v2, zlib", vec!["-O", "qcow2", "-c", "-o", "compat=0.10"]),
        (
            "512-byte clusters",
            vec!["-O", "qcow2", "-o", "cluster_size=512"],
        ),
        (
            "2 MiB clusters, zlib",
            vec!["-O", "qcow2", "-c", "-o", "cluster_size=2M"],
        ),
        (
            "extended L2",
            vec!["-O", "qcow2", "-o", "extended_l2=on,cluster_size=128k"],
        ),
        (
            "extended L2, zstd",
            vec![
                "-O",
                "qcow2",
                "-c",
                "-o",
                "extended_l2=on,compression_type=zstd",
            ],
        ),
    ] {
        let qcow2 = dir.path(&format!("{}.qcow2", name.replace([' ', ','], "-")));
        qemu_img(&args, &raw, &qcow2);
        let size = virtual_size(&qcow2, "qcow2");
        check(
            &format!("qcow2 ({name})"),
            &qcow2,
            Want::reference(ImageFormat::Qcow2, Some(size), size),
        );
    }
}

/// Zero clusters, and subclusters allocated one by one: made by writing into images.
#[test]
fn qcow2_zero_clusters_and_subclusters() {
    if !(have("qemu-img") && have("qemu-io")) {
        return;
    }
    let dir = TempDir::new("qcow2-io");
    let raw = write_reference(&dir);
    let qemu_io = |image: &Path, cmd: &str| {
        run(Command::new("qemu-io")
            .args(["-f", "qcow2", "-c", cmd])
            .arg(image));
    };
    let mut want = reference().to_vec();
    // v3: zeros written over data become zero clusters (the data stays in the file).
    let zeroed = dir.path("zeroed.qcow2");
    qemu_img(&["-O", "qcow2"], &raw, &zeroed);
    qemu_io(&zeroed, "write -z 1M 64k");
    want[MIB..MIB + (64 << 10)].fill(0);
    // Extended L2: 4 KiB written into an unallocated 128 KiB cluster allocates one
    // subcluster; 8 KiB of zeros in an allocated one mark two as zeros.
    let sub = dir.path("sub.qcow2");
    qemu_img(
        &["-O", "qcow2", "-o", "extended_l2=on,cluster_size=128k"],
        &raw,
        &sub,
    );
    qemu_io(&sub, &format!("write -P 0xab {} 4k", 5 * MIB + 8192));
    qemu_io(&sub, "write -z 1M 64k");
    qemu_io(&sub, &format!("write -z {} 8k", 2 * MIB + 4096));
    let mut want_sub = want.clone();
    want_sub[5 * MIB + 8192..5 * MIB + 12_288].fill(0xab);
    want_sub[2 * MIB + 4096..2 * MIB + 12_288].fill(0);
    for (name, path, data) in [
        ("zero clusters", &zeroed, &want),
        ("subclusters", &sub, &want_sub),
    ] {
        let size = virtual_size(path, "qcow2");
        check(
            &format!("qcow2 ({name})"),
            path,
            Want {
                format: ImageFormat::Qcow2,
                raw_size: Some(size),
                data,
                len: size,
                sized: true,
            },
        );
    }
}

#[test]
fn virtual_disks_refused() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("refused");
    let raw = dir.path("small.img");
    fs::write(&raw, noise(5, 3 * MIB)).unwrap();

    // QCOW2 with a backing file, and encrypted.
    let base = dir.path("base.qcow2");
    qemu_img(&["-O", "qcow2"], &raw, &base);
    run(Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(&base)
        .arg(dir.path("overlay.qcow2")));
    check_refused(
        "qcow2 overlay",
        &dir.path("overlay.qcow2"),
        ImageFormat::Qcow2,
        &["backing", "overlay"],
    );
    run(Command::new("qemu-img")
        .args([
            "create",
            "--object",
            "secret,id=sec0,data=abc123",
            "-f",
            "qcow2",
            "-o",
            "encrypt.format=luks,encrypt.key-secret=sec0",
        ])
        .arg(dir.path("enc.qcow2"))
        .arg("4M"));
    run(Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o"])
        .arg(format!("data_file={}", dir.path("data.raw").display()))
        .arg(dir.path("external.qcow2"))
        .arg("4M"));
    check_refused(
        "qcow2 external data",
        &dir.path("external.qcow2"),
        ImageFormat::Qcow2,
        &["separate file"],
    );
    check_refused(
        "qcow2 encrypted",
        &dir.path("enc.qcow2"),
        ImageFormat::Qcow2,
        &["encrypted"],
    );

    // VMDK spread over files: the descriptor, and an extent of a split disk.
    run(Command::new("qemu-img")
        .args([
            "convert",
            "-f",
            "raw",
            "-O",
            "vmdk",
            "-o",
            "subformat=twoGbMaxExtentSparse",
        ])
        .arg(&raw)
        .arg(dir.path("split.vmdk")));
    check_refused(
        "vmdk split (descriptor)",
        &dir.path("split.vmdk"),
        ImageFormat::Vmdk,
        &["descriptor"],
    );
    check_refused(
        "vmdk split (extent)",
        &dir.path("split-s001.vmdk"),
        ImageFormat::Vmdk,
        &["split"],
    );
    run(Command::new("qemu-img")
        .args([
            "convert",
            "-f",
            "raw",
            "-O",
            "vmdk",
            "-o",
            "subformat=monolithicFlat",
        ])
        .arg(&raw)
        .arg(dir.path("flat.vmdk")));
    check_refused(
        "vmdk flat (descriptor)",
        &dir.path("flat.vmdk"),
        ImageFormat::Vmdk,
        &["flat.vmdk"],
    );

    // Differencing VHD: a dynamic one relabelled (type 4, both footer copies).
    let vhd = dir.path("diff.vhd");
    qemu_img(&["-O", "vpc", "-o", "subformat=dynamic"], &raw, &vhd);
    let mut b = fs::read(&vhd).unwrap();
    let end = b.len() - 512;
    for at in [0, end] {
        let footer = &mut b[at..at + 512];
        footer[60..64].copy_from_slice(&4u32.to_be_bytes());
        footer[64..68].fill(0);
        let sum: u32 = footer.iter().map(|&x| x as u32).sum();
        footer[64..68].copy_from_slice(&(!sum).to_be_bytes());
    }
    fs::write(&vhd, &b).unwrap();
    check_refused(
        "vhd differencing",
        &vhd,
        ImageFormat::Vhd,
        &["differencing"],
    );

    // Differencing VHDX: the "has parent" flag of the file parameters.
    let vhdx = dir.path("diff.vhdx");
    qemu_img(&["-O", "vhdx"], &raw, &vhdx);
    let mut b = fs::read(&vhdx).unwrap();
    let meta = vhdx_region(&b, &super::vhdx::METADATA_REGION);
    let entries = u16::from_le_bytes([b[meta + 10], b[meta + 11]]) as usize;
    let params = (0..entries)
        .map(|i| meta + 32 + i * 32)
        .find(|&e| b[e..e + 16] == super::vhdx::FILE_PARAMETERS)
        .unwrap();
    let at = meta + u32::from_le_bytes(b[params + 16..params + 20].try_into().unwrap()) as usize;
    b[at + 4] |= 2;
    fs::write(&vhdx, &b).unwrap();
    check_refused(
        "vhdx differencing",
        &vhdx,
        ImageFormat::Vhdx,
        &["differencing"],
    );
}

/// The file offset of a VHDX region.
fn vhdx_region(b: &[u8], guid: &[u8; 16]) -> usize {
    let t = &b[192 << 10..256 << 10];
    let count = u32::from_le_bytes(t[8..12].try_into().unwrap()) as usize;
    (0..count)
        .map(|i| &t[16 + i * 32..48 + i * 32])
        .find(|e| e[..16] == guid[..])
        .map(|e| u64::from_le_bytes(e[16..24].try_into().unwrap()) as usize)
        .unwrap()
}

/// A VHDX that wasn't closed cleanly: its log holds changes the file doesn't have yet (a
/// data sector and a range of zeros over payload data). They have to show up.
#[test]
fn vhdx_log_replay() {
    if !have("qemu-img") {
        return;
    }
    let dir = TempDir::new("vhdx-log");
    let raw = write_reference(&dir);
    let vhdx = dir.path("dirty.vhdx");
    qemu_img(&["-O", "vhdx", "-o", "subformat=fixed"], &raw, &vhdx);
    let size = virtual_size(&vhdx, "vhdx");
    let mut b = fs::read(&vhdx).unwrap();
    // The payload of block 0: from the BAT's first entry.
    let bat = vhdx_region(&b, &super::vhdx::BAT_REGION);
    let block0 = (u64::from_le_bytes(b[bat..bat + 8].try_into().unwrap()) & !0xfffff) as usize;
    // Log changes: a new 4 KiB at disk offset 8 KiB, and zeros over 12 KiB at 64 KiB.
    let page: Vec<u8> = noise(77, 4096);
    let mut want = reference().to_vec();
    want[8192..12_288].copy_from_slice(&page);
    want[65_536..65_536 + 12_288].fill(0);
    let log_guid = [7u8; 16];
    // Headers: the log's place, and its GUID, in the current (higher-sequence) one.
    let (mut log_offset, mut log_len) = (0u64, 0u32);
    for h in [64usize << 10, 128 << 10] {
        let seq = u64::from_le_bytes(b[h + 8..h + 16].try_into().unwrap());
        let _ = seq;
        log_len = u32::from_le_bytes(b[h + 68..h + 72].try_into().unwrap());
        log_offset = u64::from_le_bytes(b[h + 72..h + 80].try_into().unwrap());
        b[h + 48..h + 64].copy_from_slice(&log_guid);
        seal_vhdx(&mut b[h..h + 4096]);
    }
    assert!(log_len >= 1 << 20);
    // One entry: header + 2 descriptors in the first sector, one data sector.
    let seq = 5u64;
    let mut entry = vec![0u8; 8192];
    entry[..4].copy_from_slice(b"loge");
    entry[8..12].copy_from_slice(&8192u32.to_le_bytes());
    entry[12..16].copy_from_slice(&0u32.to_le_bytes()); // tail: itself
    entry[16..24].copy_from_slice(&seq.to_le_bytes());
    entry[24..28].copy_from_slice(&2u32.to_le_bytes());
    entry[32..48].copy_from_slice(&log_guid);
    entry[48..56].copy_from_slice(&(b.len() as u64).to_le_bytes());
    entry[56..64].copy_from_slice(&(b.len() as u64).to_le_bytes());
    // A data descriptor: the first 8 and the last 4 bytes of the sector live here.
    let d = &mut entry[64..96];
    d[..4].copy_from_slice(b"desc");
    d[4..8].copy_from_slice(&page[4092..]);
    d[8..16].copy_from_slice(&page[..8]);
    d[16..24].copy_from_slice(&((block0 + 8192) as u64).to_le_bytes());
    d[24..32].copy_from_slice(&seq.to_le_bytes());
    let z = &mut entry[96..128];
    z[..4].copy_from_slice(b"zero");
    z[8..16].copy_from_slice(&12_288u64.to_le_bytes());
    z[16..24].copy_from_slice(&((block0 + 65_536) as u64).to_le_bytes());
    z[24..32].copy_from_slice(&seq.to_le_bytes());
    let s = &mut entry[4096..8192];
    s[..4].copy_from_slice(b"data");
    s[4..8].copy_from_slice(&((seq >> 32) as u32).to_le_bytes());
    s[8..4092].copy_from_slice(&page[8..4092]);
    s[4092..].copy_from_slice(&(seq as u32).to_le_bytes());
    let crc = super::source::crc32c(0, &entry);
    entry[4..8].copy_from_slice(&crc.to_le_bytes());
    let at = log_offset as usize;
    b[at..at + 8192].copy_from_slice(&entry);
    fs::write(&vhdx, &b).unwrap();
    check(
        "vhdx (dirty log)",
        &vhdx,
        Want {
            format: ImageFormat::Vhdx,
            // The size isn't vouched for until the log has been replayed.
            raw_size: None,
            data: &want,
            len: size,
            sized: true,
        },
    );
}

/// Sets a VHDX structure's CRC-32C (at offset 4, over all of it).
fn seal_vhdx(b: &mut [u8]) {
    b[4..8].fill(0);
    let crc = super::source::crc32c(0, b);
    b[4..8].copy_from_slice(&crc.to_le_bytes());
}

// ---------------------------------------------------------------------------------------
// DMG

const DMG_ZERO: u32 = 0;
const DMG_RAW: u32 = 1;
const DMG_IGNORE: u32 = 2;
const DMG_ADC: u32 = 0x8000_0004;
const DMG_ZLIB: u32 = 0x8000_0005;
const DMG_BZIP2: u32 = 0x8000_0006;
const DMG_LZFSE: u32 = 0x8000_0007;
const DMG_LZMA: u32 = 0x8000_0008;
const DMG_COMMENT: u32 = 0x7fff_fffe;
const DMG_END: u32 = 0xffff_ffff;

/// How the test UDIF writer lays a disk out.
struct Udif {
    /// Chunk kinds to rotate through (all-zero chunks become ZERO or IGNORE).
    kinds: Vec<u32>,
    /// Sectors per chunk.
    chunk_sectors: usize,
    /// Partitions: their sizes in sectors (the last one takes the rest).
    partitions: Vec<usize>,
    /// Blocks in an old-style resource fork instead of the XML.
    resource_fork: bool,
    /// Spoil a partition's checksum.
    wrong_crc: bool,
}

impl Udif {
    fn new(kinds: &[u32]) -> Udif {
        Udif {
            kinds: kinds.to_vec(),
            chunk_sectors: 2048,
            partitions: vec![1, 33, 2000],
            resource_fork: false,
            wrong_crc: false,
        }
    }

    /// Writes `disk` (padded to whole sectors) as a UDIF image.
    fn write(&self, disk: &[u8], path: &Path) {
        let sectors = disk.len().div_ceil(512);
        let mut padded = disk.to_vec();
        padded.resize(sectors * 512, 0);
        let mut fork = Vec::new(); // the data fork
        let mut blocks = Vec::new(); // (name, mish)
        let mut first = 0usize;
        let mut kind_at = 0usize;
        for (p, &want) in self.partitions.iter().chain([&usize::MAX]).enumerate() {
            if first >= sectors {
                break;
            }
            let count = want.min(sectors - first);
            let data_offset = if p % 2 == 1 { fork.len() as u64 } else { 0 };
            let mut chunks = vec![(DMG_COMMENT, 0u64, 0u64, 0u64, 0u64)];
            let mut crc = flate2::Crc::new();
            let mut s = 0;
            while s < count {
                let n = self.chunk_sectors.min(count - s);
                let data = &padded[(first + s) * 512..(first + s + n) * 512];
                let zeros = data.iter().all(|&b| b == 0);
                let mut kind = self.kinds[kind_at % self.kinds.len()];
                kind_at += 1;
                if zeros && kind_at.is_multiple_of(3) {
                    kind = if kind_at.is_multiple_of(2) {
                        DMG_ZERO
                    } else {
                        DMG_IGNORE
                    };
                }
                if !zeros && (kind == DMG_ZERO || kind == DMG_IGNORE) {
                    kind = DMG_ZLIB;
                }
                if kind != DMG_IGNORE {
                    crc.update(data);
                }
                let packed = match kind {
                    DMG_ZERO | DMG_IGNORE => Vec::new(),
                    DMG_RAW => data.to_vec(),
                    DMG_ZLIB => {
                        let mut z = flate2::write::ZlibEncoder::new(
                            Vec::new(),
                            flate2::Compression::fast(),
                        );
                        z.write_all(data).unwrap();
                        z.finish().unwrap()
                    }
                    DMG_BZIP2 => {
                        let mut z =
                            bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                        z.write_all(data).unwrap();
                        z.finish().unwrap()
                    }
                    DMG_LZFSE => {
                        let mut out = Vec::new();
                        lzfse_rust::encode_bytes(data, &mut out).unwrap();
                        out
                    }
                    DMG_LZMA => xz_compress(data),
                    DMG_ADC => adc_compress(data),
                    other => panic!("kind {other:#x}"),
                };
                let at = fork.len() as u64 - data_offset;
                chunks.push((
                    kind,
                    s as u64,
                    n as u64,
                    if packed.is_empty() { 0 } else { at },
                    packed.len() as u64,
                ));
                fork.extend(&packed);
                s += n;
            }
            chunks.push((DMG_COMMENT, count as u64, 0, 0, 0));
            chunks.push((DMG_END, count as u64, 0, fork.len() as u64 - data_offset, 0));
            let mut mish = Vec::new();
            mish.extend(b"mish");
            mish.extend(1u32.to_be_bytes());
            mish.extend((first as u64).to_be_bytes());
            mish.extend((count as u64).to_be_bytes());
            mish.extend(data_offset.to_be_bytes());
            mish.extend(0x208u32.to_be_bytes()); // buffers needed
            mish.extend((p as i32 - 1).to_be_bytes()); // block descriptor
            mish.extend([0u8; 24]);
            mish.extend(2u32.to_be_bytes()); // CRC-32
            mish.extend(32u32.to_be_bytes());
            let sum = crc.sum() ^ u32::from(self.wrong_crc && p == 2);
            mish.extend(sum.to_be_bytes());
            mish.extend([0u8; 124]);
            mish.extend((chunks.len() as u32).to_be_bytes());
            for (kind, sector, n, offset, len) in chunks {
                mish.extend(kind.to_be_bytes());
                mish.extend(if kind == DMG_COMMENT {
                    *b"+beg"
                } else {
                    [0; 4]
                });
                mish.extend(sector.to_be_bytes());
                mish.extend(n.to_be_bytes());
                mish.extend(offset.to_be_bytes());
                mish.extend(len.to_be_bytes());
            }
            assert_eq!(mish.len(), 204 + (mish.len() - 204) / 40 * 40);
            blocks.push((format!("partition {p} (Apple_HFS : {p})"), mish));
            first += count;
        }
        let mut file = fork.clone();
        let (mut xml_offset, mut xml_len, mut rsrc_offset, mut rsrc_len) = (0u64, 0u64, 0u64, 0u64);
        if self.resource_fork {
            let rsrc = resource_fork(&blocks);
            rsrc_offset = file.len() as u64;
            rsrc_len = rsrc.len() as u64;
            file.extend(rsrc);
        } else {
            let xml = plist_xml(&blocks);
            xml_offset = file.len() as u64;
            xml_len = xml.len() as u64;
            file.extend(xml.as_bytes());
        }
        let mut koly = vec![0u8; 512];
        koly[..4].copy_from_slice(b"koly");
        koly[4..8].copy_from_slice(&4u32.to_be_bytes());
        koly[8..12].copy_from_slice(&512u32.to_be_bytes());
        koly[12..16].copy_from_slice(&1u32.to_be_bytes()); // flags
        koly[32..40].copy_from_slice(&(fork.len() as u64).to_be_bytes());
        koly[40..48].copy_from_slice(&rsrc_offset.to_be_bytes());
        koly[48..56].copy_from_slice(&rsrc_len.to_be_bytes());
        koly[56..60].copy_from_slice(&1u32.to_be_bytes()); // segment 1
        koly[60..64].copy_from_slice(&1u32.to_be_bytes()); // of 1
        koly[216..224].copy_from_slice(&xml_offset.to_be_bytes());
        koly[224..232].copy_from_slice(&xml_len.to_be_bytes());
        koly[488..492].copy_from_slice(&1u32.to_be_bytes()); // image variant
        koly[492..500].copy_from_slice(&(sectors as u64).to_be_bytes());
        file.extend(koly);
        fs::write(path, file).unwrap();
    }
}

/// The table of contents as macOS writes it: base64 in <data>, wrapped and indented.
fn plist_xml(blocks: &[(String, Vec<u8>)]) -> String {
    use std::fmt::Write as _;
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>resource-fork</key>\n\t<dict>\n\t\t<key>blkx</key>\n\t\t<array>\n",
    );
    for (i, (name, mish)) in blocks.iter().enumerate() {
        let b64 = base64(mish);
        writeln!(x, "\t\t\t<dict>\n\t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\t\t\t\t<key>CFName</key>\n\t\t\t\t<string>{name}</string>\n\t\t\t\t<key>Data</key>\n\t\t\t\t<data>").unwrap();
        for line in b64.as_bytes().chunks(52) {
            writeln!(x, "\t\t\t\t{}", std::str::from_utf8(line).unwrap()).unwrap();
        }
        writeln!(x, "\t\t\t\t</data>\n\t\t\t\t<key>ID</key>\n\t\t\t\t<string>{}</string>\n\t\t\t\t<key>Name</key>\n\t\t\t\t<string>{name}</string>\n\t\t\t</dict>", i as i32 - 1).unwrap();
    }
    x.push_str("\t\t</array>\n\t\t<key>plst</key>\n\t\t<array>\n\t\t\t<dict>\n\t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\t\t\t\t<key>Data</key>\n\t\t\t\t<data>\n\t\t\t\tAAAAAA==\n\t\t\t\t</data>\n\t\t\t\t<key>ID</key>\n\t\t\t\t<string>0</string>\n\t\t\t\t<key>Name</key>\n\t\t\t\t<string></string>\n\t\t\t</dict>\n\t\t</array>\n\t</dict>\n</dict>\n</plist>\n");
    x
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= c.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// A classic resource fork holding the blkx resources (the layout 7-Zip reads).
fn resource_fork(blocks: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for (_, mish) in blocks {
        offsets.push(data.len() as u32);
        data.extend((mish.len() as u32).to_be_bytes());
        data.extend(mish);
    }
    let mut names = Vec::new();
    let mut name_at = Vec::new();
    for (name, _) in blocks {
        name_at.push(names.len() as u16);
        names.push(name.len() as u8);
        names.extend(name.as_bytes());
    }
    // The map: a copy of the header, 12 bytes, then the type list at 0x1c.
    let mut map = vec![0u8; 0x1c];
    let types_len = 2 + 8;
    let refs_at = types_len as u16;
    map.extend(0u16.to_be_bytes()); // one type
    map.extend(*b"blkx");
    map.extend(((blocks.len() - 1) as u16).to_be_bytes());
    map.extend(refs_at.to_be_bytes());
    for (i, off) in offsets.iter().enumerate() {
        map.extend(((i as i16) - 1).to_be_bytes()); // resource ID
        map.extend(name_at[i].to_be_bytes());
        map.extend((0x5000_0000u32 | off).to_be_bytes()); // attributes, data offset
        map.extend(0u32.to_be_bytes());
    }
    let names_offset = map.len() as u16;
    map.extend(&names);
    map[0x18..0x1a].copy_from_slice(&0x1cu16.to_be_bytes());
    map[0x1a..0x1c].copy_from_slice(&names_offset.to_be_bytes());
    let mut header = [0u8; 16];
    header[..4].copy_from_slice(&0x100u32.to_be_bytes());
    header[4..8].copy_from_slice(&(0x100 + data.len() as u32).to_be_bytes());
    header[8..12].copy_from_slice(&(data.len() as u32).to_be_bytes());
    header[12..16].copy_from_slice(&(map.len() as u32).to_be_bytes());
    map[..16].copy_from_slice(&header);
    let mut fork = vec![0u8; 0x100];
    fork[..16].copy_from_slice(&header);
    fork.extend(data);
    fork.extend(map);
    fork
}

/// xz, as ULMO chunks hold it (the xz tool, else a zlib stand-in the test notices).
fn xz_compress(data: &[u8]) -> Vec<u8> {
    let mut child = Command::new("xz")
        .args(["-c", "-0", "--check=crc32"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("xz");
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap().unwrap();
    assert!(out.status.success());
    out.stdout
}

/// A simple ADC encoder: literal runs, and matches at distance 1 and at the distance of
/// the last repeat seen (short and long codes both).
fn adc_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let flush = |lit: &mut Vec<u8>, out: &mut Vec<u8>| {
        for c in lit.chunks(128) {
            out.push(0x80 | (c.len() - 1) as u8);
            out.extend(c);
        }
        lit.clear();
    };
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        for dist in [1usize, 83, 251, 1000] {
            if dist > i {
                continue;
            }
            let max = (data.len() - i).min(67);
            let n = (0..max)
                .take_while(|&k| data[i + k] == data[i + k - dist])
                .count();
            if n > best.0 {
                best = (n, dist);
            }
        }
        let (n, dist) = best;
        if n >= 4 && dist <= 1024 && n <= 18 && i % 2 == 0 {
            flush(&mut lit, &mut out);
            out.push((((n - 3) as u8) << 2) | ((dist - 1) >> 8) as u8);
            out.push((dist - 1) as u8);
            i += n;
        } else if n >= 4 {
            flush(&mut lit, &mut out);
            out.push(0x40 | (n - 4) as u8);
            out.extend(((dist - 1) as u16).to_be_bytes());
            i += n;
        } else {
            lit.push(data[i]);
            i += 1;
        }
    }
    flush(&mut lit, &mut out);
    out
}

#[test]
fn dmg_every_kind_of_chunk() {
    let dir = TempDir::new("dmg");
    let mut kinds = vec![
        DMG_ZLIB, DMG_RAW, DMG_BZIP2, DMG_LZFSE, DMG_ADC, DMG_ZERO, DMG_IGNORE,
    ];
    if have("xz") {
        kinds.push(DMG_LZMA);
    }
    let sectors = REF_LEN.div_ceil(512) as u64;
    let want = || Want::reference(ImageFormat::Dmg, Some(sectors * 512), sectors * 512);
    let dmg = dir.path("mixed.dmg");
    Udif::new(&kinds).write(reference(), &dmg);
    check("dmg (every kind of chunk)", &dmg, want());

    // One kind at a time, for their speeds.
    let part_sectors = part().len().div_ceil(512) as u64;
    for (name, kind) in [
        ("UDZO zlib", DMG_ZLIB),
        ("UDBZ bzip2", DMG_BZIP2),
        ("ULFO lzfse", DMG_LZFSE),
        ("ULMO lzma", DMG_LZMA),
        ("UDCO adc", DMG_ADC),
        ("UDRO raw", DMG_RAW),
    ] {
        if kind == DMG_LZMA && !have("xz") {
            continue;
        }
        let path = dir.path(&format!("{kind:x}.dmg"));
        Udif::new(&[kind]).write(part(), &path);
        check(
            &format!("dmg ({name})"),
            &path,
            Want {
                format: ImageFormat::Dmg,
                raw_size: Some(part_sectors * 512),
                data: part(),
                len: part_sectors * 512,
                sized: true,
            },
        );
        fs::remove_file(&path).unwrap();
    }

    // The blocks in a resource fork instead of the XML.
    let rsrc = dir.path("rsrc.dmg");
    let mut udif = Udif::new(&[DMG_ZLIB, DMG_RAW]);
    udif.resource_fork = true;
    udif.write(reference(), &rsrc);
    check("dmg (resource fork)", &rsrc, want());
}

#[test]
fn dmg_damage_is_found() {
    let dir = TempDir::new("dmg-bad");
    let disk = noise(6, 3 * MIB + 1000);
    // A wrong checksum.
    let path = dir.path("crc.dmg");
    let mut udif = Udif::new(&[DMG_ZLIB]);
    udif.wrong_crc = true;
    udif.write(&disk, &path);
    let mut r = open(&path, ImageFormat::Dmg).unwrap().reader;
    let err = io::copy(&mut r, &mut io::sink()).unwrap_err();
    assert!(err.to_string().contains("checksum"), "{err}");
    // Damaged compressed data (or, if it still decodes, a checksum mismatch).
    let path = dir.path("zlib.dmg");
    Udif::new(&[DMG_ZLIB]).write(&disk, &path);
    let mut b = fs::read(&path).unwrap();
    b[1_500_000] ^= 0x40;
    fs::write(&path, &b).unwrap();
    let mut r = open(&path, ImageFormat::Dmg).unwrap().reader;
    assert!(io::copy(&mut r, &mut io::sink()).is_err());
    // Encrypted, sparse, segmented: refused with an explanation.
    let mut enc = b"encrcdsa".to_vec();
    enc.extend(noise(7, 100_000));
    fs::write(dir.path("enc.dmg"), &enc).unwrap();
    check_refused(
        "dmg encrypted",
        &dir.path("enc.dmg"),
        ImageFormat::Dmg,
        &["encrypted"],
    );
    let mut sparse = b"sprs".to_vec();
    sparse.extend(3u32.to_be_bytes());
    sparse.extend(noise(8, 100_000));
    fs::write(dir.path("x.sparseimage"), &sparse).unwrap();
    check_refused(
        "dmg sparse image",
        &dir.path("x.sparseimage"),
        ImageFormat::Dmg,
        &["sparse"],
    );
    let path = dir.path("segment.dmg");
    Udif::new(&[DMG_ZLIB]).write(&disk, &path);
    let mut b = fs::read(&path).unwrap();
    let koly = b.len() - 512;
    b[koly + 60..koly + 64].copy_from_slice(&2u32.to_be_bytes());
    fs::write(&path, &b).unwrap();
    check_refused("dmg segmented", &path, ImageFormat::Dmg, &["segment"]);
}

/// Real disk images from hdiutil, in every compression it offers.
#[cfg(target_os = "macos")]
#[test]
fn dmg_from_hdiutil() {
    let dir = TempDir::new("hdiutil");
    let hdiutil = |args: &[&str]| {
        let out = Command::new("hdiutil").args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let base = dir.path("x.dmg");
    let base_s = base.to_str().unwrap();
    let (ok, err) = hdiutil(&[
        "create", "-size", "40m", "-fs", "HFS+", "-volname", "T", base_s,
    ]);
    assert!(ok, "hdiutil create: {err}");
    // Some files in it: random data and text.
    let mnt = dir.path("mnt");
    fs::create_dir_all(&mnt).unwrap();
    let (ok, err) = hdiutil(&[
        "attach",
        "-nobrowse",
        "-mountpoint",
        mnt.to_str().unwrap(),
        base_s,
    ]);
    assert!(ok, "hdiutil attach: {err}");
    fs::write(mnt.join("random.bin"), noise(10, 12 * MIB)).unwrap();
    fs::write(mnt.join("text.txt"), b"hello DD-GUI ".repeat(300_000)).unwrap();
    let (ok, err) = hdiutil(&["detach", mnt.to_str().unwrap()]);
    assert!(ok, "hdiutil detach: {err}");
    // The raw disk, as hdiutil sees it.
    let raw = dir.path("raw");
    let (ok, err) = hdiutil(&[
        "convert",
        base_s,
        "-format",
        "UDTO",
        "-o",
        raw.to_str().unwrap(),
    ]);
    assert!(ok, "hdiutil convert UDTO: {err}");
    let raw = fs::read(dir.path("raw.cdr")).unwrap();
    for format in ["UDZO", "UDBZ", "ULFO", "ULMO", "UDRO", "UDCO"] {
        let out = dir.path(&format!("{format}.dmg"));
        let (ok, err) = hdiutil(&[
            "convert",
            base_s,
            "-format",
            format,
            "-o",
            out.to_str().unwrap(),
        ]);
        if !ok {
            eprintln!("note: hdiutil can't make {format} here: {err}");
            continue;
        }
        check(
            &format!("dmg from hdiutil ({format})"),
            &out,
            Want {
                format: ImageFormat::Dmg,
                raw_size: Some(raw.len() as u64),
                data: &raw,
                len: raw.len() as u64,
                sized: true,
            },
        );
    }
    // Encrypted and sparse images are refused.
    let enc = dir.path("enc.dmg");
    let mut child = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "5m",
            "-fs",
            "HFS+",
            "-encryption",
            "AES-128",
            "-stdinpass",
        ])
        .arg(&enc)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"secret").unwrap();
    if child.wait().unwrap().success() {
        check_refused(
            "dmg encrypted (hdiutil)",
            &enc,
            ImageFormat::Dmg,
            &["encrypted"],
        );
    }
    let sparse = dir.path("s.sparseimage");
    let (ok, _) = hdiutil(&[
        "create",
        "-size",
        "5m",
        "-fs",
        "HFS+",
        "-type",
        "SPARSE",
        sparse.to_str().unwrap(),
    ]);
    if ok {
        check_refused(
            "dmg sparse (hdiutil)",
            &sparse,
            ImageFormat::Dmg,
            &["sparse"],
        );
    }
}

// ---------------------------------------------------------------------------------------
// Detection

/// Raw disk images, and the formats the engine decodes itself, aren't claimed.
#[test]
fn no_false_positives() {
    let dir = TempDir::new("raw");
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("reference", reference()[..4 * MIB].to_vec()),
        ("zeros", vec![0u8; MIB]),
        ("tiny", vec![0x5d]),
        ("empty", vec![]),
    ];
    // Starts like an .lzma file: valid properties, dictionary and size, a zero byte.
    let mut lzma_like = vec![
        0x5d, 0, 0, 0x80, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0,
    ];
    lzma_like.extend(noise(11, 2 * MIB));
    cases.push(("lzma-like", lzma_like));
    // The same with a size, and with the dictionary sizes a GPT disk's zeroed boot code
    // or an x86 boot sector would give.
    let mut lzma_like_sized = vec![0x5d, 0, 0, 0x10, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0];
    lzma_like_sized.extend(noise(14, 100_000));
    cases.push(("lzma-like, sized", lzma_like_sized));
    let mut zero_dict = vec![0x5d, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0];
    zero_dict.extend(vec![0u8; 100_000]);
    cases.push(("lzma-like, no dictionary", zero_dict));
    // An MBR with boot code and a partition table.
    let mut mbr = noise(12, MIB);
    mbr[446..462].copy_from_slice(&[
        0x80, 1, 1, 0, 0x83, 0xfe, 0xff, 0xff, 0x3f, 0, 0, 0, 0, 0x10, 0, 0,
    ]);
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    cases.push(("mbr", mbr));
    // Names of formats inside, but not where they count.
    let mut inside = vec![0u8; MIB];
    inside[4096..4104].copy_from_slice(b"conectix");
    inside[8192..8196].copy_from_slice(b"koly");
    inside[100..108].copy_from_slice(b"vhdxfile");
    cases.push(("magic words inside", inside));
    for (name, data) in &cases {
        let path = dir.path(name);
        fs::write(&path, data).unwrap();
        assert!(detect_file(&path).unwrap().is_none(), "{name}: claimed");
    }
    // The engine's own formats.
    let raw = dir.path("reference");
    for (tool, args, name) in [
        ("gzip", &["-1", "-c"][..], "x.gz"),
        ("xz", &["-0", "-c"][..], "x.xz"),
        ("zstd", &["-q", "-c"][..], "x.zst"),
    ] {
        if have(tool) {
            run_to(Command::new(tool).args(args).arg(&raw), &dir.path(name));
            assert!(
                detect_file(&dir.path(name)).unwrap().is_none(),
                "{name}: claimed"
            );
        }
    }
    if have("zstd") {
        // A skippable frame first (as pzstd writes) is zstd's too.
        let path = dir.path("skip.zst");
        fs::write(&path, [0x50, 0x2a, 0x4d, 0x18, 2, 0, 0, 0, 1, 2]).unwrap();
        run_to(Command::new("zstd").args(["-q", "-c"]).arg(&raw), &path);
        assert!(
            detect_file(&path).unwrap().is_none(),
            "skippable zstd: claimed"
        );
    }
    let zip_path = dir.path("x.zip");
    let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
    zip.start_file("a.img", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&reference()[..MIB]).unwrap();
    zip.finish().unwrap();
    assert!(detect_file(&zip_path).unwrap().is_none(), "zip: claimed");
}

// ---------------------------------------------------------------------------------------
// Damaged files

/// Damaged and cut-short images give errors or data, never panics, hangs or huge
/// allocations. (Small images, bytes changed at random in and around their tables.)
#[test]
fn damaged_files_dont_panic() {
    let dir = TempDir::new("fuzz");
    let small = noise(13, 3 * MIB + 777);
    let mut disk = small.clone();
    disk[MIB..2 * MIB].fill(0);
    let raw = dir.path("small.img");
    fs::write(&raw, &disk).unwrap();
    let mut fixtures: Vec<(PathBuf, ImageFormat)> = Vec::new();
    let dmg = dir.path("small.dmg");
    Udif::new(&[DMG_ZLIB, DMG_RAW, DMG_BZIP2, DMG_LZFSE, DMG_ADC]).write(&disk, &dmg);
    fixtures.push((dmg, ImageFormat::Dmg));
    if have("qemu-img") {
        for (name, format, args) in [
            ("d.vhd", ImageFormat::Vhd, &["-O", "vpc"][..]),
            ("d.vhdx", ImageFormat::Vhdx, &["-O", "vhdx"][..]),
            ("s.vmdk", ImageFormat::Vmdk, &["-O", "vmdk"][..]),
            (
                "so.vmdk",
                ImageFormat::Vmdk,
                &["-O", "vmdk", "-o", "subformat=streamOptimized"][..],
            ),
            ("c.qcow2", ImageFormat::Qcow2, &["-O", "qcow2", "-c"][..]),
            (
                "e.qcow2",
                ImageFormat::Qcow2,
                &["-O", "qcow2", "-o", "extended_l2=on"][..],
            ),
        ] {
            qemu_img(args, &raw, &dir.path(name));
            fixtures.push((dir.path(name), format));
        }
    }
    if have("bzip2") {
        run_to(
            Command::new("bzip2").args(["-c", "-1"]).arg(&raw),
            &dir.path("x.bz2"),
        );
        fixtures.push((dir.path("x.bz2"), ImageFormat::Bzip2));
    }
    if have("lz4") {
        run_to(
            Command::new("lz4").args(["-q", "-c", "-BD"]).arg(&raw),
            &dir.path("x.lz4"),
        );
        fixtures.push((dir.path("x.lz4"), ImageFormat::Lz4));
    }
    if have("xz") {
        run_to(
            Command::new("xz")
                .args(["--format=lzma", "-c", "-0"])
                .arg(&raw),
            &dir.path("x.lzma"),
        );
        fixtures.push((dir.path("x.lzma"), ImageFormat::Lzma));
    }
    if have("tar") {
        run(Command::new("tar")
            .arg("-cf")
            .arg(dir.path("x.tar"))
            .arg("-C")
            .arg(&dir.0)
            .arg("small.img"));
        fixtures.push((dir.path("x.tar"), ImageFormat::Tar));
    }
    if have("7z") {
        run(Command::new("7z")
            .args(["a", "-bd", "-mx=1"])
            .arg(dir.path("x.7z"))
            .arg(&raw));
        fixtures.push((dir.path("x.7z"), ImageFormat::SevenZ));
    }
    let mut seed = 99u64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mutant = dir.path("mutant");
    // More with DDGUI_TEST_FUZZ_ROUNDS.
    let rounds = std::env::var("DDGUI_TEST_FUZZ_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(40);
    for (path, format) in &fixtures {
        let original = fs::read(path).unwrap();
        for round in 0..rounds {
            let mut b = original.clone();
            match round % 4 {
                // Cut short.
                0 => b.truncate((rand() % b.len() as u64) as usize),
                // Bytes changed in the first 64 KiB (headers and tables).
                1 => {
                    for _ in 0..8 {
                        let at = (rand() % b.len().min(64 << 10) as u64) as usize;
                        b[at] = rand() as u8;
                    }
                }
                // … in the last 64 KiB (trailers, directories at the end).
                2 => {
                    for _ in 0..8 {
                        let at = b.len() - 1 - (rand() % b.len().min(64 << 10) as u64) as usize;
                        b[at] = rand() as u8;
                    }
                }
                // … anywhere.
                _ => {
                    for _ in 0..16 {
                        let at = (rand() % b.len() as u64) as usize;
                        b[at] ^= 1 << (rand() % 8);
                    }
                }
            }
            fs::write(&mutant, &b).unwrap();
            let _ = detect_file(&mutant);
            if let Ok(mut opened) = open(&mutant, *format) {
                // Read at most a little more than the disk: a changed size could claim
                // terabytes of zeros.
                let mut limited = (&mut opened.reader).take(64 * MIB as u64);
                let _ = io::copy(&mut limited, &mut io::sink());
            }
        }
    }
}

/// Decoding speed on a real image: `DDGUI_TEST_IMAGE=path cargo test --release --bin dd-gui
/// formats::tests::throughput_of_a_file -- --ignored --nocapture`.
#[test]
#[ignore]
fn throughput_of_a_file() {
    let Some(path) = std::env::var_os("DDGUI_TEST_IMAGE").map(PathBuf::from) else {
        eprintln!("note: set DDGUI_TEST_IMAGE to an image file");
        return;
    };
    let started = Instant::now();
    let found = detect_file(&path)
        .unwrap()
        .expect("a format of this module");
    eprintln!(
        "{:?}, raw size {:?}, detected in {:.3} s",
        found.format,
        found.raw_size,
        started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    let mut reader = open(&path, found.format).unwrap().reader;
    let n = io::copy(&mut reader, &mut io::sink()).unwrap();
    let secs = started.elapsed().as_secs_f64();
    eprintln!(
        "{:.1} MB in {secs:.3} s: {:.0} MB/s",
        n as f64 / 1e6,
        n as f64 / 1e6 / secs
    );
}
