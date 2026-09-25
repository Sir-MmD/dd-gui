//! Apple file systems (HFS+, HFSX, APFS) and partition schemes (Apple partition maps, and
//! BSD disklabels while at it).
//!
//! On Linux, HFS+ images come from hfsprogs (`mkfs.hfsplus`, `fsck.hfsplus`) and APFS
//! containers from apfsprogs (`mkapfs`, `apfsck`); tests whose tools are missing say so and
//! pass. Files get into HFS+ images through the kernel's hfsplus driver, which needs root
//! for a loop mount, so those tests are ignored by default:
//! `DD_GUI_TEST_SUDO_PASSWORD=… cargo test smart -- --ignored hfsplus_with_files`.
//! Nothing on Linux writes files into APFS without an out-of-tree kernel module, so APFS
//! containers are tested empty, and with bitmaps edited by hand.
//!
//! On macOS, real images from `hdiutil` (APFS, journaled HFS+, Apple partition maps) get
//! files copied in and some deleted, then their smart copies (0xA5 garbage everywhere
//! else) must pass `fsck_apfs` / `fsck_hfs` and give back every file byte for byte.
//!
//! Apple partition maps and BSD disklabels are also written by hand here, on every OS.

use super::tests::{Rng, analyze_bytes, check_invariants, mbr_sector};
use super::*;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

fn be16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_be_bytes(b[at..at + 2].try_into().unwrap()))
}

fn be32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_be_bytes(b[at..at + 4].try_into().unwrap()))
}

fn put_be16(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 2].copy_from_slice(&(v as u16).to_be_bytes());
}

fn put_be32(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 4].copy_from_slice(&(v as u32).to_be_bytes());
}

fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn put_le32(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
}

fn put_le64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Bytes of `layout`'s extents in `start..start + len`.
fn copied(layout: &Layout, start: u64, len: u64) -> u64 {
    util::overlap(&layout.extents, start, len)
}

fn understood(d: &[u8]) -> bool {
    let layout = analyze_bytes(d, d.len() as u64).unwrap();
    check_invariants(&layout);
    layout.partitions.iter().all(|p| p.understood)
}

fn estimated(d: &[u8]) -> bool {
    let layout = estimate(&mut std::io::Cursor::new(d), d.len() as u64).unwrap();
    check_invariants(&layout);
    layout.partitions.iter().all(|p| p.understood)
}

/// APFS objects: the Fletcher-64 checksum of everything after the checksum field, stored
/// in it.
fn seal(o: &mut [u8]) {
    const M: u64 = 0xFFFF_FFFF;
    let (mut a, mut b) = (0, 0);
    for w in o[8..].as_chunks::<4>().0 {
        a = (a + u64::from(u32::from_le_bytes(*w))) % M;
        b = (b + a) % M;
    }
    let c1 = M - (a + b) % M;
    let c2 = M - (a + c1) % M;
    o[..8].copy_from_slice(&(c2 << 32 | c1).to_le_bytes());
}

/// An Apple partition map in `bs`-byte blocks over `d`: (name, type, first block, blocks)
/// per entry, from block 1 on.
fn apple_map(mut d: Vec<u8>, bs: u64, entries: &[(&str, &str, u64, u64)]) -> Vec<u8> {
    let bs = bs as usize;
    d[..bs].fill(0);
    d[..2].copy_from_slice(b"ER");
    put_be16(&mut d, 2, bs as u64);
    let blocks = (d.len() / bs) as u64;
    put_be32(&mut d, 4, blocks);
    for (i, &(name, kind, first, n)) in entries.iter().enumerate() {
        let e = (i + 1) * bs;
        d[e..e + bs].fill(0);
        d[e..e + 2].copy_from_slice(b"PM");
        put_be32(&mut d, e + 4, entries.len() as u64);
        put_be32(&mut d, e + 8, first);
        put_be32(&mut d, e + 12, n);
        d[e + 16..e + 16 + name.len()].copy_from_slice(name.as_bytes());
        d[e + 48..e + 48 + kind.len()].copy_from_slice(kind.as_bytes());
    }
    d
}

/// (index, start, size, understood) of each partition.
fn listed(layout: &Layout) -> Vec<(u32, u64, u64, bool)> {
    layout.partitions.iter().map(|p| (p.index, p.start, p.size, p.understood)).collect()
}

#[test]
fn apple_partition_maps() {
    let size = 64 * MIB;
    let noise = Rng(0xA93).bytes(size as usize);
    // A Mac disk of old: the map, drivers, an HFS volume (random bytes here), free space,
    // another partition. In 512-byte blocks.
    let entries = [
        ("Apple", "Apple_partition_map", 1, 63),
        ("Macintosh", "Apple_Driver43", 64, 56),
        ("Macintosh", "Apple_Driver_ATA", 120, 56),
        ("Patch Partition", "Apple_Patches", 176, 512),
        ("Untitled", "Apple_HFS", 2048, 40960),
        ("Extra", "Apple_Free", 43008, 40960),
        ("Linux", "Apple_UNIX_SVR2", 83968, 20480),
        ("Extra", "apple_free", 104448, size / 512 - 104448),
    ];
    let d = apple_map(noise.clone(), 512, &entries);
    let layout = analyze_bytes(&d, size).unwrap();
    check_invariants(&layout);
    assert_eq!(layout.table, partitions::APM);
    assert_eq!(listed(&layout), [(5, 2048 * 512, 40960 * 512, false), (7, 83968 * 512, 20480 * 512, false)]);
    // The drivers are kept; free space isn't, but for the drive's last MiB.
    assert_eq!(copied(&layout, 0, 688 * 512), 688 * 512);
    assert_eq!(copied(&layout, 43008 * 512, 40960 * 512), 0);
    assert_eq!(copied(&layout, 104448 * 512, size - MIB - 104448 * 512), 0);
    assert_eq!(layout.used(), (40960 + 20480) * 512 + 2 * MIB);

    // CDs: 2048-byte blocks.
    let cd =
        [("Apple", "Apple_partition_map", 1, 3), ("Disc", "Apple_HFS", 16, 8000), ("Free", "Apple_Free", 8016, 4000)];
    let layout = analyze_bytes(&apple_map(noise.clone(), 2048, &cd), size).unwrap();
    assert_eq!(listed(&layout), [(2, 16 * 2048, 8000 * 2048, false)]);
    assert_eq!(copied(&layout, 8016 * 2048, 4000 * 2048), 0);

    // The map ends at the first block without an entry, whatever the count says; entries
    // past the end of the drive are dropped, and one across it is cut.
    let mut d = apple_map(noise.clone(), 512, &entries[..5]);
    put_be32(&mut d, 512 + 4, 9);
    let mut far =
        apple_map(noise.clone(), 512, &[("A", "Apple_HFS", size / 512, 10), ("B", "Apple_HFS", size / 512 - 8, 16)]);
    far.truncate(size as usize);
    let layout = analyze_bytes(&d, size).unwrap();
    assert_eq!(layout.partitions.len(), 1);
    let layout = analyze_bytes(&far, size).unwrap();
    assert_eq!(listed(&layout), [(2, size - 4096, 4096, false)]);

    // Not a map: a bad block size, no entry in block 1.
    for (at, v) in [(2, 300), (512, 0)] {
        let mut d = apple_map(noise.clone(), 512, &entries);
        put_be16(&mut d, at, v);
        let layout = analyze_bytes(&d, size).unwrap();
        check_invariants(&layout);
        assert_eq!((layout.table, layout.used()), (Table::None, size));
    }

    // Next to an MBR, the MBR's partitions are used; the map's are kept as they are, but
    // its free space isn't.
    let mut d = apple_map(noise.clone(), 512, &entries);
    d[510..512].copy_from_slice(&[0x55, 0xAA]);
    d[0x1BE..0x1FE].copy_from_slice(&mbr_sector(&[(0, 0x83, 2048, 40960)])[0x1BE..0x1FE]);
    // Something recognisable in the MBR's partition, so its sectors are known to be 512 bytes.
    d[2048 * 512..][..6].copy_from_slice(b"LUKS\xba\xbe");
    let layout = analyze_bytes(&d, size).unwrap();
    check_invariants(&layout);
    assert_eq!(listed(&layout), [(1, 2048 * 512, 40960 * 512, false)]);
    assert_eq!(copied(&layout, 83968 * 512, 20480 * 512), 20480 * 512);
    assert_eq!(copied(&layout, 43008 * 512, 40960 * 512), 0);
    // Even the ones inside the MBR's partitions, whatever their file systems say.
    d[2048 * 512..][..6].copy_from_slice(&[0; 6]);
    let fat = fat_boot_sector(40960);
    d[2048 * 512..][..512].copy_from_slice(&fat);
    // Empty FATs and root directory: FAT itself would only keep its first sectors.
    d[2048 * 512 + 4 * 512..][..112 * 512].fill(0);
    let layout = analyze_bytes(&d, size).unwrap();
    check_invariants(&layout);
    assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
    assert_eq!(copied(&layout, 2048 * 512, 40960 * 512), 40960 * 512);
}

/// A FAT16 boot sector for a volume of `sectors` 512-byte sectors, with empty FATs.
fn fat_boot_sector(sectors: u64) -> [u8; 512] {
    let mut b = [0u8; 512];
    b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    b[3..11].copy_from_slice(b"MSWIN4.1");
    b[11..13].copy_from_slice(&512u16.to_le_bytes());
    b[13] = 4;
    b[14..16].copy_from_slice(&4u16.to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&512u16.to_le_bytes());
    b[21] = 0xF8;
    b[22..24].copy_from_slice(&40u16.to_le_bytes());
    b[32..36].copy_from_slice(&(sectors as u32).to_le_bytes());
    b[0x36..0x3E].copy_from_slice(b"FAT16   ");
    b[510..].copy_from_slice(&[0x55, 0xAA]);
    b
}

/// A BSD disklabel for 512-byte sectors: (size, offset, type) per partition.
fn disklabel(parts: &[(u64, u64, u8)], big: bool) -> [u8; 512] {
    let mut l = [0u8; 512];
    let mut put = |at: usize, v: u64, len: usize| {
        let bytes = if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let bytes = if big { &bytes[8 - len..] } else { &bytes[..len] };
        l[at..at + len].copy_from_slice(bytes);
    };
    put(0, 0x8256_4557, 4);
    put(132, 0x8256_4557, 4);
    put(40, 512, 4);
    put(138, parts.len() as u64, 2);
    put(140, 8192, 4);
    put(144, 8192, 4);
    for (i, &(size, offset, kind)) in parts.iter().enumerate() {
        put(148 + 16 * i, size, 4);
        put(152 + 16 * i, offset, 4);
        put(160 + 16 * i, u64::from(kind), 1);
    }
    label_checksum(&mut l, parts.len());
    l
}

/// Sets a disklabel's checksum: its 16-bit words XOR to zero, partitions included.
fn label_checksum(l: &mut [u8; 512], parts: usize) {
    l[136..138].fill(0);
    let xor = l[..148 + 16 * parts].as_chunks::<2>().0.iter().fold(0, |x, &w| x ^ u16::from_le_bytes(w));
    l[136..138].copy_from_slice(&xor.to_le_bytes());
}

#[test]
fn bsd_disklabels() {
    const FFS: u8 = 7;
    const SWAP: u8 = 1;
    let size = 96 * MIB;
    let noise = Rng(0xB5D).bytes(size as usize);
    // A slice from 1 to 81 MiB; in it (MiB from its start): a 1-21, b 22-32, c the whole
    // slice, d 40-60, e 70-75 of the "unused" type, and nothing in between.
    let (first, sectors) = (2048u64, 80 * 2048u64);
    let rel = [
        (20 * 2048, 2048, FFS),
        (10 * 2048, 22 * 2048, SWAP),
        (sectors, 0, 0),
        (20 * 2048, 40 * 2048, FFS),
        (5 * 2048, 70 * 2048, 0),
    ];
    let abs = rel.map(|(n, at, kind)| (n, at + first, kind));
    let disk = |kind: u8, label: &[u8; 512]| {
        let mut d = noise.clone();
        d[..512]
            .copy_from_slice(&mbr_sector(&[(0, 0x0C, 85 * 2048, 10 * 2048), (0, kind, first as u32, sectors as u32)]));
        d[(first as usize + 1) * 512..][..512].copy_from_slice(label);
        d
    };
    let mib = |n: u64| n * MIB;
    let slice = mib(1);
    // FreeBSD with offsets in the slice ("c" at 0) or on the drive; NetBSD, OpenBSD, and
    // either byte order.
    let whole = (size / 512, 0, 0);
    for (kind, parts, big) in [
        (0xA5, rel.to_vec(), false),
        (0xA5, abs.to_vec(), false),
        (0xA5, rel.to_vec(), true),
        (0xA9, [&abs[..], &[whole]].concat(), false),
        (0xA6, [&abs[..2], &[whole], &abs[3..]].concat(), false),
    ] {
        let layout = analyze_bytes(&disk(kind, &disklabel(&parts, big)), size).unwrap();
        check_invariants(&layout);
        assert_eq!(layout.table, Table::Mbr);
        let expected = [
            (1, mib(85), mib(10), false),
            (5, slice + mib(1), mib(20), false),
            (6, slice + mib(22), mib(10), false),
            (7, slice + mib(40), mib(20), false),
        ];
        assert_eq!(listed(&layout), expected, "type {kind:x}");
        // The boot area and label are kept, the unused-type partition too, but no gaps.
        assert_eq!(copied(&layout, slice, 64 * KIB), 64 * KIB);
        assert_eq!(copied(&layout, slice + mib(70), mib(5)), mib(5));
        for (from, to) in [(32, 40), (60, 70), (75, 80)] {
            assert_eq!(copied(&layout, slice + mib(from) + 64 * KIB, mib(to - from) - 128 * KIB), 0, "{from}-{to}");
        }
    }

    // Labels not to trust: the slice is then copied as a whole.
    let good = disklabel(&rel, false);
    let mut bad_sum = good;
    bad_sum[150] ^= 1;
    let mut sector_size = good;
    sector_size[40..44].copy_from_slice(&1024u32.to_le_bytes());
    label_checksum(&mut sector_size, rel.len());
    let across = disklabel(&[rel[0], (20 * 2048, 70 * 2048, FFS)], false);
    let lone_magic = {
        let mut l = [0u8; 512];
        l[..4].copy_from_slice(&0x8256_4557u32.to_le_bytes());
        l
    };
    for (why, label) in [("checksum", bad_sum), ("sector size", sector_size), ("across", across), ("magic", lone_magic)]
    {
        let layout = analyze_bytes(&disk(0xA5, &label), size).unwrap();
        check_invariants(&layout);
        assert_eq!(listed(&layout)[1], (2, slice, mib(80), false), "{why}");
        assert_eq!(copied(&layout, slice, mib(80)), mib(80), "{why}");
    }
    // A file system signature on the slice itself, or ZFS uberblocks: a label left over.
    for (at, magic) in [(0, &b"LUKS\xba\xbe"[..]), (128 * 1024 + 4096, &0x00BA_B10Cu64.to_le_bytes()[..])] {
        let mut d = disk(0xA5, &good);
        d[slice as usize + at..][..magic.len()].copy_from_slice(magic);
        let layout = analyze_bytes(&d, size).unwrap();
        assert_eq!(copied(&layout, slice, mib(80)), mib(80));
    }
    // Numbered after logical partitions: an extended partition with two, then the label's.
    let mut d = disk(0xA5, &good);
    d[..512].copy_from_slice(&mbr_sector(&[(0, 0xA5, first as u32, sectors as u32), (0, 0x05, 81 * 2048, 12 * 2048)]));
    d[81 * MIB as usize..][..512].copy_from_slice(&mbr_sector(&[(0, 0x83, 2048, 2048), (0, 0x05, 4096, 4096)]));
    d[83 * MIB as usize..][..512].copy_from_slice(&mbr_sector(&[(0, 0x83, 2048, 2048)]));
    let layout = analyze_bytes(&d, size).unwrap();
    check_invariants(&layout);
    let indexes: Vec<u32> = layout.partitions.iter().map(|p| p.index).collect();
    assert_eq!(indexes, [5, 6, 7, 8, 9]);
    assert_eq!(layout.partitions[2].start, slice + mib(1));
}

/// Random Apple partition maps and BSD disklabels: no panics, no broken promises.
#[test]
fn random_apple_maps_and_disklabels_never_panic() {
    let mut rng = Rng(0xA9_B5D);
    let size = 8 * MIB;
    let sectors = size / 512;
    let mut parsed = (0, 0);
    for round in 0..3000 {
        let mut d = vec![0u8; size as usize];
        let kinds = ["Apple_HFS", "Apple_Free", "Apple_partition_map", "Apple_Driver43", "Apple_UFS", "apple_free", ""];
        if round % 2 == 0 {
            let bs = [512, 2048, 1024, 4096, 0, 300][rng.below(6) as usize];
            let count = rng.below(8) as usize;
            let entries: Vec<(&str, &str, u64, u64)> = (0..count)
                .map(|_| {
                    let (first, len) = (rng.below(sectors), rng.below(sectors));
                    let (first, len) = (rng.wild(first), rng.wild(len));
                    ("x", kinds[rng.below(kinds.len() as u64) as usize], first & 0xFFFF_FFFF, len & 0xFFFF_FFFF)
                })
                .collect();
            d = apple_map(d, bs.max(512), &entries);
            put_be16(&mut d, 2, bs);
            if rng.below(4) == 0 {
                let count = rng.wild(count as u64 + 3);
                put_be32(&mut d, bs.max(512) as usize + 4, count);
            }
            // Sometimes an MBR too.
            if rng.below(3) == 0 {
                let mbr = mbr_sector(&[(0, 0x83, rng.below(sectors) as u32, rng.below(sectors) as u32)]);
                d[0x1BE..0x200].copy_from_slice(&mbr[0x1BE..0x200]);
            }
        } else {
            let kind = [0xA5, 0xA6, 0xA9][rng.below(3) as usize];
            let (first, len) = (rng.below(sectors / 2), rng.below(sectors));
            let len = rng.wild(len);
            d[..512].copy_from_slice(&mbr_sector(&[(0, kind, first as u32, len as u32)]));
            let parts: Vec<(u64, u64, u8)> = (0..rng.below(20))
                .map(|_| {
                    let offset = rng.below(sectors) + if rng.below(2) == 0 { 0 } else { first };
                    let (offset, len) = (rng.wild(offset), rng.below(sectors));
                    let len = rng.wild(len);
                    (len & 0xFFFF_FFFF, offset & 0xFFFF_FFFF, rng.below(20) as u8)
                })
                .collect();
            let mut label = disklabel(&parts, rng.below(4) == 0);
            if rng.below(3) == 0 {
                let at = rng.below(512) as usize;
                label[at] = rng.next() as u8;
            }
            if let Some(at) = ((first + 1) * 512).checked_add(512).filter(|&end| end <= size) {
                d[(at - 512) as usize..at as usize].copy_from_slice(&label);
            }
        }
        let layout = analyze_bytes(&d, size).unwrap_or_else(|e| panic!("round {round}: {e}"));
        check_invariants(&layout);
        if round % 2 == 0 {
            parsed.0 += usize::from(layout.table == partitions::APM && !layout.partitions.is_empty());
        } else {
            parsed.1 += usize::from(layout.table == Table::Mbr && layout.partitions.iter().any(|p| p.index >= 5));
        }
    }
    // Not vacuous: plenty of these got through.
    assert!(parsed.0 > 200 && parsed.1 > 50, "{parsed:?}");
}

#[cfg(target_os = "linux")]
mod linux {
    use super::super::tests::linux::*;
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const HFS: [&str; 2] = ["mkfs.hfsplus", "fsck.hfsplus"];

    /// An image of `size` bytes with a fresh HFS+ volume called "SmartHFS".
    fn hfs_image(s: &Scratch, file: &str, size: u64, args: &[&str]) -> PathBuf {
        let img = s.join(file);
        blank(&img, size);
        let mut all = vec!["-v", "SmartHFS"];
        all.extend(args);
        all.push(p(&img));
        sh("mkfs.hfsplus", &all);
        // This mkfs.hfsplus leaves a flag unset on HFSX volumes that fsck.hfsplus wants.
        if try_sh("fsck.hfsplus", &["-fn", p(&img)]).1.contains("HasFolderCount") {
            try_sh("fsck.hfsplus", &["-fy", p(&img)]);
        }
        img
    }

    /// A loop device of our own. Leaves /dev/loop0 alone (a GUI test drive on the dev
    /// machine), like the other tests.
    fn attach(img: &Path) -> Option<String> {
        let (ok, out) = sudo(&["losetup", "-f", "--show", p(img)]);
        let dev = out.trim().to_owned();
        if !ok || !dev.starts_with("/dev/loop") {
            eprintln!("skipped: losetup failed: {out}");
            return None;
        }
        if dev != "/dev/loop0" {
            return Some(dev);
        }
        let (ok, out) = sudo(&["losetup", "-f", "--show", p(img)]);
        sudo(&["losetup", "-d", "/dev/loop0"]);
        let dev = out.trim().to_owned();
        (ok && dev.starts_with("/dev/loop") && dev != "/dev/loop0").then_some(dev)
    }

    /// An HFS+ image loop-mounted by root (the kernel's hfsplus driver), unmounted and
    /// detached on drop.
    struct HfsMount {
        dev: String,
        dir: PathBuf,
        mounted: bool,
    }

    impl HfsMount {
        /// `force` writes to a journaled volume (without using the journal).
        fn mount(img: &Path, dir: &Path, read_only: bool, force: bool) -> Option<HfsMount> {
            fs::create_dir_all(dir).unwrap();
            let dev = attach(img)?;
            let mut m = HfsMount { dev, dir: dir.to_owned(), mounted: false };
            let mut opts = format!("uid={},gid={}", sh("id", &["-u"]).trim(), sh("id", &["-g"]).trim());
            if read_only {
                opts += ",ro";
            }
            if force {
                opts += ",force";
            }
            let (ok, out) = sudo(&["mount", "-t", "hfsplus", "-o", &opts, &m.dev, p(dir)]);
            m.mounted = ok;
            if !ok {
                eprintln!("mount failed: {out}");
                return None;
            }
            Some(m)
        }
    }

    impl Drop for HfsMount {
        fn drop(&mut self) {
            for _ in 0..10 {
                if !self.mounted {
                    break;
                }
                let (ok, out) = sudo(&["umount", p(&self.dir)]);
                self.mounted = !ok;
                if !ok {
                    eprintln!("umount {}: {out}", self.dir.display());
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            let (ok, out) = sudo(&["losetup", "-d", &self.dev]);
            if !ok {
                eprintln!("losetup -d {}: {out}", self.dev);
            }
        }
    }

    /// The files under `dir`, leaving out what HFS+ keeps in the root folder for itself
    /// (the journal files, the folder for hard links).
    fn read_hfs_tree(dir: &Path, prefix: &str, out: &mut Tree) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if prefix.is_empty() && (name.starts_with(".journal") || name.contains("HFS+ Private")) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                read_hfs_tree(&entry.path(), &format!("{prefix}{name}/"), out);
            } else {
                out.insert(format!("{prefix}{name}"), fs::read(entry.path()).unwrap());
            }
        }
    }

    /// Like `compare`, for HFS+.
    fn compare_hfs(expected: &Tree, dir: &Path) -> Result<(), String> {
        let mut found = Tree::new();
        read_hfs_tree(dir, "", &mut found);
        for (name, data) in expected {
            match found.get(name) {
                None => return Err(format!("{name} missing")),
                Some(d) if d != data => return Err(format!("{name} differs")),
                _ => {}
            }
        }
        match found.keys().find(|k| !expected.contains_key(*k)) {
            Some(extra) => Err(format!("unexpected file {extra}")),
            None => Ok(()),
        }
    }

    /// fsck.hfsplus; then, when there are files, a read-only mount (root) and every file.
    fn verify_hfs(img: &Path, files: &Tree, s: &Scratch, tag: &str) -> Result<(), String> {
        let (ok, text) = try_sh("fsck.hfsplus", &["-fn", p(img)]);
        if !ok || !text.contains("appears to be OK") {
            return Err(format!("fsck.hfsplus: {text}"));
        }
        if files.is_empty() {
            return Ok(());
        }
        let dir = s.join(&format!("{tag}-files"));
        let checked = match HfsMount::mount(img, &dir, true, true) {
            Some(_mount) => compare_hfs(files, &dir),
            None => Err("can't mount it".into()),
        };
        let _ = fs::remove_dir_all(&dir);
        checked
    }

    /// What the volume header at `vh` says: (block size, blocks, free blocks).
    fn hfs_numbers(img: &Path, vh: u64) -> (u64, u64, u64) {
        let h = read_range(img, vh, 512);
        (be32(&h, 0x28), be32(&h, 0x2C), be32(&h, 0x30))
    }

    /// Block by block, the used set against the bitmap read here: every used block is
    /// copied, and no free block past the partition's head is. `alloc`: the allocation
    /// file's extents, when not all in the volume header.
    fn check_blocks(img: &Path, start: u64, alloc: Option<&[(u64, u64)]>) {
        let size = file_size(img);
        let ours = fs_usage(img, start, size - start).unwrap_or_else(|why| panic!("not understood: {why}"));
        let h = read_range(img, start + 1024, 512);
        let (block, blocks) = (be32(&h, 0x28), be32(&h, 0x2C));
        let header: Vec<(u64, u64)> =
            (0..8).map(|i| (be32(&h, 0x70 + 16 + 8 * i), be32(&h, 0x70 + 20 + 8 * i))).filter(|e| e.1 > 0).collect();
        let mut map = Vec::new();
        for &(first, n) in alloc.unwrap_or(&header) {
            map.extend(read_range(img, start + first * block, n * block));
        }
        let mut free = 0;
        for b in 0..blocks {
            let (at, len) = (start + b * block, block);
            if map[(b / 8) as usize] >> (7 - b % 8) & 1 == 1 {
                assert_eq!(util::overlap(&ours, at, len), len, "used block {b} not copied");
            } else {
                free += 1;
                if at >= start + HEAD_KEPT {
                    assert_eq!(util::overlap(&ours, at, len), 0, "free block {b} copied");
                }
            }
        }
        assert_eq!(free, be32(&h, 0x30));
        let ours: u64 = ours.iter().map(|e| e.len).sum();
        eprintln!("{}: {ours} bytes used, {free} of {blocks} blocks of {block} free", img.display());
    }

    /// A smart copy of the image (garbage everywhere else) must pass the checks, and one
    /// without the biggest extent inside the volume must not.
    fn check_copy(img: &Path, files: &Tree, s: &Scratch, start: u64, len: u64) {
        let layout = layout_of(img);
        let copy = s.join("copy.img");
        let part = s.join("copy-part.img");
        let whole = start == 0 && len == file_size(img);
        let cut = |copy: &Path| -> PathBuf {
            if whole {
                copy.to_owned()
            } else {
                fs::write(&part, read_range(copy, start, len)).unwrap();
                part.clone()
            }
        };
        smart_copy(img, &layout.extents, &copy);
        verify_hfs(&cut(&copy), files, s, "smart").unwrap_or_else(|e| panic!("smart copy broken: {e}"));
        let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= start + len);
        if let Some(big) = inside.max_by_key(|e| e.len) {
            let fewer: Vec<Extent> = layout.extents.iter().filter(|e| *e != big).copied().collect();
            smart_copy(img, &fewer, &copy);
            assert!(verify_hfs(&cut(&copy), files, s, "broken").is_err(), "dropping {big:?} went unnoticed");
        }
        let _ = fs::remove_file(&copy);
        let _ = fs::remove_file(&part);
    }

    #[test]
    fn hfsplus_empty_variants() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("hfsplus");
        let variants: &[(&str, &[&str], u64)] = &[
            ("plain", &[], 64 * MB),
            ("journaled", &["-J"], 64 * MB),
            ("hfsx", &["-s"], 64 * MB),
            ("hfsx-journaled", &["-s", "-J", "1024k"], 96 * MB),
            ("512-byte-blocks", &["-b", "512"], 32 * MB),
            ("64k-blocks", &["-b", "65536"], 256 * MB),
            ("big-allocation-file", &["-c", "b=64"], 200 * MB),
            ("odd-size", &[], 50 * MB + 3 * KIB),
        ];
        for &(name, args, size) in variants {
            eprintln!("--- {name}");
            if !supports(&s, "mkfs.hfsplus", args, size) {
                continue;
            }
            let img = hfs_image(&s, &format!("{name}.img"), size, args);
            let layout = layout_of(&img);
            let part = &layout.partitions[0];
            assert!(part.understood, "{part:?}");
            assert_eq!(part.fs.as_deref(), Some("hfsplus"));
            assert_eq!(part.label.as_deref(), Some("SmartHFS"));
            check_blocks(&img, 0, None);
            // The tail past the last whole block, and the alternate volume header, are kept.
            let (block, blocks, _) = hfs_numbers(&img, 1024);
            assert_eq!(
                copied(&layout, blocks * block - 1024, size - blocks * block + 1024),
                size - blocks * block + 1024
            );
            check_copy(&img, &Tree::new(), &s, 0, size);
            fs::remove_file(&img).unwrap();
        }
    }

    /// Files written through the kernel's driver (as root), some deleted, two fragmented
    /// (their extents spill into the extents overflow file).
    fn fill_hfs(img: &Path, s: &Scratch, rng: &mut Rng, count: usize, journaled: bool) -> Option<Tree> {
        let (mut keep, gone) = trees(rng, count, 3 * MB);
        let dir = s.join("fill-mnt");
        let _mount = HfsMount::mount(img, &dir, false, journaled)?;
        write_tree(&keep, &dir);
        write_tree(&gone, &dir);
        for k in gone.keys() {
            fs::remove_file(dir.join(k)).unwrap();
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..40 {
            a.extend(rng.bytes(40_000));
            b.extend(rng.bytes(50_000));
            fs::write(dir.join("frag-a.bin"), &a).unwrap();
            fs::write(dir.join("frag-b.bin"), &b).unwrap();
        }
        keep.insert("frag-a.bin".into(), a);
        keep.insert("frag-b.bin".into(), b);
        Some(keep)
    }

    /// Leaf records in the extents overflow file of the volume at `start`.
    fn overflow_records(img: &Path, start: u64) -> u64 {
        let h = read_range(img, start + 1024, 512);
        let header = start + be32(&h, 0xC0 + 16) * be32(&h, 0x28);
        be32(&read_range(img, header, 64), 14 + 6)
    }

    /// Moves the allocation file of the volume at the start of `img` into `pieces` extents
    /// of a block or two, spread over free space in reverse order: eight in the volume
    /// header, the others in records of the (empty) extents overflow file, eight to a record.
    /// Returns the new extents.
    fn fragment_allocation_file(img: &Path, pieces: usize) -> Vec<(u64, u64)> {
        let mut d = fs::read(img).unwrap();
        let (vh, fork) = (1024, 1024 + 0x70);
        let block = be32(&d, vh + 0x28);
        let blocks = be32(&d, vh + 0x2C);
        let (total, first, count) = (be32(&d, fork + 12), be32(&d, fork + 16), be32(&d, fork + 20));
        assert!(count == total && total >= pieces as u64, "{total} blocks in {count}");
        let bs = block as usize;
        let mut map = d[first as usize * bs..(first + count) as usize * bs].to_vec();
        let used = |map: &[u8], b: u64| map[(b / 8) as usize] >> (7 - b % 8) & 1 == 1;
        let set = |map: &mut [u8], b: u64, on: bool| {
            let (i, bit) = ((b / 8) as usize, 0x80u8 >> (b % 8));
            if on { map[i] |= bit } else { map[i] &= !bit }
        };
        let mut sizes = vec![1; pieces];
        for i in 0..(total as usize - pieces) {
            sizes[i % pieces] += 1;
        }
        // Downwards from three quarters in, a free block between pieces.
        let mut new = Vec::new();
        let mut b = blocks * 3 / 4;
        for &n in &sizes {
            while (b - 1..b + n + 1).any(|x| used(&map, x)) {
                b -= 1;
            }
            new.push((b, n));
            b -= 3;
        }
        for x in first..first + count {
            set(&mut map, x, false);
        }
        for &(start, n) in &new {
            (start..start + n).for_each(|x| set(&mut map, x, true));
        }
        let mut at = 0;
        for &(start, n) in &new {
            let len = n as usize * bs;
            d[start as usize * bs..][..len].copy_from_slice(&map[at..at + len]);
            at += len;
        }
        for (i, &(start, n)) in new.iter().take(8).enumerate() {
            put_be32(&mut d, fork + 16 + 8 * i, start);
            put_be32(&mut d, fork + 20 + 8 * i, n);
        }
        // The extents overflow file: node 1 becomes its only leaf.
        let header = be32(&d, vh + 0xC0 + 16) as usize * bs;
        let node = be16(&d, header + 32) as usize;
        let mut leaf = vec![0u8; node];
        leaf[8] = 0xFF;
        leaf[9] = 1;
        let records: Vec<&[(u64, u64)]> = new[8..].chunks(8).collect();
        put_be16(&mut leaf, 10, records.len() as u64);
        let (mut pos, mut file_block) = (14, new[..8].iter().map(|e| e.1).sum::<u64>());
        for (i, record) in records.iter().enumerate() {
            put_be16(&mut leaf, node - 2 * (i + 1), pos as u64);
            put_be16(&mut leaf, pos, 10);
            put_be32(&mut leaf, pos + 4, 6);
            put_be32(&mut leaf, pos + 8, file_block);
            for (j, &(start, n)) in record.iter().enumerate() {
                put_be32(&mut leaf, pos + 12 + 8 * j, start);
                put_be32(&mut leaf, pos + 16 + 8 * j, n);
                file_block += n;
            }
            pos += 76;
        }
        put_be16(&mut leaf, node - 2 * (records.len() + 1), pos as u64);
        d[header + node..header + 2 * node].copy_from_slice(&leaf);
        // Its header record: depth 1, node 1 the root and only leaf, one node less free.
        let h = header + 14;
        put_be16(&mut d, h, 1);
        put_be32(&mut d, h + 2, 1);
        put_be32(&mut d, h + 6, records.len() as u64);
        put_be32(&mut d, h + 10, 1);
        put_be32(&mut d, h + 14, 1);
        let free_nodes = be32(&d, h + 26);
        put_be32(&mut d, h + 26, free_nodes - 1);
        let map_record = be16(&d, header + node - 6) as usize;
        d[header + map_record] |= 0x40;
        // The alternate volume header.
        let (len, vh_copy) = (d.len(), d[vh..vh + 512].to_vec());
        d[len - 1024..len - 512].copy_from_slice(&vh_copy);
        fs::write(img, &d).unwrap();
        new
    }

    #[test]
    fn hfsplus_allocation_file_in_many_extents() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("hfsplus-fragmented");
        // 512-byte blocks: 32 blocks of bitmap, in 20 extents.
        let img = hfs_image(&s, "fragmented.img", 64 * MB, &["-b", "512"]);
        let extents = fragment_allocation_file(&img, 20);
        verify_hfs(&img, &Tree::new(), &s, "edited").unwrap_or_else(|e| panic!("the edit broke the volume: {e}"));
        assert_eq!(overflow_records(&img, 0), 2);
        check_blocks(&img, 0, Some(&extents));
        check_copy(&img, &Tree::new(), &s, 0, 64 * MB);
        // Records that don't line up with the volume header's extents: copied in full.
        let good = fs::read(&img).unwrap();
        let header = be32(&good, 1024 + 0xC0 + 16) as usize * 512;
        let node = be16(&good, header + 32) as usize;
        let record = header + node + be16(&good, header + 2 * node - 2) as usize;
        let mut bad = good.clone();
        put_be32(&mut bad, record + 8, be32(&good, record + 8) + 1);
        let layout = analyze_bytes(&bad, bad.len() as u64).unwrap();
        assert!(!layout.partitions[0].understood);
        // Or a lost record.
        let mut bad = good.clone();
        put_be16(&mut bad, header + node + 10, 1);
        put_be32(&mut bad, header + 14 + 6, 1);
        let layout = analyze_bytes(&bad, bad.len() as u64).unwrap();
        assert!(!layout.partitions[0].understood);
    }

    /// The journal header checksum, as xnu computes it.
    fn journal_checksum(b: &[u8]) -> u32 {
        !b.iter().fold(0u32, |c, &x| (c << 8) ^ c.wrapping_add(u32::from(x)))
    }

    /// A journal header (4 KiB), in either byte order: transactions from `start` to `end`.
    fn journal_header(big: bool, start: u64, end: u64, size: u64, magic: u32) -> Vec<u8> {
        let mut h = Vec::new();
        let put32 = |h: &mut Vec<u8>, v: u32| h.extend(if big { v.to_be_bytes() } else { v.to_le_bytes() });
        put32(&mut h, magic);
        put32(&mut h, 0x1234_5678);
        for v in [start, end, size] {
            h.extend(if big { v.to_be_bytes() } else { v.to_le_bytes() });
        }
        put32(&mut h, 16 << 10);
        put32(&mut h, 0);
        put32(&mut h, 4096);
        put32(&mut h, 1);
        let sum = journal_checksum(&h[..44]);
        h[36..40].copy_from_slice(&if big { sum.to_be_bytes() } else { sum.to_le_bytes() });
        h.resize(4096, 0);
        h
    }

    #[test]
    fn hfsplus_unclean_and_journals() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("hfsplus-dirty");
        let img = hfs_image(&s, "j.img", 64 * MB, &["-J"]);
        let good = fs::read(&img).unwrap();
        assert!(understood(&good));
        // Mounted (the unmounted bit clear), or marked inconsistent.
        for (bit, on) in [(8, false), (11, true), (14, true)] {
            let mut d = good.clone();
            let attributes = be32(&d, 1024 + 4);
            put_be32(&mut d, 1024 + 4, if on { attributes | 1 << bit } else { attributes & !(1 << bit) });
            assert!(!understood(&d), "attribute bit {bit}");
            assert!(estimated(&d), "attribute bit {bit}");
        }
        // mkfs.hfsplus leaves the journal to be made at the first mount: nothing to replay.
        let jib = be32(&good, 1024 + 0x0C) as usize * 4096;
        assert_eq!(be32(&good, jib), 5);
        let journal = be32(&good, jib + 40) as usize;
        let size = be32(&good, jib + 48);
        // A journal in use: empty, or with transactions to replay, in either byte order.
        for big in [false, true] {
            for (start, end, magic, ok) in [
                (4096, 4096, 0x4A4E_4C78, true),
                (4096, 12288, 0x4A4E_4C78, false),
                (8192, 8192, 0x4A48_4452, true),
                (8192, 4096, 0x4A48_4452, false),
                (4096, 4096, 0x1234_5678, false),
            ] {
                let mut d = good.clone();
                put_be32(&mut d, jib, 1);
                d[journal..journal + 4096].copy_from_slice(&journal_header(big, start, end, size, magic));
                assert_eq!(understood(&d), ok, "big-endian {big}, {start}..{end}, magic {magic:x}");
                assert!(estimated(&d));
                // A bad checksum.
                if ok && magic == 0x4A4E_4C78 {
                    d[journal + 28] ^= 1;
                    assert!(!understood(&d));
                }
            }
        }
        // On another device.
        let mut d = good.clone();
        put_be32(&mut d, jib, 2);
        assert!(!understood(&d) && !estimated(&d));
        // The journal is always kept, used or not.
        let layout = layout_of(&img);
        assert_eq!(copied(&layout, journal as u64, size), size);
    }

    #[test]
    fn hfsplus_in_an_hfs_wrapper() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("hfsplus-wrapper");
        let inner = fs::read(hfs_image(&s, "inner.img", 32 * MB, &[])).unwrap();
        // The wrapper's 4 KiB allocation blocks start at sector 8; the HFS+ volume is blocks
        // 3 to 8194, with 20 more blocks after it. Random bytes around it, like wrapper files.
        let (first, block, at_block, count) = (8, 4096, 3, 32 * MB / 4096);
        let offset = first * 512 + at_block * block;
        let total = offset + 32 * MB + 20 * block;
        let mut d = Rng(0xBD).bytes(total as usize);
        let mdb = 1024;
        d[..mdb + 512].fill(0);
        d[mdb..mdb + 2].copy_from_slice(b"BD");
        put_be32(&mut d, mdb + 0x14, block);
        put_be16(&mut d, mdb + 0x12, (total - first * 512) / block);
        put_be16(&mut d, mdb + 0x1C, first);
        d[mdb + 0x7C..mdb + 0x7E].copy_from_slice(b"H+");
        put_be16(&mut d, mdb + 0x7E, at_block);
        put_be16(&mut d, mdb + 0x80, count);
        d[offset as usize..(offset + 32 * MB) as usize].copy_from_slice(&inner);
        let img = s.join("wrapped.img");
        fs::write(&img, &d).unwrap();
        let layout = layout_of(&img);
        let part = &layout.partitions[0];
        assert!(part.understood, "{part:?}");
        assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("hfsplus"), Some("SmartHFS")));
        // The wrapper is kept; the embedded volume's free space isn't.
        assert_eq!(copied(&layout, 0, offset), offset);
        let after = total - offset - 32 * MB;
        assert_eq!(copied(&layout, offset + 32 * MB, after), after);
        assert!(layout.used() < 4 * MB, "{} bytes used", layout.used());
        check_copy(&img, &Tree::new(), &s, offset, 32 * MB);
        // A volume sticking out of its wrapper.
        put_be16(&mut d, mdb + 0x80, count + 21);
        let layout = analyze_bytes(&d, total).unwrap();
        assert!(!layout.partitions[0].understood);
        assert_eq!(layout.used(), total);
        // Classic HFS: recognised only.
        d[mdb + 0x7C..mdb + 0x7E].fill(0);
        assert!(hfsplus::classic(&d[..128 << 10]) && !hfsplus::detect(&d[..128 << 10]));
        let layout = analyze_bytes(&d, total).unwrap();
        assert_eq!((layout.partitions[0].fs.as_deref(), layout.partitions[0].understood), (Some("hfs"), false));
        assert_eq!(layout.used(), total);
    }

    #[test]
    #[ignore]
    fn hfsplus_with_files() {
        if !have(&HFS) || !can_sudo() {
            return;
        }
        let s = Scratch::new("hfsplus-files");
        let mut rng = Rng(0x4846_5350);
        let variants: &[(&str, &[&str], u64)] = &[
            ("plain", &[], 160 * MB),
            // Written without the journal (Linux can't use it), which stays as mkfs made it.
            ("journaled", &["-J"], 160 * MB),
            ("hfsx-512", &["-s", "-b", "512"], 96 * MB),
        ];
        for &(name, args, size) in variants {
            eprintln!("--- {name}");
            let img = hfs_image(&s, &format!("{name}.img"), size, args);
            let Some(files) = fill_hfs(&img, &s, &mut rng, 150, args.contains(&"-J")) else { return };
            // Some extents of the fragmented files went to the extents overflow file.
            assert!(overflow_records(&img, 0) > 0);
            let layout = layout_of(&img);
            assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
            check_blocks(&img, 0, None);
            check_copy(&img, &files, &s, 0, size);
            fs::remove_file(&img).unwrap();
        }
    }

    const APFS: [&str; 2] = ["mkapfs", "apfsck"];
    const GB: u64 = 1 << 30;

    /// An image of `size` bytes (sparse) with a fresh APFS container, its volume called
    /// "SmartAPFS".
    fn apfs_image(s: &Scratch, file: &str, size: u64, args: &[&str]) -> PathBuf {
        let img = s.join(file);
        blank(&img, size);
        let mut all = vec!["-L", "SmartAPFS"];
        all.extend(args);
        all.push(p(&img));
        sh("mkapfs", &all);
        img
    }

    fn verify_apfs(img: &Path) -> Result<(), String> {
        match try_sh("apfsck", &[p(img)]) {
            (true, _) => Ok(()),
            (false, text) => Err(format!("apfsck: {text}")),
        }
    }

    /// Like `smart_copy`, with zeros (a sparse file) instead of garbage, for images too big
    /// to write out.
    fn sparse_copy(src: &Path, extents: &[Extent], dst: &Path) {
        let data = fs::File::open(src).unwrap();
        let out = fs::File::create(dst).unwrap();
        out.set_len(file_size(src)).unwrap();
        for e in extents {
            let mut done = 0;
            while done < e.len {
                let n = (e.len - done).min(4 * MB);
                let buf = {
                    use std::os::unix::fs::FileExt;
                    let mut buf = vec![0; n as usize];
                    data.read_exact_at(&mut buf, e.start + done).unwrap();
                    buf
                };
                std::os::unix::fs::FileExt::write_all_at(&out, &buf, e.start + done).unwrap();
                done += n;
            }
        }
    }

    /// Where mkapfs put things in its (only) checkpoint: block size, and the blocks of the
    /// checkpoint map, the space manager, the free queue roots and the first chunk-info
    /// block.
    struct Nx {
        bs: usize,
        map: usize,
        spaceman: usize,
        queues: Vec<usize>,
        cib: usize,
    }

    fn nx_of(d: &[u8]) -> Nx {
        let bs = le32(d, 36) as usize;
        let map = le64(d, 0x70) as usize;
        assert_eq!(le32(d, map * bs + 24), 0x4000_000C, "a checkpoint map first");
        let m = &d[map * bs..][..bs];
        let (mut spaceman, mut queues) = (0, Vec::new());
        for e in (0..le32(m, 0x24) as usize).map(|k| 0x28 + 40 * k) {
            match (le32(m, e), le32(m, e + 4)) {
                (0x8000_0005, 0) => spaceman = le64(m, e + 32) as usize,
                (0x8000_0002, 9) => queues.push(le64(m, e + 32) as usize),
                _ => {}
            }
        }
        let sm = &d[spaceman * bs..][..bs];
        assert_eq!(le32(sm, 0x44), 0, "no chunk-info address blocks");
        let cib = le64(sm, le32(sm, 0x50) as usize) as usize;
        Nx { bs, map, spaceman, queues, cib }
    }

    /// (block size, blocks, free blocks) as the space manager counts them.
    fn apfs_numbers(img: &Path) -> (u64, u64, u64) {
        let head = read_range(img, 0, 4096);
        let bs = le32(&head, 36);
        let map = read_range(img, le64(&head, 0x70) * bs, bs);
        let sm = (0..le32(&map, 0x24) as usize)
            .map(|k| 0x28 + 40 * k)
            .find(|&e| le32(&map, e) == 0x8000_0005)
            .map(|e| le64(&map, e + 32))
            .unwrap();
        let sm = read_range(img, sm * bs, bs);
        (bs, le64(&sm, 0x30), le64(&sm, 0x48))
    }

    /// Numbers, a smart copy that apfsck must pass, and one without the biggest extent inside
    /// the container that it must not. Big images get sparse copies (zeros, not garbage).
    fn check_apfs(img: &Path, s: &Scratch, start: u64, len: u64) {
        let layout = layout_of(img);
        let part = layout.partitions.iter().find(|p| p.start == start).unwrap();
        assert!(part.understood, "{part:?}");
        assert_eq!((part.fs.as_deref(), part.label.as_deref()), (Some("apfs"), Some("SmartAPFS")));
        let whole = start == 0 && len == file_size(img);
        let fs_img = s.join("container.img");
        let container = if whole {
            img.to_owned()
        } else {
            fs::write(&fs_img, read_range(img, start, len)).unwrap();
            fs_img.clone()
        };
        let (bs, blocks, free) = apfs_numbers(&container);
        let exact = fs_usage(img, start, len).unwrap();
        let ours: u64 = exact.iter().map(|e| e.len).sum();
        let used = (blocks - free) * bs;
        eprintln!("{}: ours {ours}, the space manager's {used} of {}", img.display(), blocks * bs);
        assert!(ours >= used && ours <= used + HEAD_KEPT + (len - blocks * bs), "{ours} vs {used}");
        let big = len > GB;
        let copy = s.join("copy.img");
        let part_copy = s.join("copy-part.img");
        let cut = |copy: &Path| -> PathBuf {
            if whole {
                copy.to_owned()
            } else {
                fs::write(&part_copy, read_range(copy, start, len)).unwrap();
                part_copy.clone()
            }
        };
        let make =
            |extents: &[Extent]| if big { sparse_copy(img, extents, &copy) } else { smart_copy(img, extents, &copy) };
        make(&layout.extents);
        verify_apfs(&cut(&copy)).unwrap_or_else(|e| panic!("smart copy broken: {e}"));
        // Not the drive's last MiB, say, where the container may have nothing.
        let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= start + len);
        let inside = inside.filter(|e| util::overlap(&exact, e.start, e.len) > 0);
        if let Some(biggest) = inside.max_by_key(|e| e.len) {
            let fewer: Vec<Extent> = layout.extents.iter().filter(|e| *e != biggest).copied().collect();
            make(&fewer);
            assert!(verify_apfs(&cut(&copy)).is_err(), "dropping {biggest:?} went unnoticed");
        }
        for f in [&copy, &part_copy, &fs_img] {
            let _ = fs::remove_file(f);
        }
    }

    #[test]
    fn apfs_containers() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs");
        let variants: &[(&str, &[&str], u64)] = &[
            ("small", &[], 32 * MB),
            ("plain", &[], 200 * MB),
            ("case-sensitive", &["-s"], 200 * MB),
            ("odd-size", &[], 300 * MB + 5 * KIB),
            // Several chunk-info blocks.
            ("80g", &[], 80 * GB),
            // Chunk-info address blocks too.
            ("10t", &[], 10 << 40),
        ];
        for &(name, args, size) in variants {
            eprintln!("--- {name}");
            let img = apfs_image(&s, &format!("{name}.img"), size, args);
            check_apfs(&img, &s, 0, size);
            fs::remove_file(&img).unwrap();
        }
    }

    #[test]
    fn apfs_bitmap_edits() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs-bitmaps");
        let mut d = fs::read(apfs_image(&s, "c.img", 200 * MB, &[])).unwrap();
        let nx = nx_of(&d);
        let bs = nx.bs;
        let (sm, cib) = (nx.spaceman * bs, nx.cib * bs);
        let chunk = cib + 0x28;
        let bitmap = le64(&d, chunk + 24) as usize * bs;
        let bit = |d: &[u8], b: usize| d[bitmap + b / 8] >> (b % 8) & 1 == 1;
        // Blocks 30000 to 30099 marked used, with random data in and around them.
        let (a, n) = (30_000, 100);
        assert!((a - 50..a + n + 50).all(|b| !bit(&d, b)));
        let noise = Rng(0xA9F5).bytes((n + 100) * bs);
        d[(a - 50) * bs..(a + n + 50) * bs].copy_from_slice(&noise);
        let good = d.clone();
        for b in a..a + n {
            d[bitmap + b / 8] |= 1 << (b % 8);
        }
        let free = le32(&d, chunk + 20) - n as u64;
        put_le32(&mut d, chunk + 20, free);
        seal(&mut d[cib..cib + bs]);
        let free = le64(&d, sm + 0x48) - n as u64;
        put_le64(&mut d, sm + 0x48, free);
        seal(&mut d[sm..sm + bs]);
        let layout = analyze_bytes(&d, d.len() as u64).unwrap();
        assert!(layout.partitions[0].understood);
        let (a, n, bs) = (a as u64, n as u64, bs as u64);
        assert_eq!(copied(&layout, a * bs, n * bs), n * bs);
        assert_eq!(copied(&layout, (a - 50) * bs, 30 * bs), 0);
        assert_eq!(copied(&layout, (a + n + 20) * bs, 30 * bs), 0);
        let bs = bs as usize;
        // A bit set behind the counts' back: copied in full, but fine for an estimate.
        let mut bad = good.clone();
        bad[bitmap + 1000] |= 0x10;
        assert!(!understood(&bad) && estimated(&bad));
        // A bit cleared the same way.
        let mut bad = good.clone();
        bad[bitmap] &= !1;
        assert!(!understood(&bad) && estimated(&bad));
        // Checksums: the chunk-info block's, the space manager's, the checkpoint map's.
        for at in [cib + 0x30, sm + 0x48, nx.map * bs + 0x40] {
            let mut bad = good.clone();
            bad[at] ^= 4;
            assert!(!understood(&bad) && !estimated(&bad), "change at {at} unnoticed");
        }
        // A chunk claiming to be all free (no bitmap) while its count says otherwise.
        let mut bad = good.clone();
        put_le64(&mut bad, chunk + 24, 0);
        seal(&mut bad[cib..cib + bs]);
        assert!(!understood(&bad) && !estimated(&bad));
    }

    /// Adds records to a free queue's (empty, leaf) root: (first block, blocks), one block
    /// making a ghost record.
    fn queue_records(node: &mut [u8], records: &[(u64, u64)]) {
        let bs = node.len();
        let (toc, keys, end) = (0x38, 0x38 + usize::from(u16::from_le_bytes([node[0x2A], node[0x2B]])), bs - 40);
        assert_eq!(le32(node, 0x24), 0);
        let mut values = 0;
        for (i, &(first, n)) in records.iter().enumerate() {
            put_le64(node, keys + 16 * i, 1);
            put_le64(node, keys + 16 * i + 8, first);
            let v = if n == 1 {
                0xFFFF
            } else {
                values += 8;
                put_le64(node, end - values, n);
                values
            };
            node[toc + 4 * i..toc + 4 * i + 2].copy_from_slice(&((16 * i) as u16).to_le_bytes());
            node[toc + 4 * i + 2..toc + 4 * i + 4].copy_from_slice(&(v as u16).to_le_bytes());
        }
        put_le32(node, 0x24, records.len() as u64);
        let free = (bs - 40 - values) - (keys + 16 * records.len());
        node[0x2C..0x2E].copy_from_slice(&((16 * records.len()) as u16).to_le_bytes());
        node[0x2E..0x30].copy_from_slice(&(free as u16).to_le_bytes());
        put_le64(node, end + 24, records.len() as u64);
        seal(node);
    }

    #[test]
    fn apfs_free_queues() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs-queues");
        let mut d = fs::read(apfs_image(&s, "q.img", 200 * MB, &[])).unwrap();
        let nx = nx_of(&d);
        let bs = nx.bs;
        let sm = nx.spaceman * bs;
        // The main device's queue: freed blocks, still needed by older checkpoints.
        let main = le64(&d, sm + 0xF8);
        let root = nx.queues.iter().map(|&b| b * bs).find(|&at| le64(&d, at + 8) == main).unwrap();
        let (a, n, b) = (20_000u64, 5, 40_000u64);
        let empty = d.clone();
        queue_records(&mut d[root..root + bs], &[(a, n), (b, 1)]);
        put_le64(&mut d, sm + 0xF0, n + 1);
        put_le64(&mut d, sm + 0x100, 1);
        seal(&mut d[sm..sm + bs]);
        let layout = analyze_bytes(&d, d.len() as u64).unwrap();
        assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
        let bs = bs as u64;
        assert_eq!(copied(&layout, a * bs, n * bs), n * bs);
        assert_eq!(copied(&layout, b * bs, bs), bs);
        // The count in the space manager must match.
        let mut bad = d.clone();
        put_le64(&mut bad, sm + 0xF0, n);
        seal(&mut bad[sm..sm + bs as usize]);
        assert!(!understood(&bad) && estimated(&bad));
        // Blocks past the container's end.
        let mut bad = empty.clone();
        queue_records(&mut bad[root..root + bs as usize], &[(le64(&d, 40) - 2, 5)]);
        put_le64(&mut bad, sm + 0xF0, 5);
        seal(&mut bad[sm..sm + bs as usize]);
        assert!(!understood(&bad) && !estimated(&bad));
        // The second device's queue must be empty.
        let mut bad = d.clone();
        put_le64(&mut bad, sm + 0x118, 1);
        seal(&mut bad[sm..sm + bs as usize]);
        assert!(!understood(&bad) && !estimated(&bad));
    }

    #[test]
    fn apfs_checkpoints() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs-checkpoints");
        let mut d = fs::read(apfs_image(&s, "k.img", 200 * MB, &[])).unwrap();
        let nx = nx_of(&d);
        let bs = nx.bs;
        let (desc, data, data_blocks) = (le64(&d, 0x70) as usize, le64(&d, 0x78) as usize, le32(&d, 0x6C) as usize);
        assert_eq!((nx.map, le32(&d, 0x8C)), (desc, 2), "the map, then the superblock");
        // Move the first checkpoint's ephemeral objects from the start of the data area to
        // blocks 10 to 13 of it, so that the second checkpoint's can wrap around to block 0.
        let map = desc * bs;
        let count = le32(&d, map + 0x24) as usize;
        assert!(count <= 4);
        for e in (0..count).map(|k| map + 0x28 + 40 * k) {
            let from = le64(&d, e + 32) as usize;
            let to = data + 10 + (from - data);
            d.copy_within(from * bs..(from + 1) * bs, to * bs);
            put_le64(&mut d, e + 32, to as u64);
        }
        put_le32(&mut d, (desc + 1) * bs + 0x90, 10);
        put_le32(&mut d, (desc + 1) * bs + 0x84, 10 + count as u64);
        seal(&mut d[map..map + bs]);
        seal(&mut d[(desc + 1) * bs..(desc + 2) * bs]);
        d.copy_within((desc + 1) * bs..(desc + 2) * bs, 0);
        assert!(understood(&d), "moving the objects broke it");
        let first = d.clone();

        // The second checkpoint: its map and superblock next in the descriptor area, its
        // objects at the end of the data area, the space manager (two blocks now) wrapping
        // around to block 0. Its main free queue holds blocks the first one doesn't know.
        let (map2, sb2) = ((desc + 2) * bs, (desc + 3) * bs);
        d.copy_within(map..map + bs, map2);
        d.copy_within((desc + 1) * bs..(desc + 2) * bs, sb2);
        let mut slot = data_blocks - count;
        let (queued, n) = (50_000u64, 3);
        let entries: Vec<usize> = (0..count).map(|k| map2 + 0x28 + 40 * k).collect();
        let (mut last, mut others): (Vec<usize>, Vec<usize>) =
            entries.into_iter().partition(|&e| le32(&d, e) == 0x8000_0005);
        others.append(&mut last);
        for e in others {
            let from = le64(&d, e + 32) as usize;
            let mut o = d[from * bs..(from + 1) * bs].to_vec();
            put_le64(&mut o, 16, 2);
            let spaceman = le32(&d, e) == 0x8000_0005;
            if spaceman {
                o.resize(2 * bs, 0);
                put_le64(&mut o, 0xF0, n);
                put_le64(&mut o, 0x100, 1);
                put_le32(&mut d, e + 8, 2 * bs as u64);
            }
            if le64(&o, 8) == le64(&first, nx.spaceman * bs + 0xF8) {
                queue_records(&mut o, &[(queued, n)]);
            }
            seal(&mut o);
            for (i, block) in o.chunks(bs).enumerate() {
                let at = data + (slot + i) % data_blocks;
                d[at * bs..(at + 1) * bs].copy_from_slice(block);
            }
            put_le64(&mut d, e + 32, (data + slot) as u64);
            slot = (slot + o.len() / bs) % data_blocks;
        }
        assert_eq!(slot, 1, "the space manager went last, around the end");
        put_le64(&mut d, map2 + 8, (desc + 2) as u64);
        put_le64(&mut d, map2 + 16, 2);
        seal(&mut d[map2..map2 + bs]);
        put_le64(&mut d, sb2 + 16, 2);
        put_le64(&mut d, sb2 + 0x60, 3);
        for (at, v) in [(0x80, 4), (0x88, 2), (0x8C, 2), (0x84, 1), (0x90, data_blocks - count), (0x94, count + 1)] {
            put_le32(&mut d, sb2 + at, v as u64);
        }
        seal(&mut d[sb2..sb2 + bs]);
        d.copy_within(sb2..sb2 + bs, 0);
        let bs64 = bs as u64;
        let layout = analyze_bytes(&d, d.len() as u64).unwrap();
        assert!(layout.partitions[0].understood, "{:?}", layout.partitions[0]);
        assert_eq!(copied(&layout, queued * bs64, n * bs64), n * bs64);

        // Block 0 from before the second checkpoint: not cleanly unmounted.
        let mut old = d.clone();
        old[..bs].copy_from_slice(&first[..bs]);
        assert!(!understood(&old) && estimated(&old));
        // The second checkpoint's superblock torn: the first one is the latest valid one.
        let mut torn = d.clone();
        torn[sb2 + 100] ^= 1;
        assert!(!understood(&torn));
        let layout = estimate(&mut std::io::Cursor::new(&torn), torn.len() as u64).unwrap();
        assert!(layout.partitions[0].understood);
        assert_eq!(copied(&layout, queued * bs64, n * bs64), 0);
        // Two checkpoints with the same transaction ID.
        let mut twice = d.clone();
        put_le64(&mut twice, sb2 + 16, 1);
        seal(&mut twice[sb2..sb2 + bs]);
        assert!(!understood(&twice) && !estimated(&twice));
        // A checkpoint map entry pointing out of the data area.
        let mut bad = d.clone();
        put_le64(&mut bad, map2 + 0x28 + 32, (data + data_blocks) as u64);
        seal(&mut bad[map2..map2 + bs]);
        assert!(!understood(&bad) && !estimated(&bad));
        // The wrapped space manager torn.
        let mut bad = d.clone();
        bad[data * bs + 7] ^= 1;
        assert!(!understood(&bad) && !estimated(&bad));
    }

    #[test]
    fn apfs_fusion_drives_are_copied_whole() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs-fusion");
        let (main, tier2) = (s.join("main.img"), s.join("tier2.img"));
        blank(&main, 200 * MB);
        blank(&tier2, 300 * MB);
        sh("mkapfs", &["-F", p(&tier2), p(&main)]);
        for img in [&main, &tier2] {
            let layout = layout_of(img);
            assert_eq!(layout.partitions[0].fs.as_deref(), Some("apfs"));
            assert!(!layout.partitions[0].understood);
            assert_eq!(layout.used(), file_size(img));
        }
    }

    #[test]
    fn apple_partition_map_with_file_systems() {
        if !have(&HFS) || !have(&["mkfs.fat", "fsck.fat", "mcopy", "mdel"]) {
            return;
        }
        let s = Scratch::new("apm");
        let size = 160 * MB;
        let mib = 2048;
        // The map, a driver, HFS+ in MiB 1-49, free space 49-89, FAT32 89-129, free space.
        let entries = [
            ("Apple", "Apple_partition_map", 1, 63),
            ("Macintosh", "Apple_Driver43", 64, 56),
            ("Mac", "Apple_HFS", mib, 48 * mib),
            ("Extra", "Apple_Free", 49 * mib, 40 * mib),
            ("PC", "DOS_FAT_32", 89 * mib, 40 * mib),
            ("Extra", "Apple_Free", 129 * mib, 31 * mib),
        ];
        let disk = s.join("disk.img");
        fs::write(&disk, apple_map(Rng(0xA93F).bytes(size as usize), 512, &entries)).unwrap();
        let hfs = hfs_image(&s, "hfs.img", 48 * MB, &["-J"]);
        put(&disk, MB, &hfs);
        let fat = fat_image(&s, "fat.img", 40 * MB, &["-F", "32"], 60, 2 * MB);
        put(&disk, 89 * MB, &fat.img);
        let layout = layout_of(&disk);
        assert_eq!(layout.table, partitions::APM);
        assert_eq!(listed(&layout), [(3, MB, 48 * MB, true), (5, 89 * MB, 40 * MB, true)]);
        let names: Vec<_> = layout.partitions.iter().map(|p| p.fs.as_deref()).collect();
        assert_eq!(names, [Some("hfsplus"), Some("vfat")]);
        for (start, len) in [(49 * MB, 40 * MB), (129 * MB, 30 * MB)] {
            assert_eq!(copied(&layout, start + 64 * KIB, len - 128 * KIB), 0, "free space at {start} copied");
        }
        check_copy(&disk, &Tree::new(), &s, MB, 48 * MB);
        check_fs(&Fixture { img: disk.clone(), ..fat }, &s, &disk, 89 * MB, 40 * MB, &layout);
    }

    #[test]
    fn bsd_slice_with_file_systems() {
        let tools = ["mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools) {
            return;
        }
        let s = Scratch::new("bsd");
        let size = 200 * MB;
        let mib = 2048;
        // A FreeBSD slice in MiB 1-161; in it: a FAT32 1-41, b 41-49 (random), c the slice,
        // d ext4 60-120, and random bytes in the gaps.
        let slice = MB;
        let label = disklabel(
            &[(40 * mib, mib, 8), (8 * mib, 41 * mib, 1), (160 * mib, 0, 0), (60 * mib, 60 * mib, 17)],
            false,
        );
        let mut d = Rng(0xB5D2).bytes(size as usize);
        d[..512].copy_from_slice(&mbr_sector(&[(0x80, 0xA5, mib as u32, 160 * mib as u32)]));
        d[slice as usize..][..1024].fill(0);
        d[slice as usize + 512..][..512].copy_from_slice(&label);
        let disk = s.join("disk.img");
        fs::write(&disk, d).unwrap();
        let fat = fat_image(&s, "fat.img", 40 * MB, &["-F", "32"], 60, 2 * MB);
        put(&disk, slice + MB, &fat.img);
        let ext = ext_image(&s, "ext.img", 60 * MB, &["-t", "ext4"], 80, 2 * MB);
        put(&disk, slice + 60 * MB, &ext.img);
        let layout = layout_of(&disk);
        assert_eq!(layout.table, Table::Mbr);
        let expected = [(5, 2 * MB, 40 * MB, true), (6, 42 * MB, 8 * MB, false), (7, 61 * MB, 60 * MB, true)];
        assert_eq!(listed(&layout), expected);
        for (start, len) in [(50 * MB, 11 * MB), (121 * MB, 40 * MB)] {
            assert_eq!(copied(&layout, start + 64 * KIB, len - 128 * KIB), 0, "free space at {start} copied");
        }
        check_fs(&Fixture { img: disk.clone(), ..fat }, &s, &disk, 2 * MB, 40 * MB, &layout);
        check_fs(&Fixture { img: disk.clone(), ..ext }, &s, &disk, 61 * MB, 60 * MB, &layout);
    }

    #[test]
    fn hybrid_cd_images() {
        if !have(&["xorriso"]) || !have(&HFS) {
            return;
        }
        let s = Scratch::new("hybrid-cd");
        let src = s.join("src");
        write_tree(&make_tree(&mut Rng(0x150), "cd", 40, MB), &src);
        for (name, extra) in [("apm", &[][..]), ("apm-mbr", &["--protective-msdos-label"][..])] {
            eprintln!("--- {name}");
            let iso = s.join(&format!("{name}.iso"));
            let mut args = vec!["-as", "mkisofs", "-o", p(&iso), "-hfsplus", "-apm-block-size", "2048", "-V", "HYBRID"];
            args.extend(extra);
            args.push(p(&src));
            sh("xorriso", &args);
            let layout = layout_of(&iso);
            // An ISO 9660 image at the start: kept whole.
            assert_eq!(layout.used(), layout.size);
            if name == "apm" {
                assert_eq!(layout.table, partitions::APM);
                let hfs = layout.partitions.iter().find(|p| p.fs.as_deref() == Some("hfsplus")).unwrap();
                assert!(hfs.understood && hfs.label.as_deref() == Some("HYBRID"), "{hfs:?}");
            } else {
                assert_eq!(layout.table, Table::Mbr);
            }
        }
    }

    #[test]
    fn gpt_with_apple_partitions() {
        if !have(&APFS) || !have(&HFS) || !have(&["sfdisk"]) {
            return;
        }
        let s = Scratch::new("gpt-apple");
        let size = 300 * MB;
        let disk = s.join("disk.img");
        fs::write(&disk, Rng(0x6A7).bytes(size as usize)).unwrap();
        let mib = 2048;
        let script = format!(
            "label: gpt\nstart={mib}, size={}, type=7C3457EF-0000-11AA-AA11-00306543ECAC\n\
             start={}, size={}, type=48465300-0000-11AA-AA11-00306543ECAC\n",
            200 * mib,
            201 * mib,
            64 * mib
        );
        let mut child = std::process::Command::new("sfdisk")
            .args(["-q", p(&disk)])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        let apfs = apfs_image(&s, "apfs.img", 200 * MB, &[]);
        put(&disk, MB, &apfs);
        let hfs = hfs_image(&s, "hfs.img", 64 * MB, &[]);
        put(&disk, 201 * MB, &hfs);
        let layout = layout_of(&disk);
        assert_eq!(layout.table, Table::Gpt);
        assert_eq!(listed(&layout), [(1, MB, 200 * MB, true), (2, 201 * MB, 64 * MB, true)]);
        assert_eq!(copied(&layout, 265 * MB + 64 * KIB, 30 * MB), 0);
        check_apfs(&disk, &s, MB, 200 * MB);
        check_copy(&disk, &Tree::new(), &s, 201 * MB, 64 * MB);
    }

    /// Images of every kind these tests make, small enough to fuzz in memory.
    fn apple_images(s: &Scratch) -> Vec<(String, Vec<u8>)> {
        let mut images = Vec::new();
        for (name, args, size) in [
            ("hfsplus", &[][..], 16 * MB),
            ("hfsplus-journaled", &["-J", "512k"][..], 24 * MB),
            ("hfsx-512", &["-s", "-b", "512"][..], 32 * MB),
        ] {
            let img = hfs_image(s, &format!("{name}.img"), size, args);
            if name == "hfsx-512" {
                fragment_allocation_file(&img, 12);
            }
            images.push((name.to_owned(), fs::read(&img).unwrap()));
        }
        if have(&APFS) {
            for (name, size) in [("apfs-small", 32 * MB), ("apfs", 100 * MB)] {
                images.push((name.to_owned(), fs::read(apfs_image(s, &format!("{name}.img"), size, &[])).unwrap()));
            }
        }
        // An Apple partition map holding an HFS+ volume, and a BSD slice holding another.
        let hfs = images[0].1.clone();
        let mut apm = apple_map(
            vec![0; 24 * MB as usize],
            512,
            &[
                ("Apple", "Apple_partition_map", 1, 63),
                ("Mac", "Apple_HFS", 2048, 16 * 2048),
                ("Extra", "Apple_Free", 17 * 2048, 7 * 2048),
            ],
        );
        apm[MB as usize..17 * MB as usize].copy_from_slice(&hfs);
        images.push(("apm".into(), apm));
        let mut bsd = vec![0; 24 * MB as usize];
        bsd[..512].copy_from_slice(&mbr_sector(&[(0, 0xA5, 2048, 22 * 2048)]));
        bsd[MB as usize + 512..][..512].copy_from_slice(&disklabel(&[(16 * 2048, 2048, 8), (22 * 2048, 0, 0)], false));
        bsd[2 * MB as usize..18 * MB as usize].copy_from_slice(&hfs);
        images.push(("bsd".into(), bsd));
        images
    }

    /// Random corruption in what the analysis reads: no panics, no broken promises. And
    /// changes where the analysis says nothing's used change nothing.
    #[test]
    fn apple_metadata_mutations() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("apple-fuzz");
        let mut rng = Rng(0xF0A9);
        let rounds = std::env::var("DD_GUI_SMART_FUZZ_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(300);
        for (name, mut data) in apple_images(&s) {
            let size = data.len() as u64;
            let clean = analyze_bytes(&data, size).unwrap();
            check_invariants(&clean);
            assert!(clean.partitions.iter().any(|p| p.understood), "{name}: {:?}", clean.partitions);
            let targets = clean.extents.clone();
            // Free space: between the extents.
            let mut free = Vec::new();
            let mut at = 0;
            for e in &targets {
                if e.start > at {
                    free.push(Extent { start: at, len: e.start - at });
                }
                at = e.start + e.len;
            }
            for round in 0..rounds {
                let mut undo = Vec::new();
                let in_free = round % 4 == 3 && !free.is_empty();
                let pool = if in_free { &free } else { &targets };
                for _ in 0..1 + rng.below(8) {
                    let e = pool[rng.below(pool.len() as u64) as usize];
                    let span = if !in_free && rng.below(2) == 0 { e.len.min(8192) } else { e.len };
                    let at = (e.start + rng.below(span)) as usize;
                    let width = [1, 2, 4, 8][rng.below(4) as usize].min((e.start + e.len) as usize - at);
                    let value: [u8; 8] = match rng.below(4) {
                        0 => [0; 8],
                        1 => [0xFF; 8],
                        _ => rng.next().to_le_bytes(),
                    };
                    undo.push((at, data[at..at + width].to_vec()));
                    data[at..at + width].copy_from_slice(&value[..width]);
                }
                let layout = analyze_bytes(&data, size).unwrap_or_else(|e| panic!("{name} round {round}: {e}"));
                check_invariants(&layout);
                if in_free {
                    assert_eq!(layout.extents, clean.extents, "{name} round {round}: free space changed the layout");
                }
                for (at, old) in undo.into_iter().rev() {
                    data[at..at + old.len()].copy_from_slice(&old);
                }
            }
        }
    }

    /// Fields of APFS objects changed at random, with their checksums fixed up so that the
    /// changes get past them: the checks behind the checksums hold without panics.
    #[test]
    fn apfs_resealed_mutations() {
        if !have(&APFS) {
            return;
        }
        let s = Scratch::new("apfs-reseal");
        let mut rng = Rng(0x5EA1);
        let rounds = std::env::var("DD_GUI_SMART_FUZZ_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(1000);
        let bs = 4096;
        for size in [32 * MB, 100 * MB] {
            let clean = fs::read(apfs_image(&s, "r.img", size, &[])).unwrap();
            // Every block that's an object with a good checksum.
            let sealed = |o: &[u8]| {
                let mut copy = o.to_vec();
                seal(&mut copy);
                le64(o, 0) != 0 && copy[..8] == o[..8]
            };
            let objects: Vec<usize> = (0..clean.len() / bs).filter(|&b| sealed(&clean[b * bs..(b + 1) * bs])).collect();
            assert!(objects.len() >= 10, "{} objects", objects.len());
            let mut d = clean.clone();
            let (mut still, mut not) = (0, 0);
            for round in 0..rounds {
                let mut touched = Vec::new();
                for _ in 0..1 + rng.below(4) {
                    let b = objects[rng.below(objects.len() as u64) as usize];
                    let o = &mut d[b * bs..(b + 1) * bs];
                    // Mostly in headers and near the start, where the fields are.
                    let at = match rng.below(3) {
                        0 => 8 + rng.below(0x100),
                        1 => 8 + rng.below(0x600),
                        _ => 8 + rng.below(bs as u64 - 16),
                    } as usize;
                    let width = [1, 2, 4, 8][rng.below(4) as usize].min(bs - at);
                    let old = u64::from_le_bytes(o[at..at + 8].try_into().unwrap_or([0; 8]));
                    let value: u64 = match rng.below(6) {
                        0 => 0,
                        1 => u64::MAX,
                        2 => rng.below(64),
                        3 => old.wrapping_add(rng.below(5)).wrapping_sub(2),
                        4 => 1 << rng.below(64),
                        _ => rng.next(),
                    };
                    o[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
                    seal(o);
                    touched.push(b);
                }
                for mode in [Mode::Copy, Mode::Estimate] {
                    let (layout, _) = run(&mut std::io::Cursor::new(&d), d.len() as u64, Opts::new(mode))
                        .unwrap_or_else(|e| panic!("round {round}: {e}"));
                    check_invariants(&layout);
                    if mode == Mode::Copy {
                        if layout.partitions[0].understood { still += 1 } else { not += 1 }
                    }
                }
                for b in touched {
                    d[b * bs..(b + 1) * bs].copy_from_slice(&clean[b * bs..(b + 1) * bs]);
                }
            }
            eprintln!("{size}: {still} rounds still understood, {not} not");
            assert!(not > 0 && (rounds < 100 || still > 0));
        }
    }

    #[test]
    fn truncated_apple_images_stay_conservative() {
        if !have(&HFS) {
            return;
        }
        let s = Scratch::new("apple-truncated");
        for (name, data) in apple_images(&s) {
            for cut in [512, 4096, 70_000, MB as usize + 1, 3 * MB as usize, 10 * MB as usize] {
                // A drive bigger than what can be read, and a file system bigger than its drive.
                for size in [data.len() as u64, cut as u64] {
                    if let Ok(layout) = analyze_bytes(&data[..cut], size) {
                        check_invariants(&layout);
                        let understood = layout.partitions.iter().filter(|p| p.understood).count();
                        assert_eq!(understood, 0, "{name} cut at {cut}: {:?}", layout.partitions);
                    }
                }
            }
        }
    }
}

/// Real images from hdiutil, with files: smart copies checked by macOS's own fsck, then
/// mounted read-only and compared file by file.
#[cfg(target_os = "macos")]
mod macos {
    use super::super::tests::Rng;
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    const MB: u64 = 1 << 20;

    /// A directory for one test's images, removed afterwards (unless `DD_GUI_SMART_KEEP`).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let base = std::env::var_os("DD_GUI_SMART_SCRATCH").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let dir = base.join(format!("dd-gui-apple-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if std::env::var_os("DD_GUI_SMART_KEEP").is_none() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn p(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    fn shell(cmd: &str, args: &[&str]) -> (bool, String) {
        match Command::new(cmd).args(args).stdin(Stdio::null()).output() {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
            ),
            Err(err) => (false, format!("{cmd}: {err}")),
        }
    }

    /// hdiutil, a few times over: it now and then fails with "Resource busy" on CI runners.
    fn hdiutil(args: &[&str]) -> Result<Vec<u8>, String> {
        let mut why = String::new();
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2));
            }
            match Command::new("hdiutil").args(args).stdin(Stdio::null()).output() {
                Ok(out) if out.status.success() => return Ok(out.stdout),
                Ok(out) => why = String::from_utf8_lossy(&out.stderr).into_owned(),
                Err(err) => return Err(format!("hdiutil: {err}")),
            }
        }
        Err(format!("hdiutil {args:?}: {why}"))
    }

    /// A disk image attached with hdiutil (its devices, their partition types, where
    /// volumes got mounted), detached on drop.
    struct Attached {
        disk: String,
        entities: Vec<(String, String, Option<PathBuf>)>,
    }

    impl Attached {
        fn new(args: &[&str]) -> Result<Attached, String> {
            let mut all = vec!["attach", "-plist", "-nobrowse", "-noautoopen", "-noverify"];
            all.extend(args);
            let out = hdiutil(&all)?;
            let plist = plist::Value::from_reader_xml(&out[..]).map_err(|e| format!("hdiutil's plist: {e}"))?;
            let list = plist.as_dictionary().and_then(|d| d.get("system-entities")).and_then(|v| v.as_array());
            let mut entities = Vec::new();
            for e in list.ok_or("hdiutil attached nothing")? {
                let Some(d) = e.as_dictionary() else { continue };
                let text = |key: &str| d.get(key).and_then(|v| v.as_string()).unwrap_or_default().to_owned();
                let mount = d.get("mount-point").and_then(|v| v.as_string()).map(PathBuf::from);
                entities.push((text("dev-entry"), text("content-hint"), mount));
            }
            // The image's own disk comes first: /dev/diskN, no slice.
            let whole =
                |dev: &str| dev.strip_prefix("/dev/disk").is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()));
            let disk = entities.iter().map(|e| e.0.clone()).find(|d| whole(d)).ok_or("no disk attached")?;
            Ok(Attached { disk, entities })
        }

        fn mount_point(&self) -> Option<PathBuf> {
            self.entities.iter().find_map(|e| e.2.clone())
        }

        /// The first device whose partition type is one of `hints`, else the image's first
        /// partition.
        fn device(&self, hints: &[&str]) -> Option<String> {
            let hinted = self.entities.iter().find(|e| hints.iter().any(|h| e.1.eq_ignore_ascii_case(h)));
            let first = || self.entities.iter().find(|e| e.0.starts_with(&format!("{}s", self.disk)));
            hinted.or_else(first).map(|e| e.0.clone())
        }
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            for attempt in 0..10 {
                let force = if attempt < 5 { None } else { Some("-force") };
                let args: Vec<&str> = ["detach", self.disk.as_str()].into_iter().chain(force).collect();
                if shell("hdiutil", &args).0 {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            eprintln!("couldn't detach {}", self.disk);
        }
    }

    type Tree = BTreeMap<String, Vec<u8>>;

    fn make_tree(rng: &mut Rng, tag: &str, count: usize, max: u64) -> Tree {
        (0..count)
            .map(|i| {
                let size = match rng.below(10) {
                    0..=2 => rng.below(600),
                    3..=5 => 600 + rng.below(20_000),
                    6..=8 => 20_000 + rng.below(300_000),
                    _ => rng.below(max),
                };
                let dir = if i % 4 == 0 { String::new() } else { format!("dir{}/", i % 4) };
                (format!("{dir}{tag}{i}.bin"), rng.bytes(size as usize))
            })
            .collect()
    }

    fn write_tree(tree: &Tree, dir: &Path) {
        for (name, data) in tree {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
    }

    /// The files under `dir`, but what the system keeps at the root of a volume (Spotlight,
    /// fseventsd, the HFS+ journal files…), all named with a dot.
    fn read_tree(dir: &Path, prefix: &str, out: &mut Tree) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                read_tree(&entry.path(), &format!("{prefix}{name}/"), out)?;
            } else {
                out.insert(format!("{prefix}{name}"), fs::read(entry.path()).map_err(|e| format!("{name}: {e}"))?);
            }
        }
        Ok(())
    }

    fn compare(expected: &Tree, dir: &Path) -> Result<(), String> {
        let mut found = Tree::new();
        read_tree(dir, "", &mut found)?;
        for (name, data) in expected {
            match found.get(name) {
                None => return Err(format!("{name} missing")),
                Some(d) if d != data => return Err(format!("{name} differs")),
                _ => {}
            }
        }
        match found.keys().find(|k| !expected.contains_key(*k)) {
            Some(extra) => Err(format!("unexpected file {extra}")),
            None => Ok(()),
        }
    }

    /// Only `extents` from `src`, 0xA5 everywhere else.
    fn smart_copy(src: &Path, extents: &[Extent], dst: &Path) {
        let size = fs::metadata(src).unwrap().len();
        let mut out = File::create(dst).unwrap();
        let fill = vec![0xA5u8; MB as usize];
        let mut left = size;
        while left > 0 {
            let n = left.min(MB);
            out.write_all(&fill[..n as usize]).unwrap();
            left -= n;
        }
        let mut input = File::open(src).unwrap();
        let mut buf = vec![0u8; 4 * MB as usize];
        for e in extents {
            let mut done = 0;
            while done < e.len {
                let n = (e.len - done).min(buf.len() as u64) as usize;
                input.seek(SeekFrom::Start(e.start + done)).unwrap();
                input.read_exact(&mut buf[..n]).unwrap();
                out.seek(SeekFrom::Start(e.start + done)).unwrap();
                out.write_all(&buf[..n]).unwrap();
                done += n as u64;
            }
        }
    }

    /// The layout, and why what's copied in full is.
    fn layout_of(img: &Path, mode: Mode) -> (Layout, Vec<String>) {
        let mut f = File::open(img).unwrap();
        let size = f.metadata().unwrap().len();
        let (layout, notes) = run(&mut f, size, Opts::new(mode)).unwrap();
        check_invariants(&layout);
        (layout, notes)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Apfs,
        Hfs,
    }

    impl Kind {
        /// How hdiutil names the partition's type, in the three partition schemes.
        fn hints(self) -> &'static [&'static str] {
            match self {
                Kind::Apfs => &["Apple_APFS", "7C3457EF-0000-11AA-AA11-00306543ECAC"],
                Kind::Hfs => &["Apple_HFS", "48465300-0000-11AA-AA11-00306543ECAC", "Apple_HFSX", "0xAF", "AF"],
            }
        }

        fn fs(self) -> &'static str {
            match self {
                Kind::Apfs => "apfs",
                Kind::Hfs => "hfsplus",
            }
        }
    }

    /// Root, without asking, when there is (CI runners); else as is.
    fn fsck(tool: &str, args: &[&str]) -> (bool, String) {
        if shell("sudo", &["-n", "true"]).0 {
            let mut all = vec!["-n", tool];
            all.extend(args);
            shell("sudo", &all)
        } else {
            shell(tool, args)
        }
    }

    /// fsck on the raw image's partition, then every file (mounted read-only).
    fn verify(raw: &Path, kind: Kind, files: &Tree, s: &Scratch) -> Result<(), String> {
        let class = "diskimage-class=CRawDiskImage";
        {
            let attached = Attached::new(&["-imagekey", class, "-readonly", "-nomount", p(raw)])?;
            let dev = attached.device(kind.hints()).ok_or_else(|| format!("no partition: {:?}", attached.entities))?;
            let rdev = dev.replacen("/dev/disk", "/dev/rdisk", 1);
            let (ok, text) = match kind {
                Kind::Apfs => fsck("fsck_apfs", &["-n", &rdev]),
                Kind::Hfs => fsck("fsck_hfs", &["-fn", &rdev]),
            };
            if !ok {
                return Err(format!("fsck: {text}"));
            }
        }
        let mnt = s.join("check-mnt");
        fs::create_dir_all(&mnt).unwrap();
        let attached = Attached::new(&["-imagekey", class, "-readonly", "-mountroot", p(&mnt), p(raw)])?;
        let dir = attached.mount_point().ok_or("not mounted")?;
        compare(files, &dir)
    }

    /// A fresh 200 MB image from hdiutil (`fs`: APFS, JHFS+…; `layout`: GPTSPUD, SPUD…),
    /// with files copied in and some deleted, then as a raw disk image.
    fn make_image(s: &Scratch, fs_name: &str, layout: &str, rng: &mut Rng) -> (PathBuf, Tree) {
        let dmg = s.join("image.dmg");
        hdiutil(&["create", "-size", "200m", "-fs", fs_name, "-layout", layout, "-volname", "SmartMac", p(&dmg)])
            .unwrap();
        let mut keep = make_tree(rng, "keep", 120, 3 * MB);
        let gone = make_tree(rng, "gone", 40, 3 * MB);
        {
            let mnt = s.join("fill-mnt");
            fs::create_dir_all(&mnt).unwrap();
            let attached = Attached::new(&["-mountroot", p(&mnt), p(&dmg)]).unwrap();
            let dir = attached.mount_point().expect("the volume wasn't mounted");
            write_tree(&keep, &dir);
            write_tree(&gone, &dir);
            for k in gone.keys() {
                fs::remove_file(dir.join(k)).unwrap();
            }
            // Two files grown in turns: fragmented.
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for _ in 0..40 {
                a.extend(rng.bytes(40_000));
                b.extend(rng.bytes(50_000));
                fs::write(dir.join("frag-a.bin"), &a).unwrap();
                fs::write(dir.join("frag-b.bin"), &b).unwrap();
            }
            keep.insert("frag-a.bin".into(), a);
            keep.insert("frag-b.bin".into(), b);
            shell("sync", &[]);
        }
        // Detached, so cleanly unmounted. Now the raw disk.
        let raw = s.join("image.cdr");
        hdiutil(&["convert", p(&dmg), "-format", "UDTO", "-o", p(&raw)]).unwrap();
        let raw = if raw.exists() { raw } else { s.join("image.cdr.cdr") };
        fs::remove_file(&dmg).unwrap();
        (raw, keep)
    }

    fn check_mac(name: &str, fs_name: &str, layout_name: &str, kind: Kind, table: Table) {
        if !shell("hdiutil", &["help"]).0 {
            eprintln!("skipped: no hdiutil");
            return;
        }
        let s = Scratch::new(name);
        let mut rng = Rng(name.bytes().fold(0x5EED, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b))));
        let (raw, files) = make_image(&s, fs_name, layout_name, &mut rng);
        let (layout, notes) = layout_of(&raw, Mode::Copy);
        eprintln!(
            "{name}: {:?}, {} of {} bytes in {} extents",
            layout.table,
            layout.used(),
            layout.size,
            layout.extents.len()
        );
        for part in &layout.partitions {
            eprintln!("  {part:?}");
        }
        for note in &notes {
            eprintln!("  note: {note}");
        }
        assert_eq!(layout.table, table);
        let part = layout.partitions.iter().find(|p| p.fs.as_deref() == Some(kind.fs())).expect("no such partition");
        assert!(part.understood, "{part:?}");
        assert_eq!(part.label.as_deref(), Some("SmartMac"));
        assert!(part.used < part.size / 2 + 10 * MB, "{part:?}");
        let (estimate, _) = layout_of(&raw, Mode::Estimate);
        assert!(estimate.partitions.iter().all(|p| p.understood || p.fs.as_deref() != Some(kind.fs())));
        let copy = s.join("copy.img");
        smart_copy(&raw, &layout.extents, &copy);
        verify(&copy, kind, &files, &s).unwrap_or_else(|e| panic!("{name}: smart copy broken: {e}"));
        // Negative control: without the biggest extent inside the file system, the checks
        // must fail (they'd be worthless otherwise).
        let (start, end) = (part.start, part.start + part.size);
        let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= end);
        if let Some(big) = inside.max_by_key(|e| e.len) {
            let fewer: Vec<Extent> = layout.extents.iter().filter(|e| *e != big).copied().collect();
            smart_copy(&raw, &fewer, &copy);
            assert!(verify(&copy, kind, &files, &s).is_err(), "{name}: dropping {big:?} went unnoticed");
        }
    }

    #[test]
    fn apfs_from_hdiutil() {
        check_mac("apfs", "APFS", "GPTSPUD", Kind::Apfs, Table::Gpt);
    }

    #[test]
    fn journaled_hfsplus_from_hdiutil() {
        check_mac("jhfs", "JHFS+", "GPTSPUD", Kind::Hfs, Table::Gpt);
    }

    #[test]
    fn hfsplus_in_an_apple_partition_map_from_hdiutil() {
        check_mac("jhfs-apm", "JHFS+", "SPUD", Kind::Hfs, partitions::APM);
    }

    #[test]
    fn hfsplus_in_an_mbr_from_hdiutil() {
        check_mac("hfs-mbr", "HFS+", "MBRSPUD", Kind::Hfs, Table::Mbr);
    }
}
