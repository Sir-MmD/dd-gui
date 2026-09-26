//! btrfs, XFS, F2FS, LVM2, swap, ISO 9660 and UDF on images made with the system's tools. As
//! in tests.rs: the tools' numbers against ours, then smart copies (only the extents copied,
//! everything else 0xA5 garbage) must pass the tools' read-only checks and give back every
//! file byte for byte, and dropping the largest extent must be noticed.
//!
//! Without root, images are filled with mkfs.btrfs --rootdir, mkfs.xfs -p, sload.f2fs,
//! xorriso and hand-made LVM metadata, and read back with btrfs restore, xfs_db, dump.f2fs,
//! 7z and debugfs. Tests that mount (to delete files, fragment them, snapshot and reflink,
//! write a lot, fill UDF, or make real volume groups) need root and are ignored by default:
//! `cargo test smart -- --ignored linux_root` (with DD_GUI_TEST_SUDO_PASSWORD set when sudo
//! can't ask). They use loop devices of their own (never /dev/loop0), name volume groups
//! "ddguitest…", and unmount, deactivate and detach everything when done.

#![cfg(target_os = "linux")]

use super::tests::linux::{
    MB, Scratch, Tree, blank, can_sudo, compare, estimate_of, file_size, fs_usage, have, layout_of, make_tree,
    number_after, on_path, p, put, put_bytes, read_range, sh, smart_copy, sudo, supports, trees, try_sh, write_tree,
};
use super::tests::{Rng, analyze_bytes, check_invariants};
use super::*;
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Shared plumbing.

/// Runs a command in `dir` with `input` on its stdin: (success, stdout and stderr).
fn run_in(dir: &Path, cmd: &str, args: &[&str], input: &str) -> (bool, String) {
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{cmd}: {e}"));
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    let out = child.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

/// An empty directory in the scratch space.
fn fresh(s: &Scratch, name: &str) -> PathBuf {
    let dir = s.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Files that compress well (for btrfs compression): a short random pattern, repeated.
fn compressible(rng: &mut Rng, tag: &str, count: usize) -> Tree {
    (0..count)
        .map(|i| {
            let n = 1 + rng.below(200) as usize;
            let pattern = rng.bytes(n);
            let len = 10_000 + rng.below(600_000) as usize;
            (format!("{tag}{i}.txt"), pattern.iter().copied().cycle().take(len).collect())
        })
        .collect()
}

fn bytes_of(v: &[Extent]) -> u64 {
    v.iter().map(|e| e.len).sum()
}

/// A file system alone, as the tools want it: `start..start + size` of `img` (itself when
/// that's all of it).
fn cut_out(img: &Path, start: u64, size: u64, to: &Path) -> PathBuf {
    if start == 0 && size == file_size(img) {
        return img.to_owned();
    }
    fs::write(to, read_range(img, start, size)).unwrap();
    to.to_owned()
}

/// How a smart copy is checked: given the file system alone (a file), Err says what's wrong.
type Verify<'a> = &'a dyn Fn(&Path) -> Result<(), String>;

/// A smart copy of `img` (per `layout`) must pass `verify` on the file system at
/// `start..start + size`; without the largest extent inside it (or without `drop`, when
/// given), it must not.
fn check_copy(s: &Scratch, img: &Path, start: u64, size: u64, layout: &Layout, drop: Option<Extent>, verify: Verify) {
    let copy = s.join("copy.img");
    let part = s.join("copy-part.img");
    smart_copy(img, &layout.extents, &copy);
    verify(&cut_out(&copy, start, size, &part)).unwrap_or_else(|e| panic!("smart copy broken: {e}"));
    let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= start + size);
    if let Some(big) = drop.or_else(|| inside.max_by_key(|e| e.len).copied()) {
        // The same copy, without that extent.
        let mut left = big.len;
        while left > 0 {
            let n = left.min(16 * MB);
            put_bytes(&copy, big.start + big.len - left, &vec![0xA5; n as usize]);
            left -= n;
        }
        let broken = verify(&cut_out(&copy, start, size, &part));
        assert!(broken.is_err(), "dropping {big:?} went unnoticed");
    }
    let _ = fs::remove_file(&copy);
    let _ = fs::remove_file(&part);
}

/// Our exact usage of the file system at `start..start + size` (no merging, no alignment):
/// it must cover what the tool says is used, and be no more than that plus `slack`.
fn check_numbers(what: &str, img: &Path, start: u64, size: u64, tool_used: u64, slack: u64) -> Vec<Extent> {
    let ours = fs_usage(img, start, size).unwrap_or_else(|why| panic!("{what} not understood: {why}"));
    let n = bytes_of(&ours);
    eprintln!("{what}: ours {n} bytes, the tools say {tool_used} used, of {size}");
    assert!(n >= tool_used && n <= tool_used + slack, "{what}: ours {n}, the tools {tool_used} (+{slack} allowed)");
    ours
}

/// Whether the exact extents cover all of `start..start + len`.
fn covers(v: &[Extent], start: u64, len: u64) -> bool {
    util::overlap(v, start, len) == len
}

// Swap.

#[test]
fn swap_keeps_only_its_header() {
    if !have(&["mkswap", "swaplabel", "blkid"]) {
        return;
    }
    let s = Scratch::new("swap");
    let mut rng = Rng(0x5A9);
    let size = 32 * MB;
    for page in [4096u64, 65536] {
        let img = s.join(&format!("swap-{page}.img"));
        // Garbage first: what was swapped out is never needed.
        fs::write(&img, rng.bytes(size as usize)).unwrap();
        sh(
            "mkswap",
            &["-L", "smartswap", "-U", "0b1d4c43-6a5e-4b6e-9c0f-2f2d8a1c3e55", "-p", &page.to_string(), p(&img)],
        );
        let layout = layout_of(&img);
        let part = &layout.partitions[0];
        assert!(part.understood, "{part:?}");
        assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("swap"), Some("smartswap")));
        // The header page, within what's always kept at the start.
        let ours = check_numbers("swap", &img, 0, size, page, HEAD_KEPT - page.min(HEAD_KEPT));
        assert_eq!(bytes_of(&ours), page.max(HEAD_KEPT));
        let label = sh("swaplabel", &[p(&img)]);
        let verify = |copy: &Path| -> Result<(), String> {
            let (ok, text) = try_sh("swaplabel", &[p(copy)]);
            let (_, kind) = try_sh("blkid", &["-p", "-o", "value", "-s", "TYPE", p(copy)]);
            if ok && text == label && kind.trim() == "swap" { Ok(()) } else { Err(format!("{text} {kind}")) }
        };
        // The header is in the first extent (with the drive's first MiB).
        let first = layout.extents[0];
        check_copy(&s, &img, 0, size, &layout, Some(first), &verify);
    }
    // A hibernation image in it: copied in full.
    let img = s.join("swap-4096.img");
    let good = fs::read(&img).unwrap();
    for magic in [&b"S1SUSPEND\0"[..], b"S2SUSPEND\0", b"ULSUSPEND\0", b"LINHIB0001"] {
        let mut data = good.clone();
        data[4086..4096].copy_from_slice(magic);
        let layout = analyze_bytes(&data, size).unwrap();
        check_invariants(&layout);
        assert_eq!(layout.partitions[0].fs.as_deref(), Some("swap"));
        assert!(!layout.partitions[0].understood);
        assert_eq!(layout.used(), size);
    }
    // Written by a big-endian machine: the same, byte-swapped.
    let mut data = good.clone();
    for at in (1024..1036).step_by(4) {
        data[at..at + 4].reverse();
    }
    let layout = analyze_bytes(&data, size).unwrap();
    assert!(layout.partitions[0].understood);
    // Nonsense in the header: copied in full.
    for (at, value) in [(1024, 7u32), (1028, u32::MAX), (1032, 5000)] {
        let mut data = good.clone();
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        let layout = analyze_bytes(&data, size).unwrap();
        assert!(!layout.partitions[0].understood, "field at {at}");
    }
}

// ISO 9660.

#[test]
fn iso9660_keeps_the_volume_and_appended_partitions() {
    if !have(&["xorriso", "7z", "mkfs.fat", "sfdisk"]) {
        return;
    }
    let s = Scratch::new("iso");
    let mut rng = Rng(0x150);
    let (files, _) = trees(&mut rng, 60, 2 * MB);
    let src = fresh(&s, "src");
    write_tree(&files, &src);
    let efi = s.join("efi.img");
    blank(&efi, 4 * MB);
    sh("mkfs.fat", &[p(&efi)]);
    let iso = s.join("hybrid.iso");
    sh(
        "xorriso",
        &[
            "-as",
            "mkisofs",
            "-quiet",
            "-V",
            "SMARTISO",
            "-o",
            p(&iso),
            "-append_partition",
            "2",
            "0xef",
            p(&efi),
            p(&src),
        ],
    );
    // In a partition twice its size, the rest random bytes.
    let size = file_size(&iso).next_multiple_of(MB) * 2;
    let disk = s.join("disk.img");
    blank(&disk, size + 2 * MB);
    let table = format!("label: gpt\nstart=2048, size={}, type=L\n", size / 512);
    sfdisk(&disk, &table);
    put_bytes(&disk, MB, &rng.bytes(size as usize));
    put(&disk, MB, &iso);
    let layout = layout_of(&disk);
    let part = &layout.partitions[0];
    assert!(part.understood, "{part:?}");
    assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("iso9660"), Some("SMARTISO")));
    // The volume, and the appended partition past it (as its MBR says), not the rest.
    let data = fs::read(&iso).unwrap();
    let volume = u64::from(u32::from_le_bytes(data[0x8050..0x8054].try_into().unwrap())) * 2048;
    let appended = (0..4)
        .map(|i| {
            let e = 0x1BE + 16 * i;
            u64::from(u32::from_le_bytes(data[e + 8..e + 12].try_into().unwrap()))
                + u64::from(u32::from_le_bytes(data[e + 12..e + 16].try_into().unwrap()))
        })
        .max()
        .unwrap()
        * 512;
    assert!(appended > volume);
    // The volume (and what's appended), and at least the head kept of every partition.
    let ours = check_numbers("iso9660", &disk, MB, size, appended.max(HEAD_KEPT), 0);
    assert!(covers(&ours, MB, appended));
    let verify = |copy: &Path| -> Result<(), String> {
        let out = fresh(&s, "iso-files");
        let (ok, text) = try_sh("7z", &["x", "-y", &format!("-o{}", p(&out)), p(copy)]);
        if !ok {
            return Err(format!("7z: {text}"));
        }
        compare(&files, &out)?;
        // The appended FAT image too.
        let fat = s.join("efi-copy.img");
        let start = appended - 4 * MB;
        fs::write(&fat, read_range(copy, start, 4 * MB)).unwrap();
        let (ok, text) = try_sh("fsck.fat", &["-n", p(&fat)]);
        if ok { Ok(()) } else { Err(format!("fsck.fat: {text}")) }
    };
    check_copy(&s, &disk, MB, size, &layout, None, &verify);
    // A descriptor set that doesn't end, or a volume larger than its partition: in full.
    let mut bad = fs::read(&disk).unwrap();
    for i in 1..64 {
        let at = (MB + 0x8000 + i * 2048) as usize;
        if bad[at] == 255 {
            bad[at] = 2;
        }
    }
    assert!(!analyze_bytes(&bad, bad.len() as u64).unwrap().partitions[0].understood);
    let mut bad = fs::read(&disk).unwrap();
    let at = (MB + 0x8050) as usize;
    let huge = (size / 2048 + 1) as u32;
    bad[at..at + 4].copy_from_slice(&huge.to_le_bytes());
    bad[at + 4..at + 8].copy_from_slice(&huge.to_be_bytes());
    assert!(!analyze_bytes(&bad, bad.len() as u64).unwrap().partitions[0].understood);
}

/// Writes a partition table with sfdisk.
fn sfdisk(disk: &Path, script: &str) {
    let mut child = Command::new("sfdisk")
        .args(["-q", p(disk)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sfdisk: {}", String::from_utf8_lossy(&out.stderr));
}

// UDF.

/// (block size, first block of the partition, its blocks, blocks used in it) per udfinfo.
fn udf_numbers(img: &Path) -> (u64, u64, u64, u64) {
    let out = sh("udfinfo", &[p(img)]);
    let block = number_after(&out, "blocksize=").unwrap();
    let used = number_after(&out, "usedblocks=").unwrap();
    let line = out.lines().find(|l| l.contains("type=PSPACE")).unwrap();
    (block, number_after(line, "start=").unwrap(), number_after(line, "blocks=").unwrap(), used)
}

/// Whether 7z reads the UDF volume in `img` (it doesn't read 4 KiB blocks).
fn seven_zip_reads(img: &Path) -> bool {
    try_sh("7z", &["l", p(img)]).0
}

/// Checks a UDF copy: udfinfo reads it, it's closed, and (unless `listed` is None) 7z
/// extracts the files from it.
fn udf_ok(img: &Path, files: Option<&Tree>, s: &Scratch) -> Result<(), String> {
    let (ok, text) = try_sh("udfinfo", &[p(img)]);
    if !ok || !text.contains("integrity=closed") || text.contains("rror") {
        return Err(format!("udfinfo: {text}"));
    }
    let Some(files) = files else { return Ok(()) };
    let out = fresh(s, "udf-files");
    let (ok, text) = try_sh("7z", &["x", "-y", &format!("-o{}", p(&out)), p(img)]);
    if !ok {
        return Err(format!("7z: {text}"));
    }
    compare(files, &out)
}

/// Rewrites a UDF descriptor's tag checksum and CRC after changing it.
fn udf_retag(d: &mut [u8]) {
    let len = usize::from(u16::from_le_bytes([d[10], d[11]]));
    let crc = d[16..16 + len].iter().fold(0u16, |crc, &b| {
        let mut crc = crc ^ (u16::from(b) << 8);
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
        crc
    });
    d[8..10].copy_from_slice(&crc.to_le_bytes());
    d[4] = 0;
    d[4] = d[..16].iter().fold(0u8, |s, &b| s.wrapping_add(b));
}

#[test]
fn udf_variants() {
    if !have(&["mkudffs", "udfinfo", "7z"]) {
        return;
    }
    let s = Scratch::new("udf");
    let size = 64 * MB;
    let variants: &[(&str, &[&str], bool)] = &[
        ("hd-512", &["--blocksize=512"], true),
        ("hd-2048", &["--blocksize=2048"], true),
        ("hd-4096", &["--blocksize=4096"], true),
        ("udf-1.02", &["--udfrev=1.02"], true),
        ("udf-1.50", &["--udfrev=1.50"], true),
        ("space-table", &["--space=unalloctable"], true),
        // Freed space only, sparing tables, a virtual allocation table: copied in full.
        ("freed-bitmap", &["--space=freedbitmap"], false),
        ("cdrw-sparing", &["--media-type=cdrw"], false),
        ("dvdr-vat", &["--media-type=dvdr"], false),
    ];
    for &(name, args, understood) in variants {
        eprintln!("--- {name}");
        if !supports(&s, "mkudffs", args, size) {
            continue;
        }
        let img = s.join(&format!("{name}.img"));
        blank(&img, size);
        let mut all = vec!["--label=smartudf"];
        all.extend(args);
        all.push(p(&img));
        sh("mkudffs", &all);
        let layout = layout_of(&img);
        let part = &layout.partitions[0];
        assert_eq!(part.fs.as_deref(), Some("udf"));
        assert_eq!(part.understood, understood, "{part:?}");
        if !understood {
            assert_eq!(layout.used(), size);
            continue;
        }
        assert_eq!(part.label.as_deref(), Some("smartudf"));
        let (block, start, blocks, used) = udf_numbers(&img);
        let tail = size - (start + blocks) * block;
        let ours = check_numbers(name, &img, 0, size, (start + used) * block + tail, 0);
        assert!(covers(&ours, 0, start * block));
        let empty = Tree::new();
        let files = seven_zip_reads(&img).then_some(&empty);
        // Empty, it's all descriptors; those past the partition are copies. Drop the anchor's.
        let anchor = layout.extents.iter().find(|e| e.start <= 256 * block && 256 * block < e.start + e.len).copied();
        check_copy(&s, &img, 0, size, &layout, anchor, &|copy| udf_ok(copy, files, &s));
        fs::remove_file(&img).unwrap();
    }
}

#[test]
fn udf_open_or_inconsistent_is_copied_in_full() {
    if !have(&["mkudffs", "udfinfo"]) {
        return;
    }
    let s = Scratch::new("udf-open");
    let img = s.join("udf.img");
    let size = 64 * MB;
    blank(&img, size);
    sh("mkudffs", &["--blocksize=512", p(&img)]);
    let good = fs::read(&img).unwrap();
    assert!(layout_of(&img).partitions[0].understood);
    // mkudffs puts the integrity descriptor in block 128 (per udfinfo): mark it open.
    let at = 128 * 512;
    let mut data = good.clone();
    assert_eq!(u16::from_le_bytes([data[at], data[at + 1]]), 9);
    data[at + 28..at + 32].copy_from_slice(&0u32.to_le_bytes());
    udf_retag(&mut data[at..at + 512]);
    fs::write(&img, &data).unwrap();
    let layout = layout_of(&img);
    assert!(!layout.partitions[0].understood);
    assert_eq!(layout.used(), size);
    assert!(estimate_of(&img).partitions[0].understood);
    // A free block count that doesn't match the bitmap.
    let mut data = good.clone();
    let free = u32::from_le_bytes(data[at + 80..at + 84].try_into().unwrap());
    data[at + 80..at + 84].copy_from_slice(&(free - 1).to_le_bytes());
    udf_retag(&mut data[at..at + 512]);
    assert!(!analyze_bytes(&data, size).unwrap().partitions[0].understood);
    // A broken tag: the checksum no longer matches.
    let mut data = good.clone();
    data[at + 30] ^= 1;
    assert!(!analyze_bytes(&data, size).unwrap().partitions[0].understood);
}

// btrfs.

/// A btrfs image made from `files` (and `more`, which --rootdir takes too).
fn btrfs_image(s: &Scratch, name: &str, size: u64, args: &[&str], files: &Tree) -> PathBuf {
    let img = s.join(name);
    let src = fresh(s, &format!("{name}-src"));
    write_tree(files, &src);
    blank(&img, size);
    let mut all = vec!["-q", "-f", "-L", "smartbtrfs", "--rootdir", p(&src)];
    all.extend(args);
    all.push(p(&img));
    sh("mkfs.btrfs", &all);
    fs::remove_dir_all(&src).unwrap();
    img
}

/// Checks a btrfs copy: btrfs check passes, data checksums included.
fn btrfs_checked(img: &Path) -> Result<(), String> {
    let (ok, text) = try_sh("btrfs", &["check", "--readonly", "--check-data-csum", p(img)]);
    if ok { Ok(()) } else { Err(format!("btrfs check: {text}")) }
}

/// Checks a btrfs copy: btrfs check (data checksums too) passes, and btrfs restore gives
/// back `files`. (Some compressed extents the kernel writes, restore can't read: it's used
/// on images made by mkfs.btrfs.)
fn btrfs_ok(img: &Path, files: &Tree, s: &Scratch) -> Result<(), String> {
    btrfs_checked(img)?;
    let out = fresh(s, "btrfs-files");
    let (ok, text) = try_sh("btrfs", &["restore", "-s", p(img), p(&out)]);
    if !ok || text.contains("ERROR") {
        return Err(format!("btrfs restore: {text}"));
    }
    compare(files, &out)
}

/// What btrfs's own trees say a copy needs, on the device: every chunk but the data ones
/// whole, the data extents (in each copy), the first MiB and the superblocks.
fn btrfs_needed(img: &Path, size: u64) -> Vec<Extent> {
    struct Chunk {
        logical: u64,
        len: u64,
        data: bool,
        stripes: Vec<u64>,
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    for line in sh("btrfs", &["inspect-internal", "dump-tree", "-t", "chunk", p(img)]).lines() {
        let line = line.trim();
        if line.contains("CHUNK_ITEM") {
            let logical = line.split("CHUNK_ITEM ").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
            chunks.push(Chunk { logical, len: 0, data: false, stripes: Vec::new() });
        } else if let Some(c) = chunks.last_mut() {
            if line.starts_with("length ") {
                c.len = number_after(line, "length").unwrap();
                let kind = line.split("type ").nth(1).unwrap();
                c.data = kind.starts_with("DATA|") && !kind.contains("METADATA");
            } else if line.starts_with("stripe ") {
                c.stripes.push(number_after(line, "offset").unwrap());
            }
        }
    }
    let mut v = vec![Extent { start: 0, len: MB }];
    for at in [64 << 10, 64 * MB, 256 << 30] {
        if at + 4096 <= size {
            v.push(Extent { start: at, len: 4096 });
        }
    }
    let extents = sh("btrfs", &["inspect-internal", "dump-tree", "-t", "extent", p(img)]);
    for c in &chunks {
        if !c.data {
            v.extend(c.stripes.iter().map(|&s| Extent { start: s, len: c.len }));
        }
    }
    for line in extents.lines().filter(|l| l.contains(" EXTENT_ITEM ")) {
        let key = line.split("key (").nth(1).unwrap();
        let mut words = key.split([' ', ')']);
        let start: u64 = words.next().unwrap().parse().unwrap();
        let len: u64 = words.nth(1).unwrap().parse().unwrap();
        if let Some(c) = chunks.iter().find(|c| c.data && c.logical <= start && start < c.logical + c.len) {
            v.extend(c.stripes.iter().map(|&s| Extent { start: s + (start - c.logical), len }));
        }
    }
    util::normalize(v, size, 0, 1)
}

/// The numbers (exactly what btrfs's own trees say it needs), then a smart copy that btrfs
/// checks and restores, and the negative control.
fn check_btrfs(s: &Scratch, img: &Path, start: u64, size: u64, layout: &Layout, files: &Tree) {
    btrfs_numbers(s, img, start, size);
    check_copy(s, img, start, size, layout, None, &|copy| btrfs_ok(copy, files, s));
}

/// Our exact usage of the btrfs at `start..start + size` must be what its trees say.
fn btrfs_numbers(s: &Scratch, img: &Path, start: u64, size: u64) {
    let fs_img = cut_out(img, start, size, &s.join("btrfs-alone.img"));
    let super_out = sh("btrfs", &["inspect-internal", "dump-super", p(&fs_img)]);
    let bytes_used = number_after(&super_out, "\nbytes_used").unwrap();
    let dev_used = number_after(&super_out, "dev_item.bytes_used").unwrap();
    let dev_size = number_after(&super_out, "dev_item.total_bytes").unwrap();
    let mut needed = btrfs_needed(&fs_img, dev_size);
    needed.push(Extent { start: dev_size, len: size - dev_size });
    let needed: Vec<Extent> = util::normalize(needed, size, 0, 1)
        .into_iter()
        .map(|e| Extent { start: start + e.start, len: e.len })
        .collect();
    let ours = fs_usage(img, start, size).unwrap();
    eprintln!(
        "btrfs: ours {} bytes; its trees say {} (bytes_used {bytes_used}, chunks {dev_used}) of {size}",
        bytes_of(&ours),
        bytes_of(&needed)
    );
    assert_eq!(ours, needed, "btrfs: ours differ from what its trees say");
}

/// A whole-image btrfs: its name and label, then `check_btrfs`.
fn check_btrfs_image(s: &Scratch, img: &Path, files: &Tree) {
    let size = file_size(img);
    let layout = layout_of(img);
    let part = &layout.partitions[0];
    assert!(part.understood, "{part:?}");
    assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("btrfs"), Some("smartbtrfs")));
    check_btrfs(s, img, 0, size, &layout, files);
}

#[test]
fn btrfs_variants() {
    if !have(&["mkfs.btrfs", "btrfs"]) {
        return;
    }
    let s = Scratch::new("btrfs");
    let mut rng = Rng(0xB7F5);
    let variants: &[(&str, &[&str])] = &[
        ("default", &[]),
        // Compressed extents, with files that compress.
        ("zstd", &["--compress", "zstd"]),
        ("lzo", &["--compress", "lzo"]),
        // Subvolumes (made from directories of the source).
        ("subvolumes", &["--subvol", "rw:dir1", "--subvol", "ro:dir2"]),
        // Data in DUP: two copies of every extent.
        ("data-dup", &["-d", "dup", "-m", "dup"]),
        ("metadata-single", &["-m", "single"]),
        ("xxhash", &["--csum", "xxhash"]),
        ("small-nodes", &["-n", "4096"]),
        ("big-nodes", &["-n", "65536"]),
        // Block groups in the extent tree, no free space tree.
        ("old-layout", &["-O", "^block-group-tree,^free-space-tree"]),
        // Data and metadata mixed in the same chunks: kept whole.
        ("mixed", &["--mixed"]),
    ];
    for &(name, args) in variants {
        eprintln!("--- {name}");
        // Options that need --rootdir are tried with one (holding the subvolumes' directories).
        let empty = fresh(&s, "empty");
        fs::create_dir_all(empty.join("dir1")).unwrap();
        fs::create_dir_all(empty.join("dir2")).unwrap();
        if !supports(&s, "mkfs.btrfs", &[&["-q", "-f", "--rootdir", p(&empty)][..], args].concat(), 256 * MB) {
            continue;
        }
        let (mut files, _) = trees(&mut rng, 80, 4 * MB);
        files.extend(compressible(&mut rng, "text", 30));
        let img = btrfs_image(&s, &format!("{name}.img"), 256 * MB, args, &files);
        check_btrfs_image(&s, &img, &files);
        fs::remove_file(&img).unwrap();
    }
}

/// Rewrites the checksum of a btrfs superblock (CRC32C) after changing it.
fn btrfs_reseal(sb: &mut [u8]) {
    let crc = !util::crc32c(!0, &sb[32..4096]);
    sb[..4].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn btrfs_log_tree_errors_and_damage() {
    if !have(&["mkfs.btrfs"]) {
        return;
    }
    let s = Scratch::new("btrfs-damage");
    let mut rng = Rng(0xB7D);
    let (files, _) = trees(&mut rng, 40, MB);
    let img = btrfs_image(&s, "btrfs.img", 256 * MB, &[], &files);
    let size = file_size(&img);
    let good = fs::read(&img).unwrap();
    assert!(layout_of(&img).partitions[0].understood);
    let sb = 64 << 10;
    // A log tree to replay (fsync'd changes): not cleanly unmounted. So is an error flag.
    for (at, value) in [(0x60, 0x1234_0000u64), (0x38, 1 | 1 << 2)] {
        let mut data = good.clone();
        data[sb + at..sb + at + 8].copy_from_slice(&value.to_le_bytes());
        btrfs_reseal(&mut data[sb..sb + 4096]);
        fs::write(&img, &data).unwrap();
        let layout = layout_of(&img);
        assert!(!layout.partitions[0].understood, "field {at:#x}");
        assert_eq!(layout.used(), size);
        assert!(estimate_of(&img).partitions[0].understood);
    }
    // Two devices, a newer mirror, a bad checksum, an unknown feature: in full.
    let mut cases: Vec<Vec<u8>> = Vec::new();
    let mut data = good.clone();
    data[sb + 0x88..sb + 0x90].copy_from_slice(&2u64.to_le_bytes());
    btrfs_reseal(&mut data[sb..sb + 4096]);
    cases.push(data);
    let mut data = good.clone();
    let mirror = (64 * MB) as usize;
    let generation = u64::from_le_bytes(data[mirror + 0x48..mirror + 0x50].try_into().unwrap());
    data[mirror + 0x48..mirror + 0x50].copy_from_slice(&(generation + 1).to_le_bytes());
    btrfs_reseal(&mut data[mirror..mirror + 4096]);
    cases.push(data);
    let mut data = good.clone();
    data[sb + 0x200] ^= 1;
    cases.push(data);
    let mut data = good.clone();
    data[sb + 0xBC + 2] |= 0x80;
    btrfs_reseal(&mut data[sb..sb + 4096]);
    cases.push(data);
    for (i, data) in cases.iter().enumerate() {
        let layout = analyze_bytes(data, size).unwrap();
        check_invariants(&layout);
        assert!(!layout.partitions[0].understood, "case {i}");
    }
    // Any byte of the tree blocks read (the extent tree's leaf, say) is checksummed.
    let root = u64::from_le_bytes(good[sb + 0x50..sb + 0x58].try_into().unwrap());
    let chunk_out = sh("btrfs", &["inspect-internal", "dump-tree", "-t", "extent", p(&img)]);
    assert!(root > 0 && chunk_out.contains("EXTENT_ITEM"));
    for tree in ["extent", "chunk", "root"] {
        let out = sh("btrfs", &["inspect-internal", "dump-tree", "-t", tree, p(&img)]);
        let leaf: u64 = number_after(&out, "\nleaf").or_else(|| number_after(&out, "\nnode")).unwrap();
        // Logical to physical: dump-tree names it; map through the chunk it's in.
        let phys = btrfs_physical(&img, leaf);
        let mut data = good.clone();
        data[(phys + 200) as usize] ^= 0x40;
        let layout = analyze_bytes(&data, size).unwrap();
        assert!(!layout.partitions[0].understood, "{tree} tree block damage unnoticed");
    }
}

/// Where logical address `logical` is on the device (first copy), per btrfs's chunk tree.
fn btrfs_physical(img: &Path, logical: u64) -> u64 {
    let out = sh("btrfs", &["inspect-internal", "dump-tree", "-t", "chunk", p(img)]);
    let mut current: Option<(u64, u64)> = None;
    for line in out.lines().map(str::trim) {
        if line.contains("CHUNK_ITEM") {
            let start: u64 = line.split("CHUNK_ITEM ").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
            current = Some((start, 0));
        } else if line.starts_with("length ")
            && let Some(c) = current.as_mut()
        {
            c.1 = number_after(line, "length").unwrap();
        } else if line.starts_with("stripe 0")
            && let Some((start, len)) = current
            && start <= logical
            && logical < start + len
        {
            return number_after(line, "offset").unwrap() + (logical - start);
        }
    }
    panic!("{logical} in no chunk");
}

// XFS.

/// An XFS image made from `files`.
/// Whether mkfs.xfs can fill an image from a directory (`-p <dir>`). Older xfsprogs, such
/// as Ubuntu 22.04's 5.13, only take proto files.
fn xfs_fills_from_dir(s: &Scratch) -> bool {
    let src = fresh(s, "probe-src");
    let ok = supports(s, "mkfs.xfs", &["-q", "-f", "-p", p(&src)], 320 * MB);
    let _ = fs::remove_dir_all(&src);
    ok
}

fn xfs_image(s: &Scratch, name: &str, size: u64, args: &[&str], files: &Tree) -> PathBuf {
    let img = s.join(name);
    let src = fresh(s, &format!("{name}-src"));
    write_tree(files, &src);
    blank(&img, size);
    let mut all = vec!["-q", "-f", "-L", "smartxfs", "-p", p(&src)];
    all.extend(args);
    all.push(p(&img));
    sh("mkfs.xfs", &all);
    fs::remove_dir_all(&src).unwrap();
    img
}

/// A superblock field, per xfs_db.
fn xfs_field(img: &Path, name: &str) -> u64 {
    let out = sh("xfs_db", &["-r", "-c", "sb 0", "-c", &format!("p {name}"), p(img)]);
    number_after(&out, &format!("{name} =")).unwrap_or_else(|| panic!("{name}: {out}"))
}

/// Checks the files of an XFS image through xfs_db (no mounting): each one's block map,
/// read from the image, must give back its content.
fn xfs_files(img: &Path, files: &Tree) -> Result<(), String> {
    let (agblocks, block) = (xfs_field(img, "agblocks"), xfs_field(img, "blocksize"));
    let mut f = File::open(img).unwrap();
    for (name, data) in files {
        let (ok, out) = try_sh("xfs_db", &["-r", "-c", &format!("path /{name}"), "-c", "bmap", p(img)]);
        if !ok || out.contains("not found") || out.contains("rror") {
            return Err(format!("{name}: {out}"));
        }
        let mut content = vec![0u8; data.len()];
        // data offset O startblock S (A/B) count C flag F
        for line in out.lines().filter(|l| l.starts_with("data offset")) {
            let offset = number_after(line, "data offset").unwrap() * block;
            let (ag, agbno) = line.split('(').nth(1).unwrap().split(')').next().unwrap().split_once('/').unwrap();
            let (ag, agbno): (u64, u64) = (ag.parse().unwrap(), agbno.parse().unwrap());
            let count = number_after(line, "count").unwrap() * block;
            if number_after(line, "flag") != Some(0) || offset >= data.len() as u64 {
                continue;
            }
            let n = count.min(data.len() as u64 - offset) as usize;
            f.seek(SeekFrom::Start((ag * agblocks + agbno) * block)).unwrap();
            f.read_exact(&mut content[offset as usize..offset as usize + n]).map_err(|e| e.to_string())?;
        }
        if content != *data {
            return Err(format!("{name} differs"));
        }
    }
    Ok(())
}

/// Checks an XFS copy: xfs_repair -n finds nothing (no log to replay either), and every
/// file is intact.
fn xfs_ok(img: &Path, files: &Tree) -> Result<(), String> {
    let (ok, text) = try_sh("xfs_repair", &["-n", p(img)]);
    if !ok || text.contains("ALERT") {
        return Err(format!("xfs_repair: {text}"));
    }
    xfs_files(img, files)
}

/// What xfs_db says is free, as byte ranges of the file system.
fn xfs_free(img: &Path) -> Vec<Extent> {
    let (agblocks, block) = (xfs_field(img, "agblocks"), xfs_field(img, "blocksize"));
    let out = sh("xfs_db", &["-r", "-c", "freesp -d", p(img)]);
    out.lines()
        .filter_map(|l| {
            let v: Vec<u64> = l.split_whitespace().map(|w| w.parse().ok()).collect::<Option<_>>()?;
            let [ag, agbno, len] = v[..] else { return None };
            Some(Extent { start: (ag * agblocks + agbno) * block, len: len * block })
        })
        .collect()
}

/// The blocks on each AG's free list (AGFL): xfs_db counts them free, they're kept.
fn xfs_agfl(img: &Path) -> Vec<Extent> {
    let (agblocks, block) = (xfs_field(img, "agblocks"), xfs_field(img, "blocksize"));
    let mut v = Vec::new();
    for ag in 0..xfs_field(img, "agcount") {
        let out = sh(
            "xfs_db",
            &[
                "-r",
                "-c",
                &format!("agf {ag}"),
                "-c",
                "p flfirst fllast flcount",
                "-c",
                &format!("agfl {ag}"),
                "-c",
                "p bno",
                p(img),
            ],
        );
        let (first, last, count) = (
            number_after(&out, "flfirst =").unwrap(),
            number_after(&out, "fllast =").unwrap(),
            number_after(&out, "flcount =").unwrap(),
        );
        let entries: Vec<Option<u64>> = out
            .split(" = ")
            .last()
            .unwrap()
            .split_whitespace()
            .map(|e| e.split_once(':').and_then(|(_, b)| b.parse().ok()))
            .collect();
        let n = entries.len() as u64;
        for i in 0..count {
            let bno = entries[((first + i) % n) as usize].unwrap();
            v.push(Extent { start: (ag * agblocks + bno) * block, len: block });
        }
        assert!(count == 0 || (first + count - 1) % n == last);
    }
    v
}

/// Our exact usage of the XFS at `start..start + size` must be the complement of what xfs_db
/// says is free (but the AGFL: xfs_db counts free-list blocks free, they're kept), plus what
/// mod.rs keeps of every understood partition whatever its file system says: its first
/// `HEAD_KEPT` bytes (boot sectors and loaders live there) and what lies past the file
/// system. A free block in that head is copied: whether there is one depends on where
/// mkfs.xfs -p put things, which follows the order its source directory lists files in
/// (tmpfs and ext4 differ), so it's part of the reference rather than slack.
fn xfs_numbers(s: &Scratch, img: &Path, start: u64, size: u64) {
    let fs_img = cut_out(img, start, size, &s.join("xfs-alone.img"));
    let fs_size = xfs_field(&fs_img, "dblocks") * xfs_field(&fs_img, "blocksize");
    let agfl = xfs_agfl(&fs_img);
    let mut free: Vec<Extent> = xfs_free(&fs_img).into_iter().filter(|e| !agfl.contains(e)).collect();
    free.sort_unstable_by_key(|e| e.start);
    let mut expected =
        vec![Extent { start, len: HEAD_KEPT.min(size) }, Extent { start: start + fs_size, len: size - fs_size }];
    let mut next = 0;
    for e in &free {
        expected.push(Extent { start: start + next, len: e.start.saturating_sub(next) });
        next = next.max(e.start + e.len);
    }
    expected.push(Extent { start: start + next, len: fs_size.saturating_sub(next) });
    let expected = util::normalize(expected, file_size(img), 0, 1);
    let tool_used = fs_size - bytes_of(&free);
    let ours = check_numbers("xfs", img, start, size, tool_used, HEAD_KEPT + (size - fs_size));
    assert_eq!(ours, expected, "xfs: ours differ from the complement of what xfs_db says is free");
}

/// The numbers, then a smart copy that passes xfs_repair and gives back every file.
fn check_xfs(s: &Scratch, img: &Path, start: u64, size: u64, layout: &Layout, files: &Tree) {
    xfs_numbers(s, img, start, size);
    check_copy(s, img, start, size, layout, None, &|copy| xfs_ok(copy, files));
}

#[test]
fn xfs_variants() {
    if !have(&["mkfs.xfs", "xfs_db", "xfs_repair"]) {
        return;
    }
    let s = Scratch::new("xfs");
    let mut rng = Rng(0xF5);
    let size = 320 * MB;
    let variants: &[(&str, &[&str])] = &[
        ("default", &[]),
        // v4: no CRCs. mkfs warns it's deprecated.
        ("v4", &["-m", "crc=0"]),
        ("blocks-1k", &["-b", "size=1024"]),
        ("blocks-64k", &["-b", "size=65536"]),
        ("sectors-4k", &["-s", "size=4096"]),
        ("many-ags", &["-d", "agcount=12", "-l", "agnum=5"]),
        ("no-reflink", &["-m", "reflink=0,rmapbt=0"]),
        // A log stripe unit: unmount records padded, headers over several blocks.
        ("log-su-32k", &["-l", "su=32k"]),
        ("log-su-256k", &["-l", "su=256k"]),
        ("metadir", &["-m", "metadir=1"]),
    ];
    for &(name, args) in variants {
        eprintln!("--- {name}");
        // Each AG must hold the smallest log: many AGs need a bigger file system.
        let size = if name == "many-ags" { 1024 * MB } else { size };
        let empty = fresh(&s, "empty");
        if !supports(&s, "mkfs.xfs", &[&["-q", "-f", "-p", p(&empty)][..], args].concat(), size) {
            continue;
        }
        let (files, _) = trees(&mut rng, 60, 3 * MB);
        let img = xfs_image(&s, &format!("{name}.img"), size, args, &files);
        let layout = layout_of(&img);
        let part = &layout.partitions[0];
        assert!(part.understood, "{name}: {part:?}");
        assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("xfs"), Some("smartxfs")));
        check_xfs(&s, &img, 0, size, &layout, &files);
        fs::remove_file(&img).unwrap();
    }
}

/// Where the internal log starts, in bytes.
fn xfs_log_start(img: &Path) -> u64 {
    let (agblocks, block) = (xfs_field(img, "agblocks"), xfs_field(img, "blocksize"));
    let (logstart, agblklog) = (xfs_field(img, "logstart"), xfs_field(img, "agblklog"));
    ((logstart >> agblklog) * agblocks + (logstart & ((1 << agblklog) - 1))) * block
}

#[test]
fn xfs_unclean_log_or_damage_is_copied_in_full() {
    if !have(&["mkfs.xfs", "xfs_db"]) {
        return;
    }
    let s = Scratch::new("xfs-damage");
    if !xfs_fills_from_dir(&s) {
        return;
    }
    let size = 320 * MB;
    let mut rng = Rng(0xF6);
    let (files, _) = trees(&mut rng, 20, MB);
    for (name, args) in [("v5", &[][..]), ("v4", &["-m", "crc=0"][..])] {
        let img = xfs_image(&s, &format!("{name}.img"), size, args, &files);
        let good = fs::read(&img).unwrap();
        assert!(layout_of(&img).partitions[0].understood);
        let log = xfs_log_start(&img) as usize;
        // mkfs leaves an unmount record at the start of the log: header, then its operation.
        assert_eq!(&good[log..log + 4], &[0xFE, 0xED, 0xBA, 0xBE]);
        let op = log + 512;
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        let mut data = good.clone();
        data[op + 9] = 0;
        cases.push(("not an unmount record", data));
        let mut data = good.clone();
        data[log + 40..log + 44].copy_from_slice(&2u32.to_be_bytes());
        cases.push(("two operations", data));
        // A record written after it (as a crash would leave it): cycle 1 past the head.
        let mut data = good.clone();
        data[log + 1024..log + 1028].copy_from_slice(&1u32.to_be_bytes());
        cases.push(("written past the head", data));
        let mut data = good.clone();
        data[log + 304] ^= 1;
        cases.push(("log of another file system", data));
        for (why, data) in &cases {
            fs::write(&img, data).unwrap();
            let layout = layout_of(&img);
            assert!(!layout.partitions[0].understood, "{name}: {why}");
            assert_eq!(layout.used(), size);
            assert!(estimate_of(&img).partitions[0].understood, "{name}: {why} (estimate)");
        }
        // Damage in what's read: the superblock, the first AGF and AGI, a free space tree root.
        let sect = xfs_field(&img, "sectsize") as usize;
        let block = xfs_field(&img, "blocksize") as usize;
        let mut targets = vec![(124, "superblock"), (sect + 52, "AGF free count")];
        let agf = &good[sect..2 * sect];
        let bno_root = u32::from_be_bytes(agf[16..20].try_into().unwrap()) as usize;
        targets.push((bno_root * block + 20, "free space tree"));
        if name == "v5" {
            targets.push((sect + 100, "AGF"));
            targets.push((2 * sect + 100, "AGI"));
        }
        for (at, what) in targets {
            let mut data = good.clone();
            data[at] ^= 0x04;
            let layout = analyze_bytes(&data, size).unwrap();
            check_invariants(&layout);
            assert!(!layout.partitions[0].understood, "{name}: {what} damage unnoticed");
        }
        fs::remove_file(&img).unwrap();
    }
}

// F2FS.

/// An F2FS image made from `files` (with sload.f2fs, no mounting).
fn f2fs_image(s: &Scratch, name: &str, size: u64, args: &[&str], files: &Tree) -> PathBuf {
    let img = s.join(name);
    let src = fresh(s, &format!("{name}-src"));
    write_tree(files, &src);
    blank(&img, size);
    let mut all = vec!["-q", "-f", "-l", "smartf2fs"];
    all.extend(args);
    all.push(p(&img));
    sh("mkfs.f2fs", &all);
    sh("sload.f2fs", &["-f", p(&src), p(&img)]);
    fs::remove_dir_all(&src).unwrap();
    img
}

/// A field as dump.f2fs prints it: `name [0x… : decimal]`.
fn f2fs_field(out: &str, name: &str) -> u64 {
    let line = out.lines().find(|l| l.split_whitespace().next() == Some(name)).unwrap_or_else(|| panic!("{name}"));
    line.rsplit(": ").next().unwrap().trim_end_matches(']').trim().parse().unwrap()
}

/// Checks an F2FS copy: fsck.f2fs finds nothing, and dump.f2fs gives back every file (by the
/// inode number fsck.f2fs's tree lists for it) as it does from the original, `files`.
fn f2fs_ok(img: &Path, files: &Tree, s: &Scratch) -> Result<(), String> {
    let dumped = f2fs_dump(img, files, s)?;
    match files.iter().find(|(name, data)| dumped.get(*name) != Some(*data)) {
        Some((name, _)) => Err(format!("{name} differs")),
        None => Ok(()),
    }
}

/// What dump.f2fs gives for each of `files` (after fsck.f2fs finds nothing wrong). It gets
/// files kept in their inode (inline data) wrong with extra attributes, so copies are held
/// to what it gives for the original.
fn f2fs_dump(img: &Path, files: &Tree, s: &Scratch) -> Result<Tree, String> {
    let (ok, text) = try_sh("fsck.f2fs", &["-f", "--dry-run", p(img)]);
    if !ok || text.contains("[Fail]") {
        return Err(format!("fsck.f2fs: {text}"));
    }
    let (_, tree) = try_sh("fsck.f2fs", &["-t", p(img)]);
    // "|   |-- name <ino = 0x6>, ..." (the last in a directory with "`-- "): the depth is where
    // that starts.
    let mut path: Vec<String> = Vec::new();
    let mut inodes = Tree::new();
    for line in tree.lines() {
        let Some(at) = line.find("|-- ").or_else(|| line.find("`-- ")) else {
            continue;
        };
        let Some((name, rest)) = line[at + 4..].split_once(" <ino = 0x") else {
            continue;
        };
        let depth = at / 4;
        path.truncate(depth);
        path.push(name.to_owned());
        let ino = u64::from_str_radix(rest.split('>').next().unwrap(), 16).map_err(|e| e.to_string())?;
        inodes.insert(path.join("/"), ino.to_string().into_bytes());
    }
    let work = fresh(s, "f2fs-dump");
    let mut dumped = Tree::new();
    for name in files.keys() {
        let ino = inodes.get(name).ok_or_else(|| format!("{name} missing"))?;
        let ino = String::from_utf8_lossy(ino).into_owned();
        let _ = fs::remove_dir_all(work.join("lost_found"));
        let (_, text) = run_in(&work, "dump.f2fs", &["-i", &ino, p(img)], "y\n");
        let base = name.rsplit('/').next().unwrap();
        let data = fs::read(work.join("lost_found").join(base)).map_err(|e| format!("{name}: {e} ({text})"))?;
        dumped.insert(name.clone(), data);
    }
    let expected_dirs = files.keys().filter_map(|k| k.rsplit_once('/').map(|(d, _)| d.to_owned()));
    let dirs: std::collections::BTreeSet<String> = expected_dirs.collect();
    match inodes.keys().find(|k| !files.contains_key(*k) && !dirs.contains(*k) && *k != "lost+found") {
        Some(extra) => Err(format!("unexpected file {extra}")),
        None => Ok(dumped),
    }
}

/// The numbers (exactly: the areas before the main one, and its valid blocks), then a smart
/// copy that fsck.f2fs accepts and that gives back every file.
fn check_f2fs(s: &Scratch, img: &Path, start: u64, size: u64, layout: &Layout, files: &Tree) {
    let reference = f2fs_numbers(s, img, start, size, files);
    check_copy(s, img, start, size, layout, None, &|copy| f2fs_ok(copy, &reference, s));
}

/// Our exact usage of the F2FS at `start..start + size` must be its areas before the main one
/// and its valid blocks. Returns what dump.f2fs reads of `files` there.
fn f2fs_numbers(s: &Scratch, img: &Path, start: u64, size: u64, files: &Tree) -> Tree {
    let fs_img = cut_out(img, start, size, &s.join("f2fs-alone.img"));
    let out = sh("dump.f2fs", &["-d", "1", p(&fs_img)]);
    let main = f2fs_field(&out, "main_blkaddr");
    let valid = f2fs_field(&out, "valid_block_count");
    let main_end = (main + f2fs_field(&out, "segment_count_main") * 512) * 4096;
    let used = (main + valid) * 4096;
    check_numbers("f2fs", img, start, size, used + (size - main_end), 0);
    // What the tools read from the original: every file, but those dump.f2fs misreads.
    let reference = f2fs_dump(&fs_img, files, s).unwrap();
    let wrong = files.iter().filter(|(name, data)| reference[*name] != **data).count();
    let inline = files.values().filter(|d| d.len() <= 3400).count();
    assert!(wrong <= inline, "{wrong} files differ in the original");
    reference
}

#[test]
fn f2fs_variants() {
    if !have(&["mkfs.f2fs", "sload.f2fs", "fsck.f2fs", "dump.f2fs"]) {
        return;
    }
    let s = Scratch::new("f2fs");
    let mut rng = Rng(0xF2F5);
    let size = 192 * MB;
    let variants: &[(&str, &[&str])] = &[
        ("default", &[]),
        ("checksums", &["-O", "extra_attr,inode_checksum,sb_checksum"]),
        ("compression", &["-O", "extra_attr,compression"]),
        ("big-sections", &["-s", "4", "-z", "2"]),
        ("overprovision", &["-o", "20"]),
    ];
    for &(name, args) in variants {
        eprintln!("--- {name}");
        if !supports(&s, "mkfs.f2fs", &[&["-q", "-f"][..], args].concat(), size) {
            continue;
        }
        let (files, _) = trees(&mut rng, 60, 3 * MB);
        let img = f2fs_image(&s, &format!("{name}.img"), size, args, &files);
        let layout = layout_of(&img);
        let part = &layout.partitions[0];
        assert!(part.understood, "{name}: {part:?}");
        assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("f2fs"), Some("smartf2fs")));
        check_f2fs(&s, &img, 0, size, &layout, &files);
        fs::remove_file(&img).unwrap();
    }
}

/// The current checkpoint pack of an F2FS image: (first block, header).
fn f2fs_checkpoint(data: &[u8]) -> (usize, Vec<u8>) {
    let sb = &data[1024..];
    let cp = u32::from_le_bytes(sb[76..80].try_into().unwrap()) as usize;
    let version = |b: usize| u64::from_le_bytes(data[b * 4096..b * 4096 + 8].try_into().unwrap());
    let pack = if version(cp + 512) > version(cp) { cp + 512 } else { cp };
    (pack, data[pack * 4096..pack * 4096 + 4096].to_vec())
}

/// Rewrites a checkpoint block's CRC after changing it.
fn f2fs_reseal(block: &mut [u8]) {
    let at = u32::from_le_bytes(block[164..168].try_into().unwrap()) as usize;
    let crc = util::crc32_le(0xF2F5_2010, &block[..at]);
    block[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn f2fs_unclean_or_damaged_is_copied_in_full() {
    if !have(&["mkfs.f2fs", "sload.f2fs"]) {
        return;
    }
    let s = Scratch::new("f2fs-damage");
    let size = 192 * MB;
    let mut rng = Rng(0xF2F6);
    let (files, _) = trees(&mut rng, 30, MB);
    let img = f2fs_image(&s, "f2fs.img", size, &[], &files);
    let good = fs::read(&img).unwrap();
    assert!(layout_of(&img).partitions[0].understood);
    let (pack, header) = f2fs_checkpoint(&good);
    let total = u32::from_le_bytes(header[136..140].try_into().unwrap()) as usize;
    let block = |b: usize| b * 4096..b * 4096 + 4096;
    // Without the unmount flag (in both copies of the header, resealed): not clean.
    let mut data = good.clone();
    for b in [pack, pack + total - 1] {
        data[b * 4096 + 132] &= !1;
        f2fs_reseal(&mut data[block(b)]);
    }
    let layout = analyze_bytes(&data, size).unwrap();
    assert!(!layout.partitions[0].understood);
    assert_eq!(layout.used(), size);
    // fsync'd node blocks waiting to be rolled forward: one in the warm node log's next
    // block, stamped with this checkpoint's version.
    let main = u32::from_le_bytes(good[1024 + 92..1024 + 96].try_into().unwrap()) as usize;
    let segno = u32::from_le_bytes(header[40..44].try_into().unwrap()) as usize;
    let offset = u16::from_le_bytes(header[70..72].try_into().unwrap()) as usize;
    let at = (main + segno * 512 + offset) * 4096;
    let mut data = good.clone();
    let version = &header[..8];
    data[at + 4072 + 12..at + 4072 + 20].copy_from_slice(version);
    if u32::from_le_bytes(header[132..136].try_into().unwrap()) & 0x40 != 0 {
        // CRC recovery: the checkpoint's CRC in the top half.
        let crc_at = u32::from_le_bytes(header[164..168].try_into().unwrap()) as usize;
        data[at + 4072 + 16..at + 4072 + 20].copy_from_slice(&header[crc_at..crc_at + 4]);
    }
    let layout = analyze_bytes(&data, size).unwrap();
    assert!(!layout.partitions[0].understood, "roll-forward unnoticed");
    // Damage: the checkpoint's CRC, a SIT entry's count, the superblock's layout.
    let mut cases = Vec::new();
    let mut data = good.clone();
    data[pack * 4096 + 50] ^= 1;
    data[(pack + total - 1) * 4096 + 50] ^= 1;
    // Both packs broken: nothing valid left.
    let other = if pack == u32::from_le_bytes(good[1024 + 76..1024 + 80].try_into().unwrap()) as usize {
        pack + 512
    } else {
        pack - 512
    };
    data[other * 4096 + 50] ^= 1;
    cases.push(("checkpoints", data));
    let sit = u32::from_le_bytes(good[1024 + 80..1024 + 84].try_into().unwrap()) as usize;
    let mut data = good.clone();
    for copy in [0, 1] {
        let at = (sit + copy * 512) * 4096;
        data[at] ^= 1;
    }
    cases.push(("SIT entry count", data));
    let mut data = good.clone();
    data[1024 + 92] ^= 0x10;
    data[4096 + 1024 + 92] ^= 0x10;
    cases.push(("main area address", data));
    for (what, data) in cases {
        let layout = analyze_bytes(&data, size).unwrap();
        check_invariants(&layout);
        assert!(!layout.partitions[0].understood, "{what} damage unnoticed");
    }
}

// ext4 (inside LVs).

/// An ext4 image from `keep` and `gone` (deleted afterwards, so free space holds their data).
fn ext4_image(s: &Scratch, name: &str, size: u64, keep: &Tree, gone: &Tree) -> PathBuf {
    let img = s.join(name);
    let src = fresh(s, &format!("{name}-src"));
    write_tree(keep, &src);
    write_tree(gone, &src);
    blank(&img, size);
    sh("mke2fs", &["-q", "-F", "-t", "ext4", "-L", "smartlv", "-d", p(&src), p(&img)]);
    let script = s.join(&format!("{name}-rm"));
    fs::write(&script, gone.keys().map(|k| format!("rm /{k}\n")).collect::<String>()).unwrap();
    sh("debugfs", &["-w", "-f", p(&script), p(&img)]);
    fs::remove_dir_all(&src).unwrap();
    img
}

/// Checks an ext4 copy: e2fsck finds nothing, and debugfs gives back every file.
fn ext4_ok(img: &Path, files: &Tree, s: &Scratch) -> Result<(), String> {
    let (ok, text) = try_sh("e2fsck", &["-fn", p(img)]);
    if !ok {
        return Err(format!("e2fsck: {text}"));
    }
    let out = fresh(s, "ext4-files");
    let (_, text) = try_sh("debugfs", &["-R", &format!("rdump / {}", p(&out)), p(img)]);
    compare(files, &out).map_err(|e| format!("{e} ({text})"))
}

// LVM2, made by hand the way LVM writes it (checked against real volume groups in the root
// test): the label in sector 1, a metadata area from 4 KiB to 1 MiB with the text in its ring
// buffer, 4 MiB extents from 1 MiB on.

const PV_UUID: &str = "kiq2MRnR9aRuSbcFLOI7cdlltu3eOcxL";
const PV1_UUID: &str = "Zx9ErZQOsXO24IBHkfLYg5oowf34VQbE";
const EXTENT: u64 = 4 * MB;
const PE_START: u64 = MB;

fn dashed(uuid: &str) -> String {
    let cuts = [6, 10, 14, 18, 22, 26];
    let mut out = String::new();
    let mut last = 0;
    for c in cuts {
        out += &uuid[last..c];
        out.push('-');
        last = c;
    }
    out + &uuid[last..]
}

/// A hand-made LV: name, status, and segments (first logical extent, extents, body).
struct HandLv {
    name: &'static str,
    status: &'static str,
    segments: Vec<(u64, u64, String)>,
}

const VISIBLE: &str = "\"READ\", \"WRITE\", \"VISIBLE\"";
const HIDDEN: &str = "\"READ\", \"WRITE\"";

fn linear(pv: &str, pe: u64) -> String {
    format!("type = \"striped\"\nstripe_count = 1\n\nstripes = [\n\"{pv}\", {pe}\n]\n")
}

fn hand_lv(name: &'static str, status: &'static str, segments: &[(u64, u64, String)]) -> HandLv {
    HandLv { name, status, segments: segments.to_vec() }
}

/// Volume group metadata as LVM writes it.
fn vg_text(seqno: u64, pe_count: u64, lvs: &[HandLv]) -> String {
    let dev_size = (PE_START + pe_count * EXTENT) / 512;
    let mut t = format!(
        "ddguitest9 {{\nid = \"Nda1p3-L0pO-CePJ-dzp0-LgcD-cTQE-7S0c0t\"\nseqno = {seqno}\nformat = \"lvm2\"\n\
         status = [\"RESIZEABLE\", \"READ\", \"WRITE\"]\nflags = []\nextent_size = 8192\nmax_lv = 0\nmax_pv = 0\n\
         metadata_copies = 0\n\nphysical_volumes {{\n\npv0 {{\nid = \"{}\"\ndevice = \"/dev/loop9\"\n\n\
         status = [\"ALLOCATABLE\"]\nflags = []\ndev_size = {dev_size}\npe_start = 2048\npe_count = {pe_count}\n}}\n\n\
         pv1 {{\nid = \"{}\"\ndevice = \"/dev/loop8\"\n\nstatus = [\"ALLOCATABLE\"]\nflags = []\ndev_size = 409600\n\
         pe_start = 2048\npe_count = 49\n}}\n}}\n\nlogical_volumes {{\n",
        dashed(PV_UUID),
        dashed(PV1_UUID)
    );
    for lv in lvs {
        t += &format!(
            "\n{} {{\nid = \"QbEzyx-ErZQ-OsXO-24IB-HkfL-Yg5o-owf34V\"\nstatus = [{}]\nflags = []\n\
             creation_time = 1790340673\ncreation_host = \"test\"\nsegment_count = {}\n",
            lv.name,
            lv.status,
            lv.segments.len()
        );
        for (i, (start, count, body)) in lv.segments.iter().enumerate() {
            t += &format!("\nsegment{} {{\nstart_extent = {start}\nextent_count = {count}\n\n{body}}}\n", i + 1);
        }
        t += "}\n";
    }
    t += "}\n\n}\n# Generated by LVM2 version 2.03.42(2) (2026-08-06): Fri Sep 25 16:21:13 2026\n\n\
          contents = \"Text Format Volume Group\"\nversion = 1\n\ndescription = \"\"\n\n\
          creation_host = \"test\"\t# Linux test\ncreation_time = 1790340673\t# Fri Sep 25 16:21:13 2026\n\n";
    t
}

fn lvm_crc(data: &[u8]) -> u32 {
    util::crc32_le(0xF597_A6CF, data)
}

/// How to write a hand-made PV's first MiB (and its second metadata area, if any).
struct PvMeta<'t> {
    pe_count: u64,
    /// The text, where it goes in the ring buffer (from the area's start), and whether the
    /// area is marked ignored.
    text: Option<&'t str>,
    text_at: u64,
    ignored: bool,
    /// A second metadata area at the end (after the extents): its text.
    second: Option<&'t str>,
    used_flag: bool,
}

const MDA: (u64, u64) = (4096, MB - 4096);

fn second_mda(pe_count: u64) -> (u64, u64) {
    (PE_START + pe_count * EXTENT, 512 << 10)
}

/// A metadata area header, and its text placed in the ring (as (offset, bytes) writes).
fn mda_writes(area: (u64, u64), text: Option<&str>, text_at: u64, ignored: bool) -> Vec<(u64, Vec<u8>)> {
    let (offset, size) = area;
    let mut h = vec![0u8; 512];
    h[4..20].copy_from_slice(b" LVM2 x[5A%r0N*>");
    h[20..24].copy_from_slice(&1u32.to_le_bytes());
    h[24..32].copy_from_slice(&offset.to_le_bytes());
    h[32..40].copy_from_slice(&size.to_le_bytes());
    let mut writes = Vec::new();
    if let Some(text) = text {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let len = bytes.len() as u64;
        h[40..48].copy_from_slice(&text_at.to_le_bytes());
        h[48..56].copy_from_slice(&len.to_le_bytes());
        h[56..60].copy_from_slice(&lvm_crc(&bytes).to_le_bytes());
        let first = len.min(size - text_at);
        writes.push((offset + text_at, bytes[..first as usize].to_vec()));
        writes.push((offset + 512, bytes[first as usize..].to_vec()));
    }
    if ignored {
        h[60..64].copy_from_slice(&1u32.to_le_bytes());
    }
    let crc = lvm_crc(&h[4..]);
    h[..4].copy_from_slice(&crc.to_le_bytes());
    writes.insert(0, (offset, h));
    writes
}

/// Writes a hand-made PV's label and metadata into `img`.
fn write_pv(img: &Path, m: &PvMeta) {
    let mut label = [0u8; 512];
    label[..8].copy_from_slice(b"LABELONE");
    label[8..16].copy_from_slice(&1u64.to_le_bytes());
    label[20..24].copy_from_slice(&32u32.to_le_bytes());
    label[24..32].copy_from_slice(b"LVM2 001");
    label[32..64].copy_from_slice(PV_UUID.as_bytes());
    let dev_size = file_size(img);
    let mut at = 64;
    let entry = |label: &mut [u8; 512], at: &mut usize, a: u64, b: u64| {
        label[*at..*at + 8].copy_from_slice(&a.to_le_bytes());
        label[*at + 8..*at + 16].copy_from_slice(&b.to_le_bytes());
        *at += 16;
    };
    label[at..at + 8].copy_from_slice(&dev_size.to_le_bytes());
    at += 8;
    entry(&mut label, &mut at, PE_START, 0);
    entry(&mut label, &mut at, 0, 0);
    entry(&mut label, &mut at, MDA.0, MDA.1);
    if m.second.is_some() {
        let (o, n) = second_mda(m.pe_count);
        entry(&mut label, &mut at, o, n);
    }
    entry(&mut label, &mut at, 0, 0);
    // The header extension: version 2, flags, no boot loader areas.
    label[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    label[at + 4..at + 8].copy_from_slice(&u32::from(m.used_flag).to_le_bytes());
    let crc = lvm_crc(&label[20..]);
    label[16..20].copy_from_slice(&crc.to_le_bytes());
    // The first MiB: zeros but for these.
    put_bytes(img, 0, &vec![0; MB as usize]);
    put_bytes(img, 512, &label);
    let mut writes = mda_writes(MDA, m.text, m.text_at, m.ignored);
    if let Some(second) = m.second {
        writes.extend(mda_writes(second_mda(m.pe_count), Some(second), 512, false));
    }
    for (at, bytes) in writes {
        put_bytes(img, at, &bytes);
    }
}

/// An LV's extents on pv0: (first PE, count), in logical order.
type LvMap = Vec<(u64, u64)>;

/// What's inside a hand-made LV, to check it.
enum Content {
    Ext4(Tree),
    Xfs(Tree),
    Btrfs(Tree),
    /// Kept whole: must be byte for byte what it was.
    Whole,
}

/// Reads an LV (its extents on pv0) out of a PV image.
fn read_lv(img: &Path, map: &LvMap) -> Vec<u8> {
    map.iter().flat_map(|&(pe, n)| read_range(img, PE_START + pe * EXTENT, n * EXTENT)).collect()
}

/// Writes `data` (an LV's content) into a PV image along its extents.
fn write_lv(img: &Path, map: &LvMap, data: &[u8]) {
    let mut at = 0;
    for &(pe, n) in map {
        let len = (n * EXTENT) as usize;
        put_bytes(img, PE_START + pe * EXTENT, &data[at..at + len]);
        at += len;
    }
}

/// Checks a copy of a hand-made PV: its first MiB as it was, every LV per its content.
fn lvm_ok(copy: &Path, original: &Path, lvs: &[(&str, LvMap, &Content)], s: &Scratch) -> Result<(), String> {
    if read_range(copy, 0, PE_START) != read_range(original, 0, PE_START) {
        return Err("label or metadata area differs".into());
    }
    for (name, map, content) in lvs {
        let lv = s.join("lv.img");
        fs::write(&lv, read_lv(copy, map)).unwrap();
        let checked = match content {
            Content::Ext4(files) => ext4_ok(&lv, files, s),
            Content::Xfs(files) => xfs_ok(&lv, files),
            Content::Btrfs(files) => btrfs_ok(&lv, files, s),
            Content::Whole if read_lv(original, map) == fs::read(&lv).unwrap() => Ok(()),
            Content::Whole => Err("differs".into()),
        };
        checked.map_err(|e| format!("LV {name}: {e}"))?;
    }
    Ok(())
}

/// What our analysis must say a hand-made PV uses: its first MiB, what each plain LV's file
/// system uses (as the analysis of that LV alone says, mapped back), everything of the other
/// LVs, and what lies past the extents.
fn lvm_expected(img: &Path, pe_count: u64, lvs: &[(&str, LvMap, &Content)], s: &Scratch) -> Vec<Extent> {
    let size = file_size(img);
    let mut v = vec![Extent { start: 0, len: PE_START }];
    let end = PE_START + pe_count * EXTENT;
    v.push(Extent { start: end, len: size - end });
    for (_, map, content) in lvs {
        let whole = |v: &mut Vec<Extent>| {
            v.extend(map.iter().map(|&(pe, n)| Extent { start: PE_START + pe * EXTENT, len: n * EXTENT }))
        };
        if let Content::Whole = content {
            whole(&mut v);
            continue;
        }
        let lv = s.join("lv-expected.img");
        fs::write(&lv, read_lv(img, map)).unwrap();
        let inner = fs_usage(&lv, 0, file_size(&lv)).unwrap();
        // LV offset to PV offset, segment by segment.
        let mut lv_at = 0;
        for &(pe, n) in map {
            let len = n * EXTENT;
            for e in &inner {
                let (from, to) = (e.start.max(lv_at), (e.start + e.len).min(lv_at + len));
                if from < to {
                    v.push(Extent { start: PE_START + pe * EXTENT + (from - lv_at), len: to - from });
                }
            }
            lv_at += len;
        }
    }
    util::normalize(v, size, 0, 1)
}

/// A hand-made PV of 200 extents: an ext4 LV in two pieces, XFS and btrfs LVs, a thin pool
/// (its data, metadata and spare), an LV spanning another PV, and free extents full of
/// garbage. Returns the image, its LVs and their contents.
/// LVs as the tests know them: name, extents on pv0, content.
type LvList = Vec<(&'static str, LvMap, Content)>;

fn hand_pv(s: &Scratch, rng: &mut Rng) -> (PathBuf, Vec<HandLv>, LvList) {
    let pe_count = 200;
    let img = s.join("pv.img");
    blank(&img, PE_START + pe_count * EXTENT + MB);
    // Garbage everywhere first (repeated, it's cheap): free extents keep it.
    let junk = rng.bytes(MB as usize);
    for i in 0..file_size(&img) / MB {
        put_bytes(&img, i * MB, &junk);
    }
    let (keep, gone) = trees(rng, 80, 2 * MB);
    let ext = ext4_image(s, "ext.img", 16 * EXTENT, &keep, &gone);
    let ext_map = vec![(0, 8), (40, 8)];
    write_lv(&img, &ext_map, &fs::read(&ext).unwrap());
    let (xfs_files, _) = trees(rng, 40, 3 * MB);
    let xfs = xfs_image(s, "xfs.img", 76 * EXTENT, &[], &xfs_files);
    let xfs_map = vec![(48, 76)];
    write_lv(&img, &xfs_map, &fs::read(&xfs).unwrap());
    let (btrfs_files, _) = trees(rng, 40, 3 * MB);
    let btr = btrfs_image(s, "btr.img", 32 * EXTENT, &[], &btrfs_files);
    let btr_map = vec![(124, 32)];
    write_lv(&img, &btr_map, &fs::read(&btr).unwrap());
    for f in [ext, xfs, btr] {
        fs::remove_file(f).unwrap();
    }
    let lvs = vec![
        hand_lv("ext", VISIBLE, &[(0, 8, linear("pv0", 0)), (8, 8, linear("pv0", 40))]),
        hand_lv("xfs", VISIBLE, &[(0, 76, linear("pv0", 48))]),
        hand_lv("btr", VISIBLE, &[(0, 32, linear("pv0", 124))]),
        hand_lv(
            "pool",
            VISIBLE,
            &[(0, 10, "type = \"thin-pool\"\nmetadata = \"pool_tmeta\"\npool = \"pool_tdata\"\ntransaction_id = 1\nchunk_size = 128\n".into())],
        ),
        hand_lv("pool_tdata", HIDDEN, &[(0, 10, linear("pv0", 160))]),
        hand_lv("pool_tmeta", HIDDEN, &[(0, 1, linear("pv0", 170))]),
        hand_lv("lvol0_pmspare", HIDDEN, &[(0, 1, linear("pv0", 171))]),
        hand_lv("thin1", VISIBLE, &[(0, 25, "type = \"thin\"\nthin_pool = \"pool\"\ntransaction_id = 0\ndevice_id = 1\n".into())]),
        hand_lv("span", VISIBLE, &[(0, 4, linear("pv1", 0)), (4, 4, linear("pv0", 175))]),
    ];
    let contents = vec![
        ("ext", ext_map, Content::Ext4(keep)),
        ("xfs", xfs_map, Content::Xfs(xfs_files)),
        ("btr", btr_map, Content::Btrfs(btrfs_files)),
        ("pool_tdata", vec![(160, 10)], Content::Whole),
        ("pool_tmeta", vec![(170, 1)], Content::Whole),
        ("lvol0_pmspare", vec![(171, 1)], Content::Whole),
        ("span", vec![(175, 4)], Content::Whole),
    ];
    let text = vg_text(7, pe_count, &lvs);
    write_pv(
        &img,
        &PvMeta { pe_count, text: Some(&text), text_at: 3072, ignored: false, second: None, used_flag: true },
    );
    (img, lvs, contents)
}

#[test]
fn lvm_hand_made_volume_group() {
    if !have(&["mke2fs", "e2fsck", "debugfs", "mkfs.xfs", "xfs_db", "xfs_repair", "mkfs.btrfs", "btrfs"]) {
        return;
    }
    let s = Scratch::new("lvm");
    if !xfs_fills_from_dir(&s) {
        return;
    }
    let mut rng = Rng(0x1F3);
    let (img, lvs, contents) = hand_pv(&s, &mut rng);
    let size = file_size(&img);
    let layout = layout_of(&img);
    let part = &layout.partitions[0];
    assert!(part.understood, "{part:?}");
    assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("LVM2_member"), Some("ddguitest9")));
    let refs: Vec<(&str, LvMap, &Content)> = contents.iter().map(|(n, m, c)| (*n, m.clone(), c)).collect();
    let expected = lvm_expected(&img, 200, &refs, &s);
    let ours = fs_usage(&img, 0, size).unwrap();
    eprintln!("LVM PV: ours {} bytes, expected {} of {size}", bytes_of(&ours), bytes_of(&expected));
    assert_eq!(ours, expected);
    // Free extents (and the thin LV, which has none of its own) aren't copied.
    for (pe, n) in [(8, 32), (156, 4), (172, 3), (179, 21)] {
        assert_eq!(util::overlap(&ours, PE_START + pe * EXTENT, n * EXTENT), 0, "free extents {pe}+{n} copied");
    }
    check_copy(&s, &img, 0, size, &layout, None, &|copy| lvm_ok(copy, &img, &refs, &s));

    // The same metadata wrapping around the end of the ring buffer: the same layout.
    let text = vg_text(7, 200, &lvs);
    let wrap_at = MDA.1 - 700;
    write_pv(
        &img,
        &PvMeta { pe_count: 200, text: Some(&text), text_at: wrap_at, ignored: false, second: None, used_flag: true },
    );
    assert_eq!(layout_of(&img).extents, layout.extents);

    // A second metadata area at the end: the higher sequence number wins, either way.
    let older = vg_text(6, 200, &lvs[..1]);
    write_pv(
        &img,
        &PvMeta {
            pe_count: 200,
            text: Some(&older),
            text_at: 512,
            ignored: false,
            second: Some(&text),
            used_flag: true,
        },
    );
    assert_eq!(fs_usage(&img, 0, size).unwrap(), ours);
    let newer = vg_text(8, 200, &lvs[..1]);
    write_pv(
        &img,
        &PvMeta {
            pe_count: 200,
            text: Some(&newer),
            text_at: 512,
            ignored: false,
            second: Some(&text),
            used_flag: true,
        },
    );
    let only_ext = fs_usage(&img, 0, size).unwrap();
    assert!(bytes_of(&only_ext) < bytes_of(&ours));
    assert_eq!(
        util::overlap(&only_ext, PE_START + 48 * EXTENT, 76 * EXTENT),
        0,
        "the XFS LV is gone in the newer metadata"
    );

    // Copied in full: metadata that isn't here, can't be trusted or isn't understood.
    // Why, the metadata, and a bit to flip (at, mask) after writing it.
    type Case<'t> = (&'static str, PvMeta<'t>, Option<(u64, u8)>);
    let cases: Vec<Case> = vec![
        (
            "ignored metadata area",
            PvMeta { pe_count: 200, text: Some(&text), text_at: 512, ignored: true, second: None, used_flag: true },
            None,
        ),
        (
            "in a VG, metadata elsewhere",
            PvMeta { pe_count: 200, text: None, text_at: 512, ignored: false, second: None, used_flag: true },
            None,
        ),
        (
            "label checksum",
            PvMeta { pe_count: 200, text: Some(&text), text_at: 512, ignored: false, second: None, used_flag: true },
            Some((512 + 100, 1)),
        ),
        (
            "metadata header checksum",
            PvMeta { pe_count: 200, text: Some(&text), text_at: 512, ignored: false, second: None, used_flag: true },
            Some((4096 + 30, 1)),
        ),
        (
            "metadata checksum",
            PvMeta { pe_count: 200, text: Some(&text), text_at: 512, ignored: false, second: None, used_flag: true },
            Some((4096 + 512 + 40, 1)),
        ),
    ];
    for (why, meta, flip) in cases {
        write_pv(&img, &meta);
        if let Some((at, bit)) = flip {
            let mut b = read_range(&img, at, 1);
            b[0] ^= bit;
            put_bytes(&img, at, &b);
        }
        let layout = layout_of(&img);
        assert!(!layout.partitions[0].understood, "{why}");
        assert_eq!(layout.used(), size, "{why}");
    }
    let odd: Vec<(&str, String)> = vec![
        (
            "unknown segment type",
            vg_text(7, 200, &[hand_lv("x", VISIBLE, &[(0, 4, "type = \"frobnicated\"\n".into())])]),
        ),
        (
            "overlapping LVs",
            vg_text(
                7,
                200,
                &[
                    hand_lv("a", VISIBLE, &[(0, 4, linear("pv0", 0))]),
                    hand_lv("b", VISIBLE, &[(0, 4, linear("pv0", 2))]),
                ],
            ),
        ),
        ("past the last extent", vg_text(7, 200, &[hand_lv("a", VISIBLE, &[(0, 4, linear("pv0", 198))])])),
        ("stripe on no PV", vg_text(7, 200, &[hand_lv("a", VISIBLE, &[(0, 4, linear("pv7", 0))])])),
        ("this PV missing", vg_text(7, 200, &lvs).replace(&dashed(PV_UUID), &dashed(PV1_UUID))),
        ("segment count", vg_text(7, 200, &lvs).replace("segment_count = 2", "segment_count = 3")),
        ("broken text", vg_text(7, 200, &lvs).replacen('}', "", 3)),
    ];
    for (why, text) in &odd {
        write_pv(
            &img,
            &PvMeta { pe_count: 200, text: Some(text), text_at: 512, ignored: false, second: None, used_flag: true },
        );
        let layout = layout_of(&img);
        assert!(!layout.partitions[0].understood, "{why}");
        assert_eq!(layout.used(), size, "{why}");
    }
    // An orphan PV (in no volume group) holds nothing: only its label and metadata area.
    write_pv(&img, &PvMeta { pe_count: 200, text: None, text_at: 512, ignored: false, second: None, used_flag: false });
    let orphan = fs_usage(&img, 0, size).unwrap();
    assert_eq!(bytes_of(&orphan), PE_START);
}

#[test]
fn lvm_inside_lvm() {
    if !have(&["mke2fs", "e2fsck", "debugfs"]) {
        return;
    }
    let s = Scratch::new("lvm-nested");
    let mut rng = Rng(0x1F4);
    // A PV whose one LV holds a PV whose LV holds ext4, four levels deep.
    let (keep, gone) = trees(&mut rng, 30, MB);
    let inner = ext4_image(&s, "inner.img", 8 * EXTENT, &keep, &gone);
    let mut data = fs::read(&inner).unwrap();
    let mut levels = 0;
    while levels < 4 {
        let img = s.join("level.img");
        let pe_count = data.len() as u64 / EXTENT + 1;
        blank(&img, PE_START + pe_count * EXTENT + MB);
        let map = vec![(1, data.len() as u64 / EXTENT)];
        write_lv(&img, &map, &data);
        let text = vg_text(3, pe_count, &[hand_lv("lv", VISIBLE, &[(0, map[0].1, linear("pv0", 1))])]);
        write_pv(
            &img,
            &PvMeta { pe_count, text: Some(&text), text_at: 512, ignored: false, second: None, used_flag: true },
        );
        data = fs::read(&img).unwrap();
        // Pad to whole extents for the next level up.
        data.resize(data.len().next_multiple_of(EXTENT as usize), 0);
        levels += 1;
        let layout = analyze_bytes(&data, data.len() as u64).unwrap();
        check_invariants(&layout);
        assert!(layout.partitions[0].understood);
        let used = layout.used();
        eprintln!("{levels} levels: {used} of {} bytes", data.len());
        if levels <= 3 {
            // ext4 is still read: most of the innermost LV isn't copied.
            assert!(used < data.len() as u64 - 16 * MB, "{levels} levels");
        } else {
            // Past three levels, the innermost PV is copied whole.
            assert!(used > 8 * EXTENT, "{levels} levels");
        }
    }
}

// Whole drives.

/// One smart copy of a whole drive: each partition (start, size, check) must pass its check,
/// and fail it without its largest extent.
fn check_drive(s: &Scratch, disk: &Path, layout: &Layout, parts: &[(u64, u64, Verify)]) {
    let copy = s.join("drive-copy.img");
    let part = s.join("drive-part.img");
    smart_copy(disk, &layout.extents, &copy);
    for (i, &(start, size, verify)) in parts.iter().enumerate() {
        verify(&cut_out(&copy, start, size, &part)).unwrap_or_else(|e| panic!("partition {}: {e}", i + 1));
        let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= start + size);
        let Some(&big) = inside.max_by_key(|e| e.len) else {
            continue;
        };
        // Without it, then put back.
        let saved = read_range(&copy, big.start, big.len);
        put_bytes(&copy, big.start, &vec![0xA5; big.len as usize]);
        assert!(
            verify(&cut_out(&copy, start, size, &part)).is_err(),
            "partition {}: dropping {big:?} went unnoticed",
            i + 1
        );
        put_bytes(&copy, big.start, &saved);
    }
    let _ = fs::remove_file(&copy);
    let _ = fs::remove_file(&part);
}

/// A smaller hand-made PV (60 extents): an ext4 LV in two pieces and a btrfs one, 12 free
/// extents between them.
fn small_pv(s: &Scratch, rng: &mut Rng) -> (PathBuf, LvList) {
    let pe_count = 60;
    let img = s.join("small-pv.img");
    blank(&img, PE_START + pe_count * EXTENT + MB);
    let junk = rng.bytes(MB as usize);
    for i in 0..file_size(&img) / MB {
        put_bytes(&img, i * MB, &junk);
    }
    let (keep, gone) = trees(rng, 60, 2 * MB);
    let ext = ext4_image(s, "small-ext.img", 16 * EXTENT, &keep, &gone);
    let ext_map = vec![(0, 8), (20, 8)];
    write_lv(&img, &ext_map, &fs::read(&ext).unwrap());
    let (files, _) = trees(rng, 40, 3 * MB);
    let btr = btrfs_image(s, "small-btr.img", 32 * EXTENT, &[], &files);
    let btr_map = vec![(28, 32)];
    write_lv(&img, &btr_map, &fs::read(&btr).unwrap());
    fs::remove_file(ext).unwrap();
    fs::remove_file(btr).unwrap();
    let lvs = [
        hand_lv("ext", VISIBLE, &[(0, 8, linear("pv0", 0)), (8, 8, linear("pv0", 20))]),
        hand_lv("btr", VISIBLE, &[(0, 32, linear("pv0", 28))]),
    ];
    let text = vg_text(4, pe_count, &lvs);
    write_pv(
        &img,
        &PvMeta { pe_count, text: Some(&text), text_at: 512, ignored: false, second: None, used_flag: true },
    );
    (img, vec![("ext", ext_map, Content::Ext4(keep)), ("btr", btr_map, Content::Btrfs(files))])
}

#[test]
fn gpt_drive_with_lvm_btrfs_swap_f2fs_iso_udf() {
    let tools = ["sfdisk", "mke2fs", "e2fsck", "debugfs", "mkfs.btrfs", "btrfs", "mkswap", "swaplabel", "blkid"];
    if !have(&tools)
        || !have(&["mkfs.f2fs", "sload.f2fs", "fsck.f2fs", "dump.f2fs", "xorriso", "7z", "mkudffs", "udfinfo"])
    {
        return;
    }
    let s = Scratch::new("drive");
    let mut rng = Rng(0xD15C);
    // In MiB: LVM PV 1-243, btrfs 243-443, swap 443-459, F2FS 459-587, random bytes 587-599,
    // ISO 9660 599-631, UDF 631-663, random bytes up to the backup GPT (699-700).
    let (pv, lvs) = small_pv(&s, &mut rng);
    let (btr_files, _) = trees(&mut rng, 60, 3 * MB);
    let btr = btrfs_image(&s, "p2.img", 200 * MB, &["--compress", "zstd"], &btr_files);
    let swap = s.join("p3.img");
    blank(&swap, 16 * MB);
    sh("mkswap", &["-L", "drive-swap", p(&swap)]);
    let (f2fs_files, _) = trees(&mut rng, 50, 2 * MB);
    let f2 = f2fs_image(&s, "p4.img", 128 * MB, &[], &f2fs_files);
    let (iso_files, _) = trees(&mut rng, 30, MB);
    let src = fresh(&s, "iso-src");
    write_tree(&iso_files, &src);
    let iso = s.join("p5.img");
    sh("xorriso", &["-as", "mkisofs", "-quiet", "-V", "DRIVEISO", "-o", p(&iso), p(&src)]);
    let udf = s.join("p6.img");
    blank(&udf, 32 * MB);
    sh("mkudffs", &["--label=driveudf", p(&udf)]);
    let parts: [(u64, u64, &Path, &str); 6] = [
        (1, 242, &pv, "E6D6D379-F507-44C2-A23C-238F2A3DF928"),
        (243, 200, &btr, "L"),
        (443, 16, &swap, "S"),
        (459, 128, &f2, "L"),
        (599, 32, &iso, "L"),
        (631, 32, &udf, "L"),
    ];
    let disk = s.join("disk.img");
    blank(&disk, 700 * MB);
    let table: String = parts
        .iter()
        .map(|&(at, n, _, kind)| format!("start={}, size={}, type={kind}\n", at * 2048, n * 2048))
        .collect();
    sfdisk(&disk, &format!("label: gpt\n{table}"));
    let gap = (587 * MB, 12 * MB);
    let tail = (663 * MB, 36 * MB);
    for (at, len) in [gap, tail] {
        put_bytes(&disk, at, &rng.bytes(len as usize));
    }
    for &(at, _, img, _) in &parts {
        put(&disk, at * MB, img);
    }
    let layout = layout_of(&disk);
    eprintln!("drive: {} of {} bytes in {} extents", layout.used(), layout.size, layout.extents.len());
    for part in &layout.partitions {
        eprintln!("  {part:?}");
        assert!(part.understood, "{part:?}");
    }
    let names: Vec<_> = layout.partitions.iter().map(|p| p.fs.as_deref().unwrap()).collect();
    assert_eq!(names, ["LVM2_member", "btrfs", "swap", "f2fs", "iso9660", "udf"]);
    // Random bytes outside the partitions aren't copied.
    for (at, len) in [gap, tail] {
        assert_eq!(
            util::overlap(&layout.extents, at + util::GAP + util::ALIGN, len - 2 * (util::GAP + util::ALIGN)),
            0
        );
    }
    // The numbers, partition by partition.
    let at = |i: usize| (parts[i].0 * MB, parts[i].1 * MB);
    let (pv_at, pv_len) = at(0);
    let refs: Vec<(&str, LvMap, &Content)> = lvs.iter().map(|(n, m, c)| (*n, m.clone(), c)).collect();
    let pv_alone = cut_out(&disk, pv_at, pv_len, &s.join("pv-alone.img"));
    let expected: Vec<Extent> = lvm_expected(&pv_alone, 60, &refs, &s)
        .into_iter()
        .map(|e| Extent { start: pv_at + e.start, len: e.len })
        .collect();
    assert_eq!(fs_usage(&disk, pv_at, pv_len).unwrap(), expected);
    btrfs_numbers(&s, &disk, at(1).0, at(1).1);
    check_numbers("swap", &disk, at(2).0, at(2).1, HEAD_KEPT, 0);
    let reference = f2fs_numbers(&s, &disk, at(3).0, at(3).1, &f2fs_files);
    let iso_len = file_size(&iso);
    check_numbers("iso9660", &disk, at(4).0, at(4).1, iso_len.max(HEAD_KEPT), 0);
    let (block, start, blocks, used) = udf_numbers(&udf);
    check_numbers("udf", &disk, at(5).0, at(5).1, (start + used) * block + (32 * MB - (start + blocks) * block), 0);
    // One copy for all, checked partition by partition.
    let label = sh("swaplabel", &[p(&swap)]);
    let swap_ok = |copy: &Path| -> Result<(), String> {
        let (ok, text) = try_sh("swaplabel", &[p(copy)]);
        if ok && text == label { Ok(()) } else { Err(text) }
    };
    let iso_ok = |copy: &Path| -> Result<(), String> {
        let out = fresh(&s, "iso-files");
        let (ok, text) = try_sh("7z", &["x", "-y", &format!("-o{}", p(&out)), p(copy)]);
        if !ok {
            return Err(text);
        }
        compare(&iso_files, &out)
    };
    let empty = Tree::new();
    let checks: [(u64, u64, Verify); 6] = [
        (pv_at, pv_len, &|copy| lvm_ok(copy, &pv_alone, &refs, &s)),
        (at(1).0, at(1).1, &|copy| btrfs_ok(copy, &btr_files, &s)),
        (at(2).0, at(2).1, &swap_ok),
        (at(3).0, at(3).1, &|copy| f2fs_ok(copy, &reference, &s)),
        (at(4).0, at(4).1, &iso_ok),
        (at(5).0, at(5).1, &|copy| udf_ok(copy, Some(&empty), &s)),
    ];
    check_drive(&s, &disk, &layout, &checks);
}

// Recognised by name only.

/// The layout of an image with `writes` over random bytes (whose first 128 KiB are zeros):
/// `name`, copied in full.
fn named_in_full(name: &str, size: u64, writes: &[(u64, Vec<u8>)], rng: &mut Rng) {
    let mut data = rng.bytes(size as usize);
    data[..128 << 10].fill(0);
    for (at, bytes) in writes {
        data[*at as usize..*at as usize + bytes.len()].copy_from_slice(bytes);
    }
    let layout = analyze_bytes(&data, size).unwrap();
    check_invariants(&layout);
    assert_eq!(layout.partitions[0].fs.as_deref(), Some(name), "{:?}", layout.partitions[0]);
    assert!(!layout.partitions[0].understood);
    assert_eq!(layout.used(), size);
}

/// The configuration list of a ZFS vdev label (XDR), as far as the probe looks.
fn zfs_nvlist() -> Vec<u8> {
    let mut v = vec![1, 1, 0, 0];
    for word in [0u32, 1, 0x40, 0x38, 7] {
        v.extend(word.to_be_bytes());
    }
    v.extend(b"version\0");
    v
}

#[test]
fn recognised_only_names() {
    let size = 8 * MB;
    let mut rng = Rng(0xD0D);
    let be = |n: u32| n.to_be_bytes().to_vec();
    let le = |n: u32| n.to_le_bytes().to_vec();
    let bcache = b"\xc6\x85\x73\xf6\x4e\x1a\x45\xca\x82\x65\xf5\x7f\x48\xba\x6d\x81".to_vec();
    let bcachefs = b"\xc6\x85\x73\xf6\x66\xce\x90\xa9\xd9\x6a\x60\xcf\x80\x3d\xf7\xef".to_vec();
    // NILFS2: its superblock with the CRC it needs.
    let mut nilfs = vec![0u8; 1024];
    nilfs[0..4].copy_from_slice(&2u32.to_le_bytes());
    nilfs[6..8].copy_from_slice(&0x3434u16.to_le_bytes());
    nilfs[8..10].copy_from_slice(&1024u16.to_le_bytes());
    nilfs[12..16].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    let crc = util::crc32_le(util::crc32_le(util::crc32_le(0x1234_5678, &nilfs[..16]), &[0; 4]), &nilfs[20..]);
    nilfs[16..20].copy_from_slice(&crc.to_le_bytes());
    // Classic HFS: a master directory block with allocation blocks, embedding nothing.
    let mut hfs = vec![0u8; 162];
    hfs[..2].copy_from_slice(b"BD");
    hfs[0x12..0x14].copy_from_slice(&1000u16.to_be_bytes());
    hfs[0x14..0x18].copy_from_slice(&4096u32.to_be_bytes());
    // The name, and the bytes to write where.
    type Writes = Vec<(u64, Vec<u8>)>;
    let cases: Vec<(&str, Writes)> = vec![
        ("squashfs3", vec![(0, b"sqsh".to_vec())]),
        ("bcache", vec![(0x1018, bcache.clone()), (0x1008, 8u64.to_le_bytes().to_vec())]),
        ("bcachefs", vec![(0x1018, bcachefs)]),
        ("bcachefs", vec![(0x1018, bcache), (0x1068, 8u64.to_le_bytes().to_vec())]),
        ("zfs_member", vec![(16 << 10, zfs_nvlist())]),
        ("jfs", vec![(0x8000, b"JFS1".to_vec())]),
        ("reiserfs", vec![(0x10034, b"ReIsEr2Fs".to_vec())]),
        ("reiserfs", vec![(0x2034, b"ReIsErFs".to_vec())]),
        ("reiser4", vec![(0x10000, b"ReIsEr4".to_vec())]),
        ("nilfs2", vec![(1024, nilfs)]),
        ("erofs", vec![(1024, le(0xE0F5_E1E2))]),
        ("cramfs", vec![(512, be(0x28CD_3D45)), (528, b"Compressed ROMFS".to_vec())]),
        ("romfs", vec![(0, b"-rom1fs-".to_vec())]),
        ("ufs", vec![(8192 + 1372, le(0x1954_0119))]),
        ("ufs", vec![(65536 + 1372, be(0x0001_1954))]),
        ("vxfs", vec![(1024, le(0xA501_FCF5))]),
        ("gfs2", vec![(0x10000, be(0x0116_1970)), (0x10004, be(1)), (0x10018, be(1801)), (0x1001C, be(1900))]),
        ("gfs", vec![(0x10000, be(0x0116_1970)), (0x10004, be(1)), (0x10018, be(1309)), (0x1001C, be(1401))]),
        ("ocfs2", vec![(2048, b"OCFSV2".to_vec())]),
        ("oracleasm", vec![(32, b"ORCLDISK".to_vec())]),
        ("hfs", vec![(1024, hfs)]),
        ("hpfs", vec![(0x2000, le(0xF995_E849))]),
        ("ubifs", vec![(0, le(0x0610_1831))]),
        ("ubi", vec![(0, b"UBI#".to_vec())]),
        ("bfs", vec![(0, le(0x1BAD_FACE))]),
        ("zonefs", vec![(0, le(0x5A4E_4653))]),
        ("LVM1_member", vec![(0, b"HM\x02\x00".to_vec())]),
        ("DM_integrity", vec![(0, b"integrt".to_vec())]),
        ("DM_verity_hash", vec![(0, b"verity\0\0".to_vec())]),
        ("DM_snapshot_cow", vec![(0, b"SnAp".to_vec())]),
        ("vdo", vec![(0, b"dmvdo001".to_vec())]),
        ("stratis", vec![(0x1204, b"!Stra0tis\x86\xff\x02^\x41rh".to_vec())]),
        ("ceph_bluestore", vec![(0, b"bluestore block device".to_vec())]),
        // Past the first 128 KiB.
        ("VMFS_volume_member", vec![(MB, le(0xC001_D00D))]),
        ("VMFS", vec![(2 * MB, le(0x2FAB_F15E))]),
        ("zfs_member", vec![((256 << 10) + (16 << 10), zfs_nvlist())]),
        ("zfs_member", vec![(size - (256 << 10) + (16 << 10), zfs_nvlist())]),
    ];
    for (name, writes) in &cases {
        named_in_full(name, size, writes, &mut rng);
    }
    // Real ones, where the tools are here.
    let s = Scratch::new("names");
    let img = s.join("fs.img");
    let src = fresh(&s, "src");
    write_tree(&make_tree(&mut rng, "f", 10, 50_000), &src);
    let makers: [(&str, &[&str], &str); 6] = [
        ("mkfs.minix", &["-1"], "minix"),
        ("mkfs.minix", &["-2"], "minix"),
        ("mkfs.minix", &["-3"], "minix"),
        ("mkfs.bfs", &[], "bfs"),
        ("mkfs.cramfs", &["SRC"], "cramfs"),
        ("mkcomposefs", &["SRC"], "erofs"),
    ];
    for (tool, args, name) in makers {
        if !have(&[tool]) {
            continue;
        }
        let _ = fs::remove_file(&img);
        let mut all: Vec<&str> = args.iter().map(|a| if *a == "SRC" { p(&src) } else { a }).collect();
        if !args.contains(&"SRC") {
            blank(&img, size);
        }
        all.push(p(&img));
        sh(tool, &all);
        let len = file_size(&img).max(size);
        let mut data = fs::read(&img).unwrap();
        data.resize(len as usize, 0);
        let layout = analyze_bytes(&data, len).unwrap();
        assert_eq!(layout.partitions[0].fs.as_deref(), Some(name), "{tool} {args:?}");
        assert!(!layout.partitions[0].understood);
    }
    // An ext4 free inode count that looks like a Minix magic doesn't make it ambiguous; a ZFS
    // label left at the end does (copied in full).
    if have(&["mke2fs"]) {
        blank(&img, 64 * MB);
        sh("mke2fs", &["-q", "-F", "-t", "ext4", p(&img)]);
        let mut data = fs::read(&img).unwrap();
        data[1024 + 0x10..1024 + 0x12].copy_from_slice(&0x137Fu16.to_le_bytes());
        data[1024 + 0x14..1024 + 0x16].copy_from_slice(&0u16.to_le_bytes());
        let layout = analyze_bytes(&data, 64 * MB).unwrap();
        assert_eq!(layout.partitions[0].fs.as_deref(), Some("ext4"));
        let at = (64 * MB - (256 << 10) + (16 << 10)) as usize;
        let nv = zfs_nvlist();
        data[at..at + nv.len()].copy_from_slice(&nv);
        let layout = analyze_bytes(&data, 64 * MB).unwrap();
        assert_eq!(layout.partitions[0].fs.as_deref(), Some("zfs_member"));
        assert!(!layout.partitions[0].understood);
    }
}

// Robustness.

/// Random damage in what the analysis reads (mostly the starts of used extents, where
/// headers are), and truncation: no panics, no broken promises, never trusted when cut
/// short of `fs_end` (where the file system ends).
fn fuzz(name: &str, mut data: Vec<u8>, fs_end: u64, hot: &[Extent], rng: &mut Rng, rounds: usize) {
    let size = data.len() as u64;
    let clean = analyze_bytes(&data, size).unwrap();
    check_invariants(&clean);
    assert!(clean.partitions[0].understood, "{name}: {:?}", clean.partitions[0]);
    let targets = clean.extents.clone();
    for round in 0..rounds {
        let mut undo = Vec::new();
        for _ in 0..1 + rng.below(8) {
            // Half the time where the headers are known to be.
            let e = if !hot.is_empty() && rng.below(2) == 0 {
                hot[rng.below(hot.len() as u64) as usize]
            } else {
                targets[rng.below(targets.len() as u64) as usize]
            };
            let span = if rng.below(3) > 0 { e.len.min(8192) } else { e.len };
            let at = (e.start + rng.below(span)) as usize;
            let width = [1, 2, 4, 8][rng.below(4) as usize].min(data.len() - at);
            let value: [u8; 8] = match rng.below(4) {
                0 => [0; 8],
                1 => [0xFF; 8],
                _ => rng.next().to_le_bytes(),
            };
            undo.push((at, data[at..at + width].to_vec()));
            data[at..at + width].copy_from_slice(&value[..width]);
        }
        match analyze_bytes(&data, size) {
            Ok(layout) => check_invariants(&layout),
            Err(err) => panic!("{name} round {round}: {err}"),
        }
        for (at, old) in undo.into_iter().rev() {
            data[at..at + old.len()].copy_from_slice(&old);
        }
    }
    for cut in [512, 4096, 70_000, MB as usize + 1, 16 * MB as usize, fs_end as usize / 2] {
        let cut = cut.min(fs_end as usize - 1);
        // A drive bigger than what can be read (reads fail, unless the analysis needs none of
        // what's missing), and a file system bigger than its drive (never trusted).
        for size in [data.len() as u64, cut as u64] {
            if let Ok(layout) = analyze_bytes(&data[..cut], size) {
                check_invariants(&layout);
                if size == cut as u64 {
                    assert!(layout.partitions.iter().all(|p| !p.understood), "{name} cut at {cut}");
                }
            }
        }
    }
}

#[test]
fn corrupted_linux_metadata_never_panics() {
    let tools =
        ["mkfs.btrfs", "mkfs.xfs", "mkfs.f2fs", "sload.f2fs", "mkudffs", "xorriso", "mkswap", "mke2fs", "debugfs"];
    if !have(&tools) {
        return;
    }
    let s = Scratch::new("fuzz-linux");
    let mut rng = Rng(0xF1F2);
    let rounds = std::env::var("DD_GUI_SMART_FUZZ_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(300);
    let (files, _) = trees(&mut rng, 30, 256 << 10);
    let e = |start: u64, len: u64| Extent { start, len };
    for (name, args) in [("btrfs", &[][..]), ("btrfs xxhash", &["--csum", "xxhash", "-O", "^block-group-tree"][..])] {
        let btrfs = btrfs_image(&s, "btrfs.img", 128 * MB, args, &files);
        // The superblock, and the tree blocks: in the metadata and system chunks.
        let mut hot = vec![e(64 << 10, 4096)];
        let out = sh("btrfs", &["inspect-internal", "dump-tree", "-t", "chunk", p(&btrfs)]);
        let mut len = 0;
        for line in out.lines().map(str::trim) {
            if line.starts_with("length ") {
                len = if line.contains("DATA|") { 0 } else { number_after(line, "length").unwrap() };
            } else if line.starts_with("stripe ") && len > 0 {
                hot.push(e(number_after(line, "offset").unwrap(), len.min(MB)));
            }
        }
        fuzz(name, fs::read(&btrfs).unwrap(), 128 * MB, &hot, &mut rng, rounds);
        fs::remove_file(&btrfs).unwrap();
    }
    let xfs_variants: &[(&str, &[&str])] =
        if xfs_fills_from_dir(&s) { &[("xfs", &[]), ("xfs v4", &["-m", "crc=0"])] } else { &[] };
    for &(name, args) in xfs_variants {
        let xfs = xfs_image(&s, "xfs.img", 300 * MB, args, &files);
        // Each AG's first blocks (headers, tree roots), and the log's start.
        let (agblocks, block) = (xfs_field(&xfs, "agblocks"), xfs_field(&xfs, "blocksize"));
        let mut hot: Vec<Extent> =
            (0..xfs_field(&xfs, "agcount")).map(|ag| e(ag * agblocks * block, 16 * block)).collect();
        hot.push(e(xfs_log_start(&xfs), 8192));
        fuzz(name, fs::read(&xfs).unwrap(), 300 * MB, &hot, &mut rng, rounds);
        fs::remove_file(&xfs).unwrap();
    }
    let f2 = f2fs_image(&s, "f2fs.img", 64 * MB, &[], &files);
    // The superblocks, checkpoint packs and the SIT.
    let out = sh("dump.f2fs", &["-d", "1", p(&f2)]);
    let (cp, sit) = (f2fs_field(&out, "cp_blkaddr") * 4096, f2fs_field(&out, "sit_blkaddr") * 4096);
    let hot = [e(0, 8192), e(cp, 8 * 4096), e(cp + 2 * MB, 8 * 4096), e(sit, 4 * 4096), e(sit + 2 * MB, 4 * 4096)];
    fuzz("f2fs", fs::read(&f2).unwrap(), 64 * MB, &hot, &mut rng, rounds);
    fs::remove_file(&f2).unwrap();
    for args in [&["--blocksize=512"][..], &["--blocksize=2048", "--space=unalloctable"][..]] {
        let udf = s.join("udf.img");
        blank(&udf, 32 * MB);
        sh("mkudffs", &[args, &[p(&udf)]].concat());
        // Recognition sequence, descriptor sequences, integrity, anchor, the space bitmap.
        let block = if args[0].ends_with("512") { 512 } else { 2048 };
        let hot = [e(32 << 10, 8192), e(96 * block, 40 * block), e(256 * block, 8 * block)];
        fuzz("udf", fs::read(&udf).unwrap(), 32 * MB, &hot, &mut rng, rounds);
    }
    let src = fresh(&s, "iso-src");
    write_tree(&files, &src);
    let iso = s.join("iso.img");
    sh("xorriso", &["-as", "mkisofs", "-quiet", "-o", p(&iso), p(&src)]);
    let mut data = fs::read(&iso).unwrap();
    let volume = data.len() as u64;
    data.resize(data.len() + 4 * MB as usize, 0);
    fuzz("iso9660", data, volume, &[e(32 << 10, 8192)], &mut rng, rounds);
    let swap = s.join("swap.img");
    blank(&swap, 8 * MB);
    sh("mkswap", &[p(&swap)]);
    fuzz("swap", fs::read(&swap).unwrap(), 8 * MB, &[e(1024, 3072)], &mut rng, rounds);
    // A hand-made PV with an ext4 LV: the label, metadata area and text get the damage.
    let pv = s.join("pv.img");
    blank(&pv, PE_START + 10 * EXTENT + MB);
    let (keep, gone) = trees(&mut rng, 20, 256 << 10);
    let ext = ext4_image(&s, "ext.img", 8 * EXTENT, &keep, &gone);
    write_lv(&pv, &vec![(1, 8)], &fs::read(&ext).unwrap());
    let text = vg_text(3, 10, &[hand_lv("ext", VISIBLE, &[(0, 8, linear("pv0", 1))])]);
    write_pv(
        &pv,
        &PvMeta { pe_count: 10, text: Some(&text), text_at: 3072, ignored: false, second: None, used_flag: true },
    );
    fuzz("lvm", fs::read(&pv).unwrap(), PE_START + 10 * EXTENT, &[e(512, 512), e(4096, 8192)], &mut rng, rounds);
}

// With root: mounted file systems, real volume groups. Ignored by default (see the top).

/// A loop device of our own over `img` (with its partitions), detached on drop. Never
/// /dev/loop0 (a GUI test drive on the dev machine).
struct Loop {
    dev: String,
}

impl Loop {
    fn attach(img: &Path) -> Option<Loop> {
        let (ok, out) = sudo(&["losetup", "-f", "--show", "-P", p(img)]);
        let dev = out.lines().find(|l| l.starts_with("/dev/loop"))?.trim().to_owned();
        if !ok {
            eprintln!("losetup: {out}");
            return None;
        }
        if dev == "/dev/loop0" {
            // Take another one while holding it, then let it go.
            let other = Loop::attach(img);
            sudo(&["losetup", "-d", "/dev/loop0"]);
            return other;
        }
        Some(Loop { dev })
    }

    fn part(&self, n: u32) -> String {
        format!("{}p{n}", self.dev)
    }
}

/// Makes btrfs forget `dev` (unmounted by now): a copy of a file system it saw has the same
/// UUID. Only our own devices, never all of them.
fn forget(dev: &str) {
    if on_path("btrfs") && Path::new(dev).exists() {
        let _ = sudo(&["btrfs", "device", "scan", "--forget", dev]);
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        for n in 1..=8 {
            forget(&self.part(n));
        }
        forget(&self.dev);
        for _ in 0..40 {
            if sudo(&["losetup", "-d", &self.dev]).0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        eprintln!("losetup -d {} failed", self.dev);
    }
}

/// A file system mounted by root, unmounted on drop.
struct Mounted {
    dir: PathBuf,
}

impl Mounted {
    fn at(dev: &str, dir: &Path, kind: &str, opts: &str) -> Option<Mounted> {
        fs::create_dir_all(dir).unwrap();
        let (ok, out) = sudo(&["mount", "-t", kind, "-o", opts, dev, p(dir)]);
        if !ok {
            eprintln!("mount {dev}: {out}");
            return None;
        }
        Some(Mounted { dir: dir.to_owned() })
    }

    /// Makes `path` in it the current user's.
    fn own(&self, path: &Path) {
        let ids = format!("{}:{}", sh("id", &["-u"]).trim(), sh("id", &["-g"]).trim());
        sh_sudo(&["chown", &ids, p(path)]);
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        for _ in 0..40 {
            if sudo(&["umount", p(&self.dir)]).0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        eprintln!("umount {} failed", self.dir.display());
    }
}

/// LVM without backups or archives in /etc/lvm.
const LVM_CONFIG: &str = "backup/backup=0 backup/archive=0";

/// A volume group of ours, deactivated on drop.
struct Vg(String);

impl Drop for Vg {
    fn drop(&mut self) {
        let (ok, out) = sudo(&["vgchange", "--config", LVM_CONFIG, "-an", &self.0]);
        if !ok {
            eprintln!("vgchange -an {}: {out}", self.0);
        }
    }
}

fn sh_sudo(args: &[&str]) -> String {
    let (ok, out) = sudo(args);
    assert!(ok, "sudo {args:?}: {out}");
    out
}

fn lvm(args: &[&str]) -> String {
    let mut all = vec![args[0], "--config", LVM_CONFIG];
    all.extend(&args[1..]);
    sh_sudo(&all)
}

/// Whether the root tests can run here: sudo, and the tools.
fn root_ready(tools: &[&str]) -> bool {
    can_sudo() && have(tools) && sudo(&["true"]).0
}

/// SHA-256 of every file under `dir`, sorted by path (lost+found left out).
fn sha256s(dir: &Path) -> Result<String, String> {
    let script = "find . -path ./lost+found -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum";
    let out = Command::new("sh").args(["-c", script]).current_dir(dir).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The SHA-256 listing of `files`, as they should be.
fn expected_sha256s(files: &Tree, s: &Scratch) -> String {
    let dir = fresh(s, "expected");
    write_tree(files, &dir);
    let listing = sha256s(&dir).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    listing
}

/// Mounts `dev` read-only and compares the SHA-256 of every file with `listing`.
fn mounted_sha256s(dev: &str, kind: &str, opts: &str, listing: &str, s: &Scratch) -> Result<(), String> {
    let dir = fresh(s, "mnt-check");
    let mounted = Mounted::at(dev, &dir, kind, opts).ok_or("won't mount")?;
    let found = sha256s(&mounted.dir)?;
    if found == listing {
        Ok(())
    } else {
        let diff = found.lines().zip(listing.lines()).find(|(a, b)| a != b);
        Err(format!("files differ, first: {diff:?} ({} vs {} files)", found.lines().count(), listing.lines().count()))
    }
}

/// Mounts an image file (through a loop device) read-only and compares its files' SHA-256.
fn image_sha256s(img: &Path, kind: &str, opts: &str, listing: &str, s: &Scratch) -> Result<(), String> {
    let lo = Loop::attach(img).ok_or("no loop device")?;
    mounted_sha256s(&lo.dev, kind, opts, listing, s)
}

/// Writes `files` into a mounted directory, and `gone`, which is deleted afterwards; with
/// two fragmented files grown in turns (fsync'd, so they interleave). Returns what's left.
fn fill_mounted(dir: &Path, rng: &mut Rng, count: usize, max: u64) -> Tree {
    let (mut keep, gone) = trees(rng, count, max);
    write_tree(&keep, dir);
    write_tree(&gone, dir);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..40 {
        for (name, data, n) in [("frag-a.bin", &mut a, 40_000), ("frag-b.bin", &mut b, 50_000)] {
            let chunk = rng.bytes(n);
            let mut f = fs::OpenOptions::new().create(true).append(true).open(dir.join(name)).unwrap();
            f.write_all(&chunk).unwrap();
            f.sync_all().unwrap();
            data.extend(chunk);
        }
    }
    keep.insert("frag-a.bin".into(), a);
    keep.insert("frag-b.bin".into(), b);
    for k in gone.keys() {
        fs::remove_file(dir.join(k)).unwrap();
    }
    keep
}

#[test]
#[ignore]
fn linux_root_btrfs_snapshots_compression_reflinks() {
    if !root_ready(&["mkfs.btrfs", "btrfs", "cp"]) {
        return;
    }
    let s = Scratch::new("root-btrfs");
    let mut rng = Rng(0xB7F6);
    let img = s.join("btrfs.img");
    blank(&img, 512 * MB);
    sh("mkfs.btrfs", &["-q", "-L", "smartbtrfs", p(&img)]);
    let mut files;
    {
        let lo = Loop::attach(&img).unwrap();
        let m = Mounted::at(&lo.dev, &s.join("mnt"), "btrfs", "compress=zstd").unwrap();
        m.own(&m.dir);
        files = fill_mounted(&m.dir, &mut rng, 150, 3 * MB);
        for (name, data) in compressible(&mut rng, "text", 40) {
            fs::write(m.dir.join(&name), &data).unwrap();
            files.insert(name, data);
        }
        // A subvolume with files, a snapshot of it, then changes that only the subvolume sees.
        sh_sudo(&["btrfs", "subvolume", "create", p(&m.dir.join("sub"))]);
        m.own(&m.dir.join("sub"));
        let sub = make_tree(&mut rng, "sub", 40, MB);
        write_tree(&sub, &m.dir.join("sub"));
        sh("sync", &[]);
        sh_sudo(&["btrfs", "subvolume", "snapshot", p(&m.dir.join("sub")), p(&m.dir.join("snap"))]);
        for (i, (name, data)) in sub.iter().enumerate() {
            files.insert(format!("snap/{name}"), data.clone());
            if i % 3 == 0 {
                fs::remove_file(m.dir.join("sub").join(name)).unwrap();
            } else {
                files.insert(format!("sub/{name}"), data.clone());
            }
        }
        // Reflinked copies, one of them changed afterwards.
        let big: Vec<String> = files
            .iter()
            .filter(|(k, v)| v.len() > 200_000 && !k.contains('/'))
            .map(|(k, _)| k.clone())
            .take(5)
            .collect();
        for (i, name) in big.iter().enumerate() {
            let copy = format!("reflink{i}.bin");
            sh("cp", &["--reflink=always", p(&m.dir.join(name)), p(&m.dir.join(&copy))]);
            let mut data = files[name].clone();
            if i == 0 {
                data[1000..5000].fill(7);
                fs::write(m.dir.join(&copy), &data).unwrap();
            }
            files.insert(copy, data);
        }
    }
    let layout = layout_of(&img);
    assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
    btrfs_numbers(&s, &img, 0, 512 * MB);
    let listing = expected_sha256s(&files, &s);
    check_copy(&s, &img, 0, 512 * MB, &layout, None, &|copy| {
        btrfs_checked(copy)?;
        image_sha256s(copy, "btrfs", "ro", &listing, &s)
    });
}

#[test]
#[ignore]
fn linux_root_xfs_reflinks_and_a_dirty_log() {
    if !root_ready(&["mkfs.xfs", "xfs_db", "xfs_repair", "xfs_io", "cp"]) {
        return;
    }
    let s = Scratch::new("root-xfs");
    let mut rng = Rng(0xF7);
    let size = 400 * MB;
    let img = s.join("xfs.img");
    blank(&img, size);
    sh("mkfs.xfs", &["-q", "-L", "smartxfs", "-m", "reflink=1", p(&img)]);
    let mut files;
    {
        let lo = Loop::attach(&img).unwrap();
        let m = Mounted::at(&lo.dev, &s.join("mnt"), "xfs", "rw").unwrap();
        m.own(&m.dir);
        files = fill_mounted(&m.dir, &mut rng, 200, 3 * MB);
        let big: Vec<String> =
            files.iter().filter(|(_, v)| v.len() > 200_000).map(|(k, _)| k.clone()).take(6).collect();
        for (i, name) in big.iter().enumerate() {
            let copy = format!("reflink{i}.bin");
            sh("cp", &["--reflink=always", p(&m.dir.join(name)), p(&m.dir.join(&copy))]);
            let mut data = files[name].clone();
            if i % 2 == 0 {
                // Copy on write: this one gets blocks of its own.
                data[100..70_000].fill(9);
                fs::write(m.dir.join(&copy), &data).unwrap();
            }
            files.insert(copy, data);
        }
    }
    let layout = layout_of(&img);
    assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
    let listing = expected_sha256s(&files, &s);
    xfs_numbers(&s, &img, 0, size);
    check_copy(&s, &img, 0, size, &layout, None, &|copy| {
        xfs_ok(copy, &files)?;
        image_sha256s(copy, "xfs", "ro,norecovery", &listing, &s)
    });
    // Shut down while changes are only in the log: it needs replaying, copied in full.
    {
        let lo = Loop::attach(&img).unwrap();
        let m = Mounted::at(&lo.dev, &s.join("mnt"), "xfs", "rw").unwrap();
        fs::write(m.dir.join("late.bin"), rng.bytes(3 * MB as usize)).unwrap();
        sh("sync", &[]);
        fs::write(m.dir.join("later.bin"), rng.bytes(MB as usize)).unwrap();
        sh_sudo(&["xfs_io", "-x", "-c", "shutdown -f", p(&m.dir)]);
    }
    let (_, repair) = try_sh("xfs_repair", &["-n", p(&img)]);
    assert!(repair.contains("ALERT") || repair.contains("log"), "{repair}");
    let layout = layout_of(&img);
    assert!(!layout.partitions[0].understood);
    assert_eq!(layout.used(), size);
    assert!(estimate_of(&img).partitions[0].understood);
}

#[test]
#[ignore]
fn linux_root_f2fs_after_many_writes() {
    if !root_ready(&["mkfs.f2fs", "fsck.f2fs", "dump.f2fs", "f2fs_io"]) {
        return;
    }
    let s = Scratch::new("root-f2fs");
    let mut rng = Rng(0xF2F7);
    let size = 256 * MB;
    let img = s.join("f2fs.img");
    blank(&img, size);
    sh("mkfs.f2fs", &["-q", "-l", "smartf2fs", p(&img)]);
    let mut files = Tree::new();
    {
        let lo = Loop::attach(&img).unwrap();
        let m = Mounted::at(&lo.dev, &s.join("mnt"), "f2fs", "rw").unwrap();
        m.own(&m.dir);
        // Rounds of writes, rewrites and deletions: segments fill, get cleaned, get reused.
        for round in 0..25 {
            let batch = make_tree(&mut rng, &format!("r{round}-"), 40, 1500 * 1024);
            write_tree(&batch, &m.dir);
            files.extend(batch);
            let names: Vec<String> = files.keys().cloned().collect();
            for (i, name) in names.iter().enumerate() {
                match (i + round) % 7 {
                    0 => {
                        fs::remove_file(m.dir.join(name)).unwrap();
                        files.remove(name);
                    }
                    1 => {
                        let n = rng.below(200_000) as usize;
                        let data = rng.bytes(n);
                        fs::write(m.dir.join(name), &data).unwrap();
                        files.insert(name.clone(), data);
                    }
                    _ => {}
                }
            }
            sh("sync", &[]);
        }
        files.extend(fill_mounted(&m.dir, &mut rng, 30, MB));
    }
    let layout = layout_of(&img);
    assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
    let reference = f2fs_numbers(&s, &img, 0, size, &files);
    let listing = expected_sha256s(&files, &s);
    check_copy(&s, &img, 0, size, &layout, None, &|copy| {
        f2fs_ok(copy, &reference, &s)?;
        image_sha256s(copy, "f2fs", "ro", &listing, &s)
    });
    // Stopped without a checkpoint after a sync's: not cleanly unmounted.
    {
        let lo = Loop::attach(&img).unwrap();
        let m = Mounted::at(&lo.dev, &s.join("mnt"), "f2fs", "rw").unwrap();
        fs::write(m.dir.join("late.bin"), rng.bytes(2 * MB as usize)).unwrap();
        sh("sync", &[]);
        sh_sudo(&["f2fs_io", "shutdown", "2", p(&m.dir)]);
    }
    let layout = layout_of(&img);
    assert!(!layout.partitions[0].understood);
    assert_eq!(layout.used(), size);
}

#[test]
#[ignore]
fn linux_root_udf_with_files() {
    if !root_ready(&["mkudffs", "udfinfo", "7z"]) {
        return;
    }
    let s = Scratch::new("root-udf");
    let mut rng = Rng(0x0DF);
    let size = 128 * MB;
    for block in ["512", "2048"] {
        let img = s.join(&format!("udf-{block}.img"));
        blank(&img, size);
        sh("mkudffs", &["--label=smartudf", &format!("--blocksize={block}"), p(&img)]);
        let files;
        {
            let lo = Loop::attach(&img).unwrap();
            let ids = format!("uid={},gid={}", sh("id", &["-u"]).trim(), sh("id", &["-g"]).trim());
            let m = Mounted::at(&lo.dev, &s.join("mnt"), "udf", &format!("rw,{ids}")).unwrap();
            files = fill_mounted(&m.dir, &mut rng, 100, 2 * MB);
        }
        let layout = layout_of(&img);
        assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
        let (bs, start, blocks, used) = udf_numbers(&img);
        check_numbers("udf", &img, 0, size, (start + used) * bs + (size - (start + blocks) * bs), 0);
        let listing = expected_sha256s(&files, &s);
        // 7z can't read files in many extents: the kernel reads them all.
        check_copy(&s, &img, 0, size, &layout, None, &|copy| {
            udf_ok(copy, None, &s)?;
            image_sha256s(copy, "udf", "ro", &listing, &s)
        });
    }
}

#[test]
#[ignore]
fn linux_root_lvm_on_a_gpt_drive() {
    let tools = ["sfdisk", "pvcreate", "vgcreate", "lvcreate", "vgchange", "vgremove", "mke2fs", "e2fsck", "mkfs.xfs"];
    if !root_ready(&tools) || !have(&["xfs_repair", "mkfs.btrfs", "btrfs", "mkswap", "swaplabel"]) {
        return;
    }
    let s = Scratch::new("root-lvm");
    let mut rng = Rng(0x17A);
    let vg = format!("ddguitest{}", std::process::id());
    // In MiB: the PV 1-1025, btrfs 1025-1281, swap 1281-1313; the backup GPT at the end.
    let disk = s.join("disk.img");
    blank(&disk, 1314 * MB);
    sfdisk(
        &disk,
        "label: gpt\nstart=2048, size=2097152, type=E6D6D379-F507-44C2-A23C-238F2A3DF928\nstart=2099200, size=524288, type=L\nstart=2623488, size=65536, type=S\n",
    );
    // Garbage first: free extents and free space inside the LVs keep it.
    let junk = rng.bytes(MB as usize);
    for i in 1..1025 {
        put_bytes(&disk, i * MB, &junk);
    }
    let (btr_files, _) = trees(&mut rng, 60, 3 * MB);
    let btr = btrfs_image(&s, "p2.img", 256 * MB, &[], &btr_files);
    put(&disk, 1025 * MB, &btr);
    fs::remove_file(&btr).unwrap();
    let swap = s.join("p3.img");
    blank(&swap, 32 * MB);
    sh("mkswap", &["-L", "lvm-drive", p(&swap)]);
    put(&disk, 1281 * MB, &swap);
    // The volume group: ext4, XFS and btrfs LVs, a thin pool with an ext4 thin LV, and
    // unallocated extents.
    let mut contents: Vec<(&str, &str, Tree)> = Vec::new();
    {
        let lo = Loop::attach(&disk).unwrap();
        let pv = lo.part(1);
        lvm(&["pvcreate", "-q", "-ff", "-y", &pv]);
        lvm(&["vgcreate", "-q", "--setautoactivation", "n", &vg, &pv]);
        let _vg = Vg(vg.clone());
        for (name, size) in [("ext", "96M"), ("xfs", "320M"), ("btr", "192M")] {
            lvm(&["lvcreate", "-q", "-Zn", "-Wn", "-L", size, "-n", name, &vg]);
        }
        lvm(&["lvcreate", "-q", "-Zn", "--type", "thin-pool", "-L", "64M", "-n", "pool", &vg]);
        lvm(&["lvcreate", "-q", "-Wn", "-V", "160M", "-T", &format!("{vg}/pool"), "-n", "thin"]);
        for (name, kind, mkfs) in [
            ("ext", "ext4", vec!["mke2fs", "-q", "-F", "-t", "ext4"]),
            ("xfs", "xfs", vec!["mkfs.xfs", "-q", "-f"]),
            ("btr", "btrfs", vec!["mkfs.btrfs", "-q", "-f"]),
            ("thin", "ext4", vec!["mke2fs", "-q", "-F", "-t", "ext4"]),
        ] {
            let dev = format!("/dev/{vg}/{name}");
            let mut args = mkfs.clone();
            args.push(&dev);
            sh_sudo(&args);
            let m = Mounted::at(&dev, &s.join(&format!("mnt-{name}")), kind, "rw").unwrap();
            m.own(&m.dir);
            let files = fill_mounted(&m.dir, &mut rng, 60, 2 * MB);
            drop(m);
            forget(&dev);
            contents.push((name, kind, files));
        }
    }
    let layout = layout_of(&disk);
    for part in &layout.partitions {
        eprintln!("  {part:?}");
        assert!(part.understood, "{part:?}");
    }
    let names: Vec<_> = layout.partitions.iter().map(|p| p.fs.as_deref().unwrap()).collect();
    assert_eq!(names, ["LVM2_member", "btrfs", "swap"]);
    assert_eq!(layout.partitions[0].label.as_deref(), Some(vg.as_str()));
    // Unallocated extents (per LVM) aren't copied; the thin pool's are.
    let segments = {
        let lo = Loop::attach(&disk).unwrap();
        let _vg = Vg(vg.clone());
        lvm(&[
            "pvs",
            "--noheadings",
            "--units",
            "b",
            "--segments",
            "-o",
            "pvseg_start,pvseg_size,lv_name,vg_extent_size",
            &lo.part(1),
        ])
    };
    let (pv_start, pv_size) = (MB, 1024 * MB);
    let pe_start = 1 << 20;
    for line in segments.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let (first, count): (u64, u64) = (w[0].parse().unwrap(), w[1].parse().unwrap());
        let extent: u64 = w.last().unwrap().trim_end_matches('B').parse().unwrap();
        let (at, len) = (pv_start + pe_start + first * extent, count * extent);
        let copied = util::overlap(&layout.extents, at, len);
        match w.len() {
            // No LV: free.
            3 => assert!(copied <= 2 * (util::GAP + util::ALIGN), "free extents {first}+{count} copied: {copied}"),
            _ if w[2].starts_with("[pool") || w[2].starts_with("[lvol") => {
                assert_eq!(copied, len, "{line}")
            }
            _ => {}
        }
    }
    let used_pv = util::overlap(&layout.extents, pv_start, pv_size);
    eprintln!("PV: {used_pv} of {pv_size} bytes copied");
    // One copy, checked: every LV (fsck, mounted, SHA-256), the btrfs partition, the swap.
    let listings: Vec<String> = contents.iter().map(|(_, _, files)| expected_sha256s(files, &s)).collect();
    let btr_listing = expected_sha256s(&btr_files, &s);
    let label = sh("swaplabel", &[p(&swap)]);
    let verify = |copy: &Path| -> Result<(), String> {
        let lo = Loop::attach(copy).ok_or("no loop device")?;
        let (ok, out) = sudo(&["vgchange", "--config", LVM_CONFIG, "-ay", &vg]);
        let _vg = Vg(vg.clone());
        if !ok {
            return Err(format!("vgchange: {out}"));
        }
        for ((name, kind, _), listing) in contents.iter().zip(&listings) {
            let dev = format!("/dev/{vg}/{name}");
            let (ok, out) = match *kind {
                "ext4" => sudo(&["e2fsck", "-fn", &dev]),
                "xfs" => sudo(&["xfs_repair", "-n", &dev]),
                _ => sudo(&["btrfs", "check", "--readonly", "--check-data-csum", &dev]),
            };
            if !ok || out.contains("ALERT") {
                return Err(format!("LV {name}: {out}"));
            }
            let opts = match *kind {
                "ext4" => "ro,noload",
                "xfs" => "ro,norecovery",
                _ => "ro",
            };
            let checked = mounted_sha256s(&dev, kind, opts, listing, &s);
            forget(&dev);
            checked.map_err(|e| format!("LV {name}: {e}"))?;
        }
        let (ok, out) = sudo(&["btrfs", "check", "--readonly", "--check-data-csum", &lo.part(2)]);
        if !ok {
            return Err(format!("btrfs partition: {out}"));
        }
        mounted_sha256s(&lo.part(2), "btrfs", "ro", &btr_listing, &s).map_err(|e| format!("btrfs partition: {e}"))?;
        let (ok, text) = sudo(&["swaplabel", &lo.part(3)]);
        if !ok || text != label {
            return Err(format!("swap: {text}"));
        }
        Ok(())
    };
    // The whole drive copied; without the PV's largest extent, something must break.
    let inside = layout.extents.iter().filter(|e| e.start >= pv_start && e.start + e.len <= pv_start + pv_size);
    let biggest = inside.max_by_key(|e| e.len).copied();
    check_copy(&s, &disk, 0, file_size(&disk), &layout, biggest, &verify);
    // Gone for good.
    let lo = Loop::attach(&disk).unwrap();
    lvm(&["vgremove", "-q", "-f", &vg]);
    drop(lo);
}

/// Analysis speed on 2 TB (sparse) images, and big-file-system paths small images don't take
/// (F2FS's SIT bitmap in checkpoint payload blocks):
/// `cargo test smart -- --ignored linux_two_terabytes --nocapture`.
#[test]
#[ignore]
fn linux_two_terabytes() {
    let s = Scratch::new("2tb-linux");
    let img = s.join("big.img");
    let cases: &[(&str, &[&str])] = &[
        ("mkfs.btrfs", &["-q", "-f"]),
        ("mkfs.xfs", &["-q", "-f"]),
        ("mkfs.f2fs", &["-q", "-f"]),
        ("mkudffs", &["--blocksize=4096"]),
    ];
    for &(mkfs, args) in cases {
        if !have(&[mkfs]) {
            continue;
        }
        blank(&img, 2_000_000_000_000);
        let mut all = args.to_vec();
        all.push(p(&img));
        let t = std::time::Instant::now();
        sh(mkfs, &all);
        let made = t.elapsed();
        let t = std::time::Instant::now();
        let layout = layout_of(&img);
        eprintln!(
            "{mkfs}: made in {made:?}, analyzed in {:?}: {} bytes in {} extents, {:?}",
            t.elapsed(),
            layout.used(),
            layout.extents.len(),
            layout.partitions[0]
        );
        assert!(layout.partitions[0].understood);
        if mkfs == "mkfs.f2fs" {
            let out = sh("dump.f2fs", &["-d", "1", p(&img)]);
            eprintln!("  f2fs cp_payload {}", f2fs_field(&out, "cp_payload"));
        }
        fs::remove_file(&img).unwrap();
    }
}
