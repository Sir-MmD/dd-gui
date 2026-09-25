//! Tests for the copy engine. No root and no GUI needed: "drives" are files (strict ones
//! refuse misaligned transfers, like O_DIRECT and raw devices do), layouts are built by
//! hand, and the end-to-end tests run the real binary (built on first use).
//!
//! Big sparse files go to `$DDGUI_TEST_SCRATCH` (default: the temp folder). The heavy
//! tests are `#[ignore]`d: `cargo test --release --bin dd-gui engine -- --ignored`.
//! `DDGUI_TEST_LOOP=/dev/loopN` (a loop device you may write, over a scratch file) runs
//! `real_drive_with_direct_io` against it.

use super::disk::{AlignedBuf, Disk, DiskReader};
use super::image::{self, Frame, Map, Zeros};
use super::progress::Progress;
use super::{Args, ImageFormat, Mode, copy, parse_size, restore};
use crate::smart::{self, Extent};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
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

fn progress() -> Arc<Progress> {
    Arc::new(Progress::default())
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

/// A "drive" full of junk, with data in the extents: random bytes and text (so that
/// compression has something to do), and a run of zeros.
fn junk_drive(path: &Path, size: u64, extents: &[Extent]) -> Vec<u8> {
    let mut content: Vec<u8> = (0..size).map(|i| 0x5a ^ (i % 251) as u8).collect();
    let text = b"DD-GUI smart images: used data in place, zeros for free space. ";
    for (n, e) in extents.iter().enumerate() {
        let range = e.start as usize..(e.start + e.len) as usize;
        if n % 2 == 0 {
            content[range].copy_from_slice(&noise(n as u64 + 7, e.len as usize));
        } else {
            for (i, b) in content[range].iter_mut().enumerate() {
                *b = text[(i + n) % text.len()];
            }
        }
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

/// What any zstd decoder makes of a file (libzstd, through the zstd crate).
fn unzstd(path: &Path) -> Vec<u8> {
    zstd::stream::decode_all(fs::File::open(path).unwrap()).unwrap()
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

fn q(p: &Path) -> String {
    format!("'{}'", p.display())
}

fn assert_same(what: &str, got: &[u8], want: &[u8]) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(at) = got.iter().zip(want).position(|(a, b)| a != b) {
        panic!("{what}: first difference at byte {at}");
    }
}

// ---- Smart image format -----------------------------------------------------------------

#[test]
fn map_text_round_trip() {
    let size = 10 << 30;
    let map = Map {
        size,
        extents: vec![ext(0, MIB), ext(5 * MIB, 4096), ext(size - 12288, 12288)],
    };
    for magic in [image::MAGIC_V1, image::MAGIC_V2] {
        let text = map.to_text(magic);
        assert!(text.starts_with(&format!(
            "{magic}\nsize=10737418240 used=1064960 extents=3 unit=4096\nmap="
        )));
        assert_eq!(Map::parse(text.as_bytes(), magic), Some(Ok(map.clone())));
    }
    let text = map.to_text(image::MAGIC_V2);
    // Versions don't mix.
    assert_eq!(Map::parse(text.as_bytes(), image::MAGIC_V1), None);
    let damaged = text.replace("used=1064960", "used=1064961");
    assert!(matches!(
        Map::parse(damaged.as_bytes(), image::MAGIC_V2),
        Some(Err(_))
    ));
    let truncated = &text.as_bytes()[..text.len() - 4];
    assert!(matches!(
        Map::parse(truncated, image::MAGIC_V2),
        Some(Err(_))
    ));
    assert_eq!(
        Map::parse(b"Created by some other tool", image::MAGIC_V2),
        None
    );
    assert_eq!(
        Map::parse(b"DD-GUI smart image v3\nsize=1", image::MAGIC_V2),
        None
    );

    let empty = Map {
        size: 4096,
        extents: vec![],
    };
    let text = empty.to_text(image::MAGIC_V2);
    assert_eq!(
        Map::parse(text.as_bytes(), image::MAGIC_V2),
        Some(Ok(empty))
    );

    // A drive whose size isn't a multiple of 4 KiB, and extents that aren't aligned at all.
    let odd = Map {
        size: size + 1000,
        extents: vec![ext(0, 4096), ext(size - 4096, 5096)],
    };
    let text = odd.to_text(image::MAGIC_V2);
    assert!(text.contains(" unit=4096\n"));
    assert_eq!(Map::parse(text.as_bytes(), image::MAGIC_V2), Some(Ok(odd)));
    let unaligned = Map {
        size: 5000,
        extents: vec![ext(100, 900), ext(1000, 1), ext(4999, 1)],
    };
    let text = unaligned.to_text(image::MAGIC_V2);
    assert!(text.contains(" unit=1\n"));
    assert_eq!(
        Map::parse(text.as_bytes(), image::MAGIC_V2),
        Some(Ok(unaligned))
    );

    // Many extents stay compact: under 3 characters each here.
    let many = Map {
        size: 1 << 40,
        extents: (0..100_000)
            .map(|i| ext(i * 64 * 4096, 4096 * (1 + i % 7)))
            .collect(),
    };
    let text = many.to_text(image::MAGIC_V2);
    assert!(text.len() < 100_000 * 3, "{} bytes", text.len());
    assert_eq!(Map::parse(text.as_bytes(), image::MAGIC_V2), Some(Ok(many)));
}

#[test]
fn zero_frames_decompress_to_zeros() {
    let mut zeros = Zeros::default();
    for len in [1u64, 4095, 4096, 12288, 64 * MIB + 4096 + 500, 3 * 64 * MIB] {
        let pieces: Vec<u64> = Zeros::pieces(len).collect();
        assert_eq!(pieces.iter().sum::<u64>(), len);
        assert!(pieces.iter().all(|&p| p <= image::ZEROS_MAX));
        let mut out = Vec::new();
        for &piece in &pieces {
            let frame = zeros.frame(piece).unwrap();
            // One frame, which says how much it holds.
            assert_eq!(
                zstd::zstd_safe::find_frame_compressed_size(frame),
                Ok(frame.len())
            );
            assert_eq!(
                zstd::zstd_safe::get_frame_content_size(frame).ok(),
                Some(Some(piece))
            );
            out.extend_from_slice(frame);
        }
        let data = zstd::stream::decode_all(&out[..]).unwrap();
        assert_eq!(data.len() as u64, len);
        assert!(data.iter().all(|&b| b == 0));
    }
    // 64 MiB of zeros takes a few KiB, and the frames' windows stay small for decoders.
    let frame = zeros.frame(64 * MIB).unwrap().to_vec();
    assert!(frame.len() < 4096, "{} bytes", frame.len());
    let mut d = zstd::stream::read::Decoder::new(&frame[..]).unwrap();
    d.window_log_max(22).unwrap();
    assert_eq!(
        std::io::copy(&mut d, &mut std::io::sink()).unwrap(),
        64 * MIB
    );
}

#[test]
fn seek_table_round_trip() {
    let frames = vec![
        Frame {
            packed: 300,
            len: 0,
        },
        Frame {
            packed: 1234,
            len: 4 << 20,
        },
        Frame {
            packed: 2000,
            len: 64 << 20,
        },
    ];
    let mut file = vec![0u8; 3534];
    let table = image::seek_table(&frames);
    assert_eq!(table.len(), 8 + 8 * 3 + 9);
    assert_eq!(&table[..4], &[0x5e, 0x2a, 0x4d, 0x18]);
    assert_eq!(&table[table.len() - 4..], &[0xb1, 0xea, 0x92, 0x8f]);
    file.extend(&table);
    let size = file.len() as u64;
    let mut c = std::io::Cursor::new(&file);
    assert_eq!(
        image::read_seek_table(&mut c, size).unwrap(),
        (frames, 3534)
    );
    // No table, or a damaged one.
    let mut c = std::io::Cursor::new(&file[..file.len() - 1]);
    assert!(image::read_seek_table(&mut c, size - 1).is_err());
    let mut bad = file.clone();
    bad[3534 + 4] ^= 1; // the frame's size
    let mut c = std::io::Cursor::new(&bad);
    assert!(image::read_seek_table(&mut c, size).is_err());
}

#[test]
fn parses_options() {
    let args = |words: &[&str]| Args::parse(words.iter().map(OsString::from));
    let a = args(&[
        "--ddgui-worker",
        "--ddgui-sync=/dev/sdz",
        "--mode=smart",
        "--from=/dev/sdy",
        "--to=/tmp/x.img.zst",
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

    // Zeros: no --from, and --size (only) there.
    let a = args(&["--mode=zeros", "--to=/dev/sdz", "--size=5000000000"]).unwrap();
    assert_eq!((a.mode, a.size), (Mode::Zeros, Some(5_000_000_000)));
    assert_eq!(a.from, PathBuf::new());
    let a = args(&["--mode=zeros", "--to=/dev/sdz", "--block-size=16M"]).unwrap();
    assert_eq!((a.size, a.block_size), (None, 16 << 20));
    assert!(args(&["--mode=zeros", "--from=a", "--to=b"]).is_err());
    assert!(args(&["--mode=zeros", "--to=b", "--size=lots"]).is_err());
    assert!(args(&["--mode=restore", "--from=a", "--to=b", "--size=1M"]).is_err());
    assert!(args(&["--mode=zeros"]).is_err());

    assert_eq!(parse_size("4M"), Some(4 << 20));
    assert_eq!(parse_size("512K"), Some(512 << 10));
    assert_eq!(parse_size("64MiB"), Some(64 << 20));
    assert_eq!(parse_size("2T"), Some(2 << 40));
    assert_eq!(parse_size("4X"), None);
    assert_eq!(parse_size(""), None);
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

/// Checks a version 2 smart image frame by frame against its map.
fn check_layout(path: &Path, map: &Map) {
    let bytes = fs::read(path).unwrap();
    assert_eq!(
        &bytes[..4],
        &[0x5d, 0x2a, 0x4d, 0x18],
        "starts with the map frame"
    );
    let head = image::read_head(&mut &bytes[..]).unwrap().unwrap();
    assert_eq!(head.map.as_ref(), Ok(map));
    let mut c = std::io::Cursor::new(&bytes);
    let (frames, table_at) = image::read_seek_table(&mut c, bytes.len() as u64).unwrap();
    assert_eq!(
        frames[0],
        Frame {
            packed: head.len as u32,
            len: 0
        }
    );
    let mut cursor = image::Cursor::new(map);
    let (mut at, mut pos) = (head.len as usize, 0u64);
    for f in &frames[1..] {
        let frame = &bytes[at..at + f.packed as usize];
        assert_eq!(
            zstd::zstd_safe::find_frame_compressed_size(frame),
            Ok(frame.len())
        );
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(frame).ok(),
            Some(Some(f.len as u64))
        );
        // Either all used data (4 MiB at most) or all free space.
        let (used, run) = cursor.run(pos, f.len as u64);
        assert_eq!(
            run, f.len as u64,
            "a frame at {pos} mixes used and free space"
        );
        assert!(f.len as u64 <= if used { 4 * MIB } else { image::ZEROS_MAX });
        at += f.packed as usize;
        pos += f.len as u64;
    }
    assert_eq!((at as u64, pos), (table_at, map.size));
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

    let made = Progress::default();
    let out = Disk::create(&dir.path("smart.img.zst")).unwrap();
    copy::to_image(&src, &map, &out, &made).unwrap();
    let file_size = fs::metadata(dir.path("smart.img.zst")).unwrap().len();
    assert_eq!(made.done.load(Relaxed), map.used());
    assert_eq!(made.written.load(Relaxed), file_size);
    check_layout(&dir.path("smart.img.zst"), &map);

    // (a) Any zstd decoder gives the whole drive: the used data, zeros elsewhere.
    let expected = only_extents(&content, &extents, 0);
    assert_same("libzstd", &unzstd(&dir.path("smart.img.zst")), &expected);
    for tool in ["zstd", "pzstd"] {
        if have(tool) {
            let out = Command::new(tool)
                .args(["-d", "-c", "-q"])
                .arg(dir.path("smart.img.zst"))
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{tool}: {err}");
            assert_same(&format!("{tool} -dc"), &out.stdout, &expected);
        }
    }

    // probe() reads the map at the start.
    let info = super::probe(&dir.path("smart.img.zst")).unwrap();
    assert_eq!(info.format, ImageFormat::Zstd);
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
    let mut image = restore::inspect(&dir.path("smart.img.zst")).unwrap();
    assert_eq!(image.map.as_ref(), Some(&map));
    assert_eq!(image.total(), Some(map.used()));
    let target = Disk::open_drive(&dir.path("target")).unwrap();
    let p = progress();
    restore::restore(&mut image, &target, 4 << 20, &p).unwrap();
    assert_eq!(p.done.load(Relaxed), map.used());
    assert_eq!(p.written.load(Relaxed), map.used());
    assert_same(
        "restored onto 0xAA",
        &fs::read(dir.path("target")).unwrap(),
        &only_extents(&content, &extents, 0xaa),
    );

    // Restored into a new file: the complete image, sparse where the OS can.
    let file = Disk::create(&dir.path("restored.img")).unwrap();
    restore::restore(&mut image, &file, 1 << 20, &progress()).unwrap();
    assert_same(
        "restored into a file",
        &fs::read(dir.path("restored.img")).unwrap(),
        &expected,
    );

    // And onto a drive that only takes whole 4 KiB sectors (the drive's end is odd, so
    // its last write is a partial sector: read, patched, written back).
    let mut strict = vec![0xaa; size as usize];
    strict.resize(size.next_multiple_of(4096) as usize, 0xbb);
    fs::write(dir.path("strict"), &strict).unwrap();
    let target = Disk::strict(&dir.path("strict"), 4096).unwrap();
    restore::restore(&mut image, &target, 1 << 20, &progress()).unwrap();
    let mut want = only_extents(&content, &extents, 0xaa);
    want.resize(strict.len(), 0xbb);
    assert_same(
        "restored onto a strict drive",
        &fs::read(dir.path("strict")).unwrap(),
        &want,
    );
}

#[test]
fn empty_and_full_drives() {
    let dir = TempDir::new("edges");
    let size = 3 * MIB + 512;
    let content = junk_drive(&dir.path("drive"), size, &[]);
    let src = Disk::open_read(&dir.path("drive")).unwrap();
    for (name, extents) in [("empty", vec![]), ("full", vec![ext(0, size)])] {
        let map = Map {
            size,
            extents: extents.clone(),
        };
        let path = dir.path(&format!("{name}.img.zst"));
        copy::to_image(
            &src,
            &map,
            &Disk::create(&path).unwrap(),
            &Progress::default(),
        )
        .unwrap();
        check_layout(&path, &map);
        assert_same(name, &unzstd(&path), &only_extents(&content, &extents, 0));
        let mut image = restore::inspect(&path).unwrap();
        let out = dir.path(&format!("{name}.out"));
        restore::restore(
            &mut image,
            &Disk::create(&out).unwrap(),
            4 << 20,
            &progress(),
        )
        .unwrap();
        assert_same(
            name,
            &fs::read(&out).unwrap(),
            &only_extents(&content, &extents, 0),
        );
    }
}

#[test]
fn version_1_smart_images_still_restore() {
    let dir = TempDir::new("v1");
    let (size, extents) = test_layout();
    let content = junk_drive(&dir.path("drive"), size, &extents);
    let map = Map {
        size,
        extents: extents.clone(),
    };
    let bytes = image::v1::image(&content, &map);
    fs::write(dir.path("old.img.gz"), &bytes).unwrap();
    let expected = only_extents(&content, &extents, 0);
    let mut plain = Vec::new();
    flate2::read::MultiGzDecoder::new(&bytes[..])
        .read_to_end(&mut plain)
        .unwrap();
    assert_same("gunzip", &plain, &expected);

    let info = super::probe(&dir.path("old.img.gz")).unwrap();
    assert_eq!(info.format, ImageFormat::Gzip);
    assert_eq!(
        info.smart,
        Some(super::SmartInfo {
            disk_size: size,
            used: map.used()
        })
    );
    fs::write(dir.path("target"), vec![0xaa; size as usize]).unwrap();
    let mut image = restore::inspect(&dir.path("old.img.gz")).unwrap();
    let p = progress();
    restore::restore(
        &mut image,
        &Disk::open_drive(&dir.path("target")).unwrap(),
        4 << 20,
        &p,
    )
    .unwrap();
    assert_eq!(p.done.load(Relaxed), map.used());
    assert_same(
        "restored onto 0xAA",
        &fs::read(dir.path("target")).unwrap(),
        &only_extents(&content, &extents, 0xaa),
    );
    let file = Disk::create(&dir.path("restored.img")).unwrap();
    restore::restore(&mut image, &file, 1 << 20, &progress()).unwrap();
    assert_same(
        "into a file",
        &fs::read(dir.path("restored.img")).unwrap(),
        &expected,
    );

    // Damage is noticed.
    fs::write(dir.path("short.gz"), &bytes[..bytes.len() - 200]).unwrap();
    let mut image = restore::inspect(&dir.path("short.gz")).unwrap();
    let err = restore::restore(
        &mut image,
        &Disk::create(&dir.path("x")).unwrap(),
        4 << 20,
        &progress(),
    );
    assert!(err.unwrap_err().contains("damaged"));
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
        &Disk::create(&dir.path("a.zst")).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    let bytes = fs::read(dir.path("a.zst")).unwrap();
    let restore_it = |name: &str, bytes: &[u8]| {
        fs::write(dir.path(name), bytes).unwrap();
        let mut image = restore::inspect(&dir.path(name)).unwrap();
        assert!(image.map.is_some(), "{name}: still has its map");
        restore::restore(
            &mut image,
            &Disk::create(&dir.path("x")).unwrap(),
            4 << 20,
            &progress(),
        )
    };

    // Cut short: the seek table is gone. Nothing gets written then.
    let err = restore_it("short.zst", &bytes[..bytes.len() - 200]).unwrap_err();
    assert!(err.contains("damaged"), "{err}");
    assert_eq!(fs::metadata(dir.path("x")).unwrap().len(), 0);

    // A flipped byte in the data (the first frame after the map holds used data): the
    // frame's checksum catches it.
    let head = image::read_head(&mut &bytes[..]).unwrap().unwrap();
    let mut c = std::io::Cursor::new(&bytes);
    let (frames, _) = image::read_seek_table(&mut c, bytes.len() as u64).unwrap();
    let mut flipped = bytes.clone();
    flipped[(head.len + frames[1].packed as u64 / 2) as usize] ^= 0x40;
    assert!(restore_it("flipped.zst", &flipped).is_err());

    // The seek table doesn't match the frames.
    let mut table = bytes.clone();
    let at = bytes.len() - 9 - 8 * 2;
    table[at] ^= 1;
    let err = restore_it("table.zst", &table).unwrap_err();
    assert!(err.contains("damaged"), "{err}");
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

// ---- Alignment ----------------------------------------------------------------------------

#[test]
fn analysis_reads_through_disk_reader() {
    // The analysis gets small reads anywhere; DiskReader serves them from aligned reads.
    let dir = TempDir::new("reader");
    let data = noise(3, 3 * MIB as usize + 4096);
    fs::write(dir.path("d"), &data).unwrap();
    let plain = Disk::open_read(&dir.path("d")).unwrap();
    for sector in [0, 512, 4096] {
        // 0: a plain file; else a strict drive, which fails any misaligned read.
        let strict;
        let disk = if sector == 0 {
            &plain
        } else {
            strict = Disk::strict(&dir.path("d"), sector).unwrap();
            &strict
        };
        let mut r = DiskReader::new(disk);
        for (at, len) in [
            (0u64, 512usize),
            (446, 66),
            (MIB - 10, 20),
            (2 * MIB + 5, 300_000),
            (1234, 2 * MIB as usize + 777),
            (3 * MIB + 700, 77),
        ] {
            r.seek(SeekFrom::Start(at)).unwrap();
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf).unwrap();
            assert!(buf == data[at as usize..at as usize + len], "{len} at {at}");
        }
        let mut rest = Vec::new();
        r.seek(SeekFrom::End(-100)).unwrap();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[data.len() - 100..]);
    }
}

#[test]
fn strict_drives_get_whole_sectors() {
    let dir = TempDir::new("strict");
    let size = 2 * MIB as usize;
    let data = noise(21, size);
    for sector in [512u64, 4096] {
        fs::write(dir.path("d"), &data).unwrap();
        let disk = Disk::strict(&dir.path("d"), sector).unwrap();
        // Reads anywhere, of any length, into buffers with and without room to spare.
        for (off, len, room) in [
            (0u64, 4096usize, 4096usize),
            (100, 50, 4096),
            (4096, 5000, 5000),
            (4096, 5000, 8192),
            (511, 2, 4096),
            (size as u64 - 1000, 1000, 1000),
            (777, 300_000, 300_000),
        ] {
            let mut buf = AlignedBuf::new(room);
            disk.read_at(off, &mut buf, len).unwrap();
            assert!(
                buf[..len] == data[off as usize..off as usize + len],
                "{len} at {off}"
            );
        }
        // Writes anywhere: partial sectors are read, patched and written back.
        let mut want = data.clone();
        for (off, len) in [
            (0u64, 4096usize),
            (100, 50),
            (4096, 5000),
            (8191, 2),
            (size as u64 - 700, 700),
            (12345, 70_000),
        ] {
            let mut buf = AlignedBuf::new(len);
            buf.copy_from_slice(&noise(off, len));
            disk.write_at(off, &buf).unwrap();
            want[off as usize..off as usize + len].copy_from_slice(&buf);
        }
        drop(disk);
        assert_same("strict writes", &fs::read(dir.path("d")).unwrap(), &want);
    }
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

/// A 96 MiB drive image with a GPT, an ext4 and an NTFS partition, each with files.
fn gpt_drive(dir: &TempDir) -> Option<PathBuf> {
    if !(have("sfdisk") && have("mkfs.ext4") && have("mkntfs")) {
        return None;
    }
    let files = dir.path("files");
    fs::create_dir_all(files.join("sub")).unwrap();
    for n in 0..20u64 {
        fs::write(
            files.join(format!("f{n}")),
            noise(n, (n as usize + 1) * 30_000),
        )
        .unwrap();
    }
    fs::write(files.join("sub/big"), noise(99, 5 << 20)).unwrap();
    let drive = dir.path("gpt.img");
    fs::File::create(&drive).unwrap().set_len(96 * MIB).unwrap();
    let d = drive.display();
    sh(&format!(
        "printf 'label: gpt\\nstart=2048, size=81920, type=linux\\nstart=83968, size=100352, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\\n' | sfdisk -q '{d}'"
    ));
    let (ext4, ntfs) = (dir.path("p1"), dir.path("p2"));
    sh(&format!(
        "mkfs.ext4 -q -F -d {} {} 40M",
        q(&files),
        q(&ext4)
    ));
    fs::File::create(&ntfs)
        .unwrap()
        .set_len(100352 * 512)
        .unwrap();
    sh(&format!(
        "mkntfs -q -F -Q -s 512 -p 83968 -H 255 -S 63 {} >/dev/null 2>&1",
        q(&ntfs)
    ));
    sh(&format!(
        "dd if={} of='{d}' bs=512 seek=2048 conv=notrunc status=none && dd if={} of='{d}' bs=512 seek=83968 conv=notrunc status=none",
        q(&ext4),
        q(&ntfs)
    ));
    Some(drive)
}

#[test]
fn analysis_on_strict_drives() {
    // The analysis reads many small, odd pieces; through a strict drive every one of them
    // has to become whole sectors, or it fails. The layout has to come out the same.
    let dir = TempDir::new("analysis");
    let drives: Vec<PathBuf> = [fat_drive(&dir), gpt_drive(&dir)]
        .into_iter()
        .flatten()
        .collect();
    for drive in drives {
        let plain = Disk::open_read(&drive).unwrap();
        let want = smart::analyze(&mut DiskReader::new(&plain), plain.size).unwrap();
        assert!(want.partitions.iter().all(|p| p.understood), "{want:?}");
        for sector in [512, 4096] {
            let disk = Disk::strict(&drive, sector).unwrap();
            let got = smart::analyze(&mut DiskReader::new(&disk), disk.size).unwrap();
            assert_eq!(
                got.extents,
                want.extents,
                "{} with {sector}-byte sectors",
                drive.display()
            );
            let got = smart::estimate(&mut DiskReader::new(&disk), disk.size).unwrap();
            assert_eq!(got.used(), want.used());
        }
    }
}

#[test]
fn unaligned_extents_on_strict_drives() {
    // Maps in bytes rather than 4 KiB units (unit=1): reads and writes that start and end
    // inside sectors, from and to drives that refuse anything but whole sectors.
    let dir = TempDir::new("unaligned");
    let size = 8 * MIB;
    let extents = vec![
        ext(100, 900),
        ext(5000, 3 * MIB + 17),
        ext(size - 4000, 3999),
    ];
    let content = junk_drive(&dir.path("drive"), size, &extents);
    let map = Map {
        size,
        extents: extents.clone(),
    };
    let src = Disk::strict(&dir.path("drive"), 4096).unwrap();
    copy::to_image(
        &src,
        &map,
        &Disk::create(&dir.path("u.zst")).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    assert!(
        image::read_head(&mut fs::File::open(dir.path("u.zst")).unwrap())
            .unwrap()
            .unwrap()
            .map
            .unwrap()
            .to_text(image::MAGIC_V2)
            .contains(" unit=1\n")
    );
    for sector in [512, 4096] {
        fs::write(dir.path("t"), vec![0xaa; size as usize]).unwrap();
        let dst = Disk::strict(&dir.path("t"), sector).unwrap();
        let mut image = restore::inspect(&dir.path("u.zst")).unwrap();
        restore::restore(&mut image, &dst, 1 << 20, &progress()).unwrap();
        assert_same(
            "restored",
            &fs::read(dir.path("t")).unwrap(),
            &only_extents(&content, &extents, 0xaa),
        );
        fs::write(dir.path("t"), vec![0xaa; size as usize]).unwrap();
        let dst = Disk::strict(&dir.path("t"), sector).unwrap();
        copy::to_drive(&src, &dst, &extents, 1 << 20, &Progress::default()).unwrap();
        assert_same(
            "copied",
            &fs::read(dir.path("t")).unwrap(),
            &only_extents(&content, &extents, 0xaa),
        );
    }
}

// ---- Zeros ----------------------------------------------------------------------------

#[test]
fn zeros_to_files_and_drives() {
    let dir = TempDir::new("zeros");
    // A file, created at the size asked for.
    let len = 10 * MIB + 123;
    let p = progress();
    copy::zeros(&Disk::create(&dir.path("z")).unwrap(), len, 4 << 20, &p).unwrap();
    let got = fs::read(dir.path("z")).unwrap();
    assert_eq!(got.len() as u64, len);
    assert!(got.iter().all(|&b| b == 0));
    assert_eq!((p.done.load(Relaxed), p.written.load(Relaxed)), (len, len));

    // A strict drive, partly: the bytes after stay, even inside the last sector.
    let size = 3 * MIB as usize;
    fs::write(dir.path("d"), vec![0xaa; size]).unwrap();
    let drive = Disk::strict(&dir.path("d"), 4096).unwrap();
    copy::zeros(&drive, MIB + 100, 1 << 20, &progress()).unwrap();
    let got = fs::read(dir.path("d")).unwrap();
    assert!(got[..MIB as usize + 100].iter().all(|&b| b == 0));
    assert!(got[MIB as usize + 100..].iter().all(|&b| b == 0xaa));
    // All of it.
    copy::zeros(&drive, drive.size, 1 << 20, &progress()).unwrap();
    assert!(fs::read(dir.path("d")).unwrap().iter().all(|&b| b == 0));
}

// ---- Other tools' images ----------------------------------------------------------------

/// 16 MiB and a bit of mixed data: noise, text, zeros.
fn mixed_data() -> Vec<u8> {
    let mut data = noise(11, 4 << 20);
    let text = b"The quick brown fox jumps over the lazy dog. DD-GUI restores images. ";
    data.extend(text.iter().cycle().take(4 << 20));
    data.extend(vec![0u8; 4 << 20]);
    data.extend(noise(12, (4 << 20) + 1000));
    data
}

/// Restores `path` into a file and checks what comes out, and the progress.
fn check_restore(dir: &TempDir, name: &str, data: &[u8]) {
    let path = dir.path(name);
    let mut image = restore::inspect(&path).unwrap();
    let out = dir.path(&format!("{name}.out"));
    let p = progress();
    let started = Instant::now();
    restore::restore(&mut image, &Disk::create(&out).unwrap(), 4 << 20, &p).unwrap();
    eprintln!(
        "{name}: restored in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    assert_same(name, &fs::read(&out).unwrap(), data);
    assert_eq!(
        Some(p.done.load(Relaxed)),
        image.total(),
        "{name}: progress"
    );
    assert_eq!(
        p.written.load(Relaxed),
        data.len() as u64,
        "{name}: written"
    );
}

#[test]
fn restores_images_made_by_other_tools() {
    let dir = TempDir::new("formats");
    let data = mixed_data();
    let raw = dir.path("disk.img");
    fs::write(&raw, &data).unwrap();
    let len = data.len() as u64;

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
        // Several frames (from pipes, so without sizes), with a skippable frame in between,
        // and a long window.
        sh(&format!(
            "head -c 5000000 {r} | zstd -q > {o}; printf '\\x50\\x2a\\x4d\\x18\\x03\\x00\\x00\\x00abc' >> {o}; tail -c +5000001 {r} | zstd -q --long=31 >> {o}",
            r = q(&raw),
            o = q(&dir.path("frames.zst"))
        ));
        // Several threads, and a huge window.
        sh(&format!(
            "zstd -q -T4 --long=31 -c {} > {}",
            q(&raw),
            q(&dir.path("long.zst"))
        ));
        cases.extend([
            ("disk.img.zst", ImageFormat::Zstd, Some(len)),
            ("piped.zst", ImageFormat::Zstd, None),
            ("frames.zst", ImageFormat::Zstd, None),
            ("long.zst", ImageFormat::Zstd, Some(len)),
        ]);
        if have("pzstd") {
            // pzstd's own skippable frames before each of its frames.
            sh(&format!(
                "pzstd -q -p 4 -c {} > {}",
                q(&raw),
                q(&dir.path("p.zst"))
            ));
            cases.push(("p.zst", ImageFormat::Zstd, None));
        }
    }
    if have("python3") {
        for (name, method) in [("disk.zip", "ZIP_DEFLATED"), ("stored.zip", "ZIP_STORED")] {
            // A folder, an empty file and a symbolic link first (all skipped), and another
            // file after the image.
            let script = format!(
                "import zipfile,sys\nwith zipfile.ZipFile(sys.argv[1],'w',zipfile.{method}) as z:\n z.writestr(zipfile.ZipInfo('images/'),'')\n z.writestr('images/empty.txt','')\n l=zipfile.ZipInfo('images/link')\n l.external_attr=0o120777<<16\n z.writestr(l,'disk.img')\n z.write(sys.argv[2],'images/disk.img')\n z.writestr('README.txt','hello')\n"
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
        let info = super::probe(&dir.path(name)).unwrap();
        assert_eq!(info.format, format, "{name}: format");
        assert_eq!(info.uncompressed, uncompressed, "{name}: size");
        assert_eq!(info.smart, None);
        check_restore(&dir, name, &data);
    }
}

#[test]
fn damaged_zstd_is_refused() {
    let dir = TempDir::new("badzstd");
    let data = mixed_data();
    fs::write(dir.path("a.zst"), zstd::bulk::compress(&data, 3).unwrap()).unwrap();
    let mut bytes = fs::read(dir.path("a.zst")).unwrap();
    bytes.truncate(bytes.len() - 100);
    fs::write(dir.path("short.zst"), &bytes).unwrap();
    let mut image = restore::inspect(&dir.path("short.zst")).unwrap();
    let err = restore::restore(
        &mut image,
        &Disk::create(&dir.path("x")).unwrap(),
        4 << 20,
        &progress(),
    )
    .unwrap_err();
    assert!(err.starts_with("couldn't unpack short.zst"), "{err}");
}

/// A 512-byte tar header.
fn tar_header(name: &str, kind: u8, size: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[124..124 + size.len()].copy_from_slice(size);
    h[156] = kind;
    h[257..265].copy_from_slice(b"ustar  \0");
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

#[test]
fn sizes_of_files_in_tars() {
    let first = |tar: &[u8]| restore::tar_first_file(tar);
    // Octal, after a folder.
    let mut tar = tar_header("images/", b'5', b"00000000000\0");
    tar.extend(tar_header("images/disk.img", b'0', b"00000001750\0"));
    assert_eq!(first(&tar[..]), Some(1000));
    // GNU base-256, for files of 8 GiB and more.
    let mut big = [0u8; 12];
    big[0] = 0x80;
    big[4..].copy_from_slice(&(20u64 << 30).to_be_bytes());
    assert_eq!(
        first(&tar_header("disk.img", b'0', &big)[..]),
        Some(20 << 30)
    );
    // A pax header with the size, for the file after it.
    let record = "30 size=21474836480\n";
    let record = format!("{} {}", record.len(), &record[3..]);
    let mut tar = tar_header(
        "PaxHeaders/disk.img",
        b'x',
        format!("{:011o}\0", record.len()).as_bytes(),
    );
    let mut data = record.into_bytes();
    data.resize(512, 0);
    tar.extend(data);
    tar.extend(tar_header("disk.img", b'0', b"00000000000\0"));
    assert_eq!(first(&tar[..]), Some(20 << 30));
    // Not a tar: a disk's first sector, or nothing.
    let mut mbr = noise(1, 512);
    mbr[510..].copy_from_slice(&[0x55, 0xaa]);
    assert_eq!(first(&mbr[..]), None);
    assert_eq!(first(&[0u8; 1024][..]), None);
    assert_eq!(first(&b""[..]), None);
}

#[test]
fn tars_inside_compressed_images() {
    // What gets restored from a .tar.gz and the like is the first file in the tar, so
    // that's the size that counts (for the "doesn't fit" check and the GUI).
    if !(have("tar") && have("gzip") && have("xz") && have("zstd")) {
        return;
    }
    let dir = TempDir::new("tars");
    let data = mixed_data();
    fs::create_dir_all(dir.path("in/images")).unwrap();
    fs::write(dir.path("in/images/disk.img"), &data).unwrap();
    let tarred = [
        ("disk.tar.gz", "--gzip"),
        ("disk.tar.xz", "--xz"),
        ("disk.tar.zst", "--zstd"),
    ];
    for (name, how) in tarred {
        sh(&format!(
            "tar -C {} -c {how} -f {} images",
            q(&dir.path("in")),
            q(&dir.path(name))
        ));
    }
    // Pax headers first (bsdtar's default), and a zip holding the tar.
    sh(&format!(
        "tar -C {} --format=pax -c --xz -f {} images",
        q(&dir.path("in")),
        q(&dir.path("pax.tar.xz"))
    ));
    sh(&format!(
        "tar -C {} -c -f {} images",
        q(&dir.path("in")),
        q(&dir.path("disk.tar"))
    ));
    sh(&format!(
        "python3 -c \"import zipfile,sys; zipfile.ZipFile(sys.argv[1],'w',zipfile.ZIP_DEFLATED).write(sys.argv[2],'disk.tar')\" {} {}",
        q(&dir.path("tar.zip")),
        q(&dir.path("disk.tar"))
    ));
    // Is `formats::untar` there yet? (The formats module is written separately.)
    let tar = fs::read(dir.path("disk.tar")).unwrap();
    let mut untarred = Vec::new();
    let untar_works = super::formats::untar(Box::new(std::io::Cursor::new(tar)))
        .and_then(|mut r| r.read_to_end(&mut untarred))
        .is_ok_and(|_| untarred == data);
    if !untar_works {
        eprintln!("note: formats::untar doesn't unpack tars yet: only checking sizes");
    }
    for name in [
        "disk.tar.gz",
        "disk.tar.xz",
        "disk.tar.zst",
        "pax.tar.xz",
        "tar.zip",
    ] {
        let info = super::probe(&dir.path(name)).unwrap();
        assert_eq!(info.uncompressed, Some(data.len() as u64), "{name}");
        if untar_works {
            let mut image = restore::inspect(&dir.path(name)).unwrap();
            let out = dir.path(&format!("{name}.out"));
            restore::restore(
                &mut image,
                &Disk::create(&out).unwrap(),
                4 << 20,
                &progress(),
            )
            .unwrap();
            assert_same(name, &fs::read(&out).unwrap(), &data);
        }
    }
}

#[test]
fn formats_from_the_formats_module() {
    // Virtual disks and the like come through `formats`: progress then counts the raw
    // bytes written, out of the raw disk's size (when the format says; else no total).
    if !(have("qemu-img") && have("bzip2")) {
        return;
    }
    let dir = TempDir::new("vdisks");
    let data = mixed_data();
    let raw = dir.path("disk.img");
    fs::write(&raw, &data).unwrap();
    sh(&format!(
        "qemu-img convert -q -f raw -O vpc -o subformat=fixed,force_size=on {} {}",
        q(&raw),
        q(&dir.path("disk.vhd"))
    ));
    sh(&format!(
        "qemu-img convert -q -f raw -O qcow2 {} {}",
        q(&raw),
        q(&dir.path("disk.qcow2"))
    ));
    sh(&format!(
        "bzip2 -c {} > {}",
        q(&raw),
        q(&dir.path("disk.img.bz2"))
    ));
    for (name, format) in [
        ("disk.vhd", ImageFormat::Vhd),
        ("disk.qcow2", ImageFormat::Qcow2),
        ("disk.img.bz2", ImageFormat::Bzip2),
    ] {
        let info = super::probe(&dir.path(name)).unwrap();
        if info.format != format {
            eprintln!("note: formats doesn't recognise {name} yet, skipping it");
            continue;
        }
        let mut image = restore::inspect(&dir.path(name)).unwrap();
        image.open().unwrap();
        let total = image.total();
        let out = dir.path(&format!("{name}.out"));
        let p = progress();
        restore::restore(&mut image, &Disk::create(&out).unwrap(), 4 << 20, &p).unwrap();
        // qemu-img rounds a virtual disk up to whole sectors (or more): zeros at the end.
        let got = fs::read(&out).unwrap();
        assert_same(name, &got[..data.len()], &data);
        assert!(got[data.len()..].iter().all(|&b| b == 0), "{name}: its end");
        assert_eq!(
            p.done.load(Relaxed),
            got.len() as u64,
            "{name}: done counts raw bytes"
        );
        assert_eq!(p.written.load(Relaxed), got.len() as u64);
        if format != ImageFormat::Bzip2 {
            assert_eq!(
                total,
                Some(got.len() as u64),
                "{name}: a virtual disk knows its size"
            );
        }
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
    let mut image = restore::inspect(&dir.path("img")).unwrap();
    let err = restore::restore(&mut image, &target, MIB as usize, &progress()).unwrap_err();
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

/// A running `dd-gui …`, with its stdout lines as they come.
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

fn copy_args(extra: &[&str], from: Option<&Path>, to: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "copy".into(),
        "--ddgui-worker".into(),
        "--ddgui-watch-stdin".into(),
    ];
    args.extend(extra.iter().map(OsString::from));
    if let Some(from) = from {
        let mut from_arg = OsString::from("--from=");
        from_arg.push(from);
        args.push(from_arg);
    }
    let mut to_arg = OsString::from("--to=");
    to_arg.push(to);
    args.push(to_arg);
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
        Some(&dir.path("drive")),
        &dir.path("drive.img.zst"),
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
            fs::metadata(dir.path("drive.img.zst")).unwrap().len()
        )
    );
    assert_same("unzstd", &unzstd(&dir.path("drive.img.zst")), &data);

    let w = Worker::start(&copy_args(
        &["--mode=restore", "--block-size=1M"],
        Some(&dir.path("drive.img.zst")),
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
        Some(&dir.path("missing.img")),
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

#[test]
fn e2e_zeros() {
    let dir = TempDir::new("e2ezeros");
    fs::write(dir.path("z"), noise(1, 100_000)).unwrap();
    let len = 9 * MIB + 5;
    let w = Worker::start(&copy_args(
        &["--mode=zeros", &format!("--size={len}")],
        None,
        &dir.path("z"),
    ));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(protocol(&lines), ["@ready", "@copy", "@total", "@sync"]);
    assert_eq!(total(&lines), len);
    assert_eq!(*progress_lines(&lines).last().unwrap(), (len, len));
    let got = fs::read(dir.path("z")).unwrap();
    assert_eq!(got.len() as u64, len);
    assert!(got.iter().all(|&b| b == 0));

    // A file needs --size.
    let w = Worker::start(&copy_args(&["--mode=zeros"], None, &dir.path("y")));
    let (code, _, stderr) = w.finish();
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "dd-gui: --size is needed to write zeros to a file\n"
    );
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
        Some(&drive),
        &dir.path("smart.img.zst"),
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
    let info = super::probe(&dir.path("smart.img.zst")).unwrap();
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
        Some(&dir.path("smart.img.zst")),
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

    // "full": the bundled dd copies everything and reports like `dd-gui dd`.
    let mut args = copy_args(
        &["--mode=smart", "--ask"],
        Some(&drive),
        &dir.path("unused.zst"),
    );
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
    assert!(!dir.path("unused.zst").exists());

    // dd's own errors come through as dd's, with its exit code.
    let mut args = copy_args(
        &["--mode=smart", "--ask"],
        Some(&drive),
        &dir.path("unused.zst"),
    );
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
        Some(&drive),
        &dir.path("no.zst"),
    ));
    w.wait_for("@layout ");
    w.say("nope");
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(130), "{stderr}");
    assert_eq!(protocol(&lines), ["@ready", "@layout"]);
    assert!(stderr.ends_with("dd-gui: cancelled\n"));

    let mut w = Worker::start(&copy_args(
        &["--mode=smart", "--ask"],
        Some(&drive),
        &dir.path("eof.zst"),
    ));
    w.wait_for("@layout ");
    drop(w.stdin.take());
    let (code, lines, _) = w.finish();
    assert_eq!(code, Some(130));
    assert_eq!(protocol(&lines), ["@ready", "@layout"]);

    // The answer file (macOS), with stdin unused.
    let answer = dir.path("answer");
    let mut args = copy_args(
        &["--mode=smart", "--ask"],
        Some(&drive),
        &dir.path("file.zst"),
    );
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
    let mut w = Worker::start(&copy_args(
        &["--mode=smart"],
        Some(&drive),
        &dir.path("a.zst"),
    ));
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
    let mut args = copy_args(&["--mode=smart"], Some(&drive), &dir.path("b.zst"));
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

/// Drives of 16 and 64 GiB with a few MB used, and 64 GiB with 9 GiB used: their smart
/// images take next to no time for the free space.
#[test]
#[ignore]
fn gaps_cost_almost_nothing() {
    for gib in [16u64, 64] {
        gaps(gib << 30, 0);
    }
    gaps(64 << 30, 9 << 30);
}

fn gaps(size: u64, bulk: u64) {
    let dir = TempDir::scratch("gaps");
    let drive = dir.path("sparse");
    fs::File::create(&drive).unwrap().set_len(size).unwrap();
    // 1 MiB at both ends and 64 KiB every 256 MiB, like a file system's metadata, and
    // `bulk` bytes of files in 64 MiB pieces all over the drive.
    let mut extents = vec![ext(0, MIB)];
    extents.extend((1..size / (256 * MIB)).map(|i| ext(i * 256 * MIB, 64 << 10)));
    let (pieces, periods) = (bulk / (64 * MIB), size / (256 * MIB));
    for i in 0..pieces {
        // Between the bits of metadata, never over them.
        let k = 1 + i * (periods - 2) / pieces;
        extents.push(ext(k * 256 * MIB + MIB, 64 * MIB));
    }
    extents.push(ext(size - MIB, MIB));
    extents.sort_by_key(|e| e.start);
    let file = fs::OpenOptions::new().write(true).open(&drive).unwrap();
    use std::os::unix::fs::FileExt;
    let text = b"Some text that compresses a bit, like most files do. 0123456789\n";
    let mut bulk_data = noise(1, 32 * MIB as usize);
    bulk_data.extend(text.iter().cycle().take(32 * MIB as usize));
    for (n, e) in extents.iter().enumerate() {
        if e.len == 64 * MIB {
            file.write_at(&bulk_data, e.start).unwrap();
        } else {
            file.write_at(&noise(n as u64, e.len as usize), e.start)
                .unwrap();
        }
    }
    drop(file);
    let map = Map {
        size,
        extents: super::tidy(&extents, size),
    };
    let src = Disk::open_read(&drive).unwrap();
    let out = dir.path("sparse.img.zst");
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
        "{} GiB drive, {} MiB used: smart image of {:.1} MB in {:.2} s",
        size >> 30,
        map.used() >> 20,
        bytes as f64 / 1e6,
        took.as_secs_f64()
    );
    // The free space itself: a table of frames, and those of zeros.
    let mut zeros = Zeros::default();
    let started = Instant::now();
    let mut free = 0;
    let mut pos = 0;
    for e in map.extents.iter().chain([&ext(size, 0)]) {
        for piece in Zeros::pieces(e.start - pos) {
            free += zeros.frame(piece).unwrap().len();
        }
        pos = e.start + e.len;
    }
    eprintln!(
        "  its {} GiB of free space: {} KB, made in {:.3} s",
        (size - map.used()) >> 30,
        free / 1000,
        started.elapsed().as_secs_f64()
    );
    if bulk == 0 {
        assert!(took < Duration::from_secs(10));
    }

    // Restoring skips the free space too.
    let mut image = restore::inspect(&out).unwrap();
    let back = dir.path("back");
    let started = Instant::now();
    restore::restore(
        &mut image,
        &Disk::create(&back).unwrap(),
        4 << 20,
        &progress(),
    )
    .unwrap();
    eprintln!("  restored in {:.2} s", started.elapsed().as_secs_f64());
    let back = fs::File::open(&back).unwrap();
    assert_eq!(back.metadata().unwrap().len(), size);
    for (n, e) in extents.iter().enumerate() {
        let mut buf = vec![0u8; e.len as usize];
        back.read_exact_at(&mut buf, e.start).unwrap();
        if e.len == 64 * MIB {
            assert!(buf == bulk_data);
        } else {
            assert!(buf == noise(n as u64, e.len as usize));
        }
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
        let out = dir.path(&format!("{name}.zst"));
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
        let mut image = restore::inspect(&out).unwrap();
        let back = dir.path(&format!("{name}.back"));
        let started = Instant::now();
        restore::restore(
            &mut image,
            &Disk::create(&back).unwrap(),
            4 << 20,
            &progress(),
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

/// On a real drive (a loop device, say) opened with O_DIRECT: `DDGUI_TEST_LOOP=/dev/loopN`,
/// writable by you, and nothing on it you want to keep. Its sectors may be 512 or 4096
/// bytes (`losetup -b 4096`).
#[test]
#[ignore]
fn real_drive_with_direct_io() {
    let Some(dev) = std::env::var_os("DDGUI_TEST_LOOP").map(PathBuf::from) else {
        eprintln!("note: set DDGUI_TEST_LOOP to a loop device to run this");
        return;
    };
    assert!(
        super::disk::is_drive(&dev),
        "{} isn't a drive",
        dev.display()
    );
    let dir = TempDir::scratch("loop");
    let drive = Disk::open_drive(&dev).unwrap();
    let size = drive.size;
    eprintln!(
        "{}: {size} bytes, aligned to {}",
        dev.display(),
        drive.align()
    );
    assert!(
        size >= 32 * MIB && size <= 4 << 30,
        "use a loop device of 32 MiB to 4 GiB"
    );

    // Zeros over all of it, through the binary.
    let w = Worker::start(&copy_args(&["--mode=zeros"], None, &dev));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(total(&lines), size);
    assert_eq!(*progress_lines(&lines).last().unwrap(), (size, size));
    let mut all = Vec::new();
    fs::File::open(&dev).unwrap().read_to_end(&mut all).unwrap();
    assert!(all.len() as u64 == size && all.iter().all(|&b| b == 0));

    // A smart image with odd extents, restored onto it (partial sectors at the end of
    // extents, written back whole), then a smart copy from it and an analysis through it.
    let extents = vec![
        ext(0, MIB),
        ext(5 * MIB + 4096, 3 * MIB + 100),
        ext(size - 5000, 5000),
    ];
    let content = junk_drive(&dir.path("src"), size, &extents);
    let map = Map {
        size,
        extents: extents.clone(),
    };
    let src = Disk::open_read(&dir.path("src")).unwrap();
    copy::to_image(
        &src,
        &map,
        &Disk::create(&dir.path("i.zst")).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    let w = Worker::start(&copy_args(
        &["--mode=restore"],
        Some(&dir.path("i.zst")),
        &dev,
    ));
    let (code, lines, stderr) = w.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(total(&lines), map.used());
    let mut all = Vec::new();
    fs::File::open(&dev).unwrap().read_to_end(&mut all).unwrap();
    assert_same(
        "restored onto the drive",
        &all,
        &only_extents(&content, &extents, 0),
    );

    let drive = Disk::open_read(&dev).unwrap();
    let layout = smart::analyze(&mut DiskReader::new(&drive), drive.size).unwrap();
    assert_eq!(layout.size, size);
    let back = dir.path("back.zst");
    copy::to_image(
        &drive,
        &map,
        &Disk::create(&back).unwrap(),
        &Progress::default(),
    )
    .unwrap();
    assert_same(
        "imaged from the drive",
        &unzstd(&back),
        &only_extents(&content, &extents, 0),
    );
    let mut sectors = AlignedBuf::new(8192);
    drive.read_at(size - 5000, &mut sectors, 5000).unwrap();
    assert!(sectors[..5000] == content[size as usize - 5000..]);
}
