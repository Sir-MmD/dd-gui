//! Recognising what a partition (or a whole drive) holds, by signature.

use super::util::{Disk, at, be16_at, be32_at, u16_at, u32_at, u64_at};
use std::io;

/// Bytes read from the start of a partition: enough for every signature below.
pub(crate) const HEAD: usize = 128 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fs {
    Fat,
    Exfat,
    Ntfs,
    Ext,
    Btrfs,
    Xfs,
    F2fs,
    Lvm,
    Swap,
    Iso9660,
    Udf,
    Apfs,
    HfsPlus,
    /// Recognised by name only: copied in full.
    Other(&'static str),
}

impl Fs {
    /// Stale signatures of these often survive inside other file systems, so finding one
    /// next to another file system doesn't make a partition ambiguous.
    pub fn weak(self) -> bool {
        matches!(self, Fs::Swap | Fs::Iso9660)
    }
}

const MD_MAGIC: u32 = 0xA92B_4EFC;

/// Whether a partition's first bytes carry a signature.
type Test = fn(&[u8]) -> bool;

/// Signatures of file systems (and containers) that are recognised but not read, where
/// libblkid looks for them.
const OTHERS: &[(&str, Test)] = &[
    ("crypto_LUKS", |h| at(h, 0, b"LUKS\xba\xbe")),
    ("squashfs", |h| at(h, 0, b"hsqs")),
    // md RAID with its superblock at the start (metadata 1.1 and 1.2)
    ("linux_raid_member", |h| [0, 4096].iter().any(|&o| u32_at(h, o) == MD_MAGIC && u32_at(h, o + 4) == 1)),
    // Big-endian squashfs (version 3 and older)
    ("squashfs3", |h| at(h, 0, b"sqsh")),
    ("bcachefs", |h| at(h, 0x1018, BCACHEFS_MAGIC) || (at(h, 0x1018, BCACHE_MAGIC) && u64_at(h, 0x1068) == 8)),
    // A bcache backing or cache device (its superblock says it's in sector 8)
    ("bcache", |h| at(h, 0x1018, BCACHE_MAGIC) && u64_at(h, 0x1008) == 8),
    // The first ZFS vdev label's configuration (the others are looked for by `far`)
    ("zfs_member", |h| h.get(16 << 10..).is_some_and(zfs_label)),
    ("jfs", |h| at(h, 0x8000, b"JFS1")),
    ("reiserfs", |h| {
        [&b"ReIsErFs"[..], b"ReIsEr2Fs", b"ReIsEr3Fs"].iter().any(|m| at(h, 0x10034, m)) || at(h, 0x2034, b"ReIsErFs")
    }),
    ("reiser4", |h| at(h, 0x10000, b"ReIsEr4")),
    ("nilfs2", nilfs2),
    ("erofs", |h| u32_at(h, 1024) == 0xE0F5_E1E2),
    ("cramfs", |h| {
        let magic = |o| u32_at(h, o) == 0x28CD_3D45 || be32_at(h, o) == 0x28CD_3D45;
        [0, 512].iter().any(|&o| magic(o) && at(h, o + 16, b"Compressed ROMFS"))
    }),
    ("romfs", |h| at(h, 0, b"-rom1fs-")),
    ("minix", minix),
    ("ufs", |h| {
        const MAGICS: [u32; 6] = [0x0001_1954, 0x1954_0119, 0x0019_5612, 0x0009_5014, 0x0061_2195, 0x0523_1994];
        let magic = |o| MAGICS.contains(&u32_at(h, o)) || MAGICS.contains(&be32_at(h, o));
        [0, 8 << 10, 64 << 10].iter().any(|&o| magic(o + 1372))
    }),
    ("vxfs", |h| u32_at(h, 1024) == 0xA501_FCF5 || be32_at(h, 8192) == 0xA501_FCF5),
    ("gfs2", |h| gfs(h) == Some((1801, 1900))),
    ("gfs", |h| gfs(h) == Some((1309, 1401))),
    ("ocfs2", |h| [1, 2, 4, 8].iter().any(|&k| at(h, k << 10, b"OCFSV2"))),
    ("oracleasm", |h| at(h, 32, b"ORCLDISK")),
    ("hfs", super::hfsplus::classic),
    ("hpfs", |h| u32_at(h, 0x2000) == 0xF995_E849),
    ("ubifs", |h| u32_at(h, 0) == 0x0610_1831),
    ("ubi", |h| at(h, 0, b"UBI#")),
    ("bfs", |h| u32_at(h, 0) == 0x1BAD_FACE),
    ("zonefs", |h| u32_at(h, 0) == 0x5A4E_4653),
    ("LVM1_member", |h| at(h, 0, b"HM") && matches!(u16_at(h, 2), 1 | 2)),
    ("DM_integrity", |h| at(h, 0, b"integrt")),
    ("DM_verity_hash", |h| at(h, 0, b"verity\0\0")),
    ("DM_snapshot_cow", |h| at(h, 0, b"SnAp")),
    ("vdo", |h| at(h, 0, b"dmvdo001")),
    ("stratis", |h| at(h, 0x204, b"!Stra0tis\x86\xff\x02^\x41rh") || at(h, 0x1204, b"!Stra0tis\x86\xff\x02^\x41rh")),
    ("ceph_bluestore", |h| at(h, 0, b"bluestore block device")),
];

const BCACHE_MAGIC: &[u8] = b"\xc6\x85\x73\xf6\x4e\x1a\x45\xca\x82\x65\xf5\x7f\x48\xba\x6d\x81";
const BCACHEFS_MAGIC: &[u8] = b"\xc6\x85\x73\xf6\x66\xce\x90\xa9\xd9\x6a\x60\xcf\x80\x3d\xf7\xef";

/// The configuration in a ZFS vdev label: an XDR-encoded list of names and values, whose
/// first name (usually "version") follows the header.
fn zfs_label(b: &[u8]) -> bool {
    let header = matches!(b.get(..4), Some([1, 0 | 1, 0, 0]));
    let name = be32_at(b, 20) as usize;
    header
        && be32_at(b, 4) == 0
        && be32_at(b, 8) <= 3
        && be32_at(b, 12) != 0
        && (1..=64).contains(&name)
        && b.get(24..24 + name).is_some_and(|n| n.iter().all(|&c| c.is_ascii_lowercase() || c == b'_'))
}

/// A NILFS2 superblock at 1 KiB, with its CRC.
fn nilfs2(h: &[u8]) -> bool {
    let Some(sb) = h.get(1024..2048) else {
        return false;
    };
    let bytes = usize::from(u16_at(sb, 8));
    if u16_at(sb, 6) != 0x3434 || !(20..=1024).contains(&bytes) {
        return false;
    }
    let crc = super::util::crc32_le(u32_at(sb, 12), &sb[..16]);
    super::util::crc32_le(super::util::crc32_le(crc, &[0; 4]), &sb[20..bytes]) == u32_at(sb, 16)
}

/// A Minix superblock (versions 1 to 3, either byte order) at 1 KiB that makes sense, the
/// way libblkid checks it: its 2-byte magic alone turns up in ext superblocks.
fn minix(h: &[u8]) -> bool {
    if h.len() < 2048 || u16_at(h, 1024 + 0x38) == 0xEF53 {
        return false;
    }
    let sb = &h[1024..];
    [false, true].iter().any(|&big| {
        let u16 = |at| if big { be16_at(sb, at) } else { u16_at(sb, at) };
        let u32 = |at| if big { be32_at(sb, at) } else { u32_at(sb, at) };
        let (inodes, imaps, zmaps, first, log_zone, zones, block) = match (u16(16), u16(24)) {
            (0x137F | 0x138F, _) => (u32::from(u16(0)), u16(4), u16(6), u16(8), u16(10), u32::from(u16(2)), 1024),
            (0x2468 | 0x2478, _) => (u32::from(u16(0)), u16(4), u16(6), u16(8), u16(10), u32(20), 1024),
            (_, 0x4D5A) => (u32(0), u16(6), u16(8), u16(10), u16(12), u32(20), u32::from(u16(28))),
            _ => return false,
        };
        let bits = |maps: u16| u64::from(maps) * u64::from(block) * 8;
        let first = u32::from(first);
        log_zone == 0
            && inodes != 0
            && inodes != u32::MAX
            && block.is_power_of_two()
            && (512..=65536).contains(&block)
            && bits(imaps) > u64::from(inodes)
            && first <= zones
            && bits(zmaps) > u64::from(zones - first)
    })
}

/// GFS or GFS2 at 64 KiB: its (file system, multihost) format numbers.
fn gfs(h: &[u8]) -> Option<(u32, u32)> {
    (be32_at(h, 0x10000) == 0x0116_1970 && be32_at(h, 0x10004) == 1).then(|| (be32_at(h, 0x10018), be32_at(h, 0x1001C)))
}

/// Signatures past `HEAD` that tell what a partition holds, whatever its first bytes say:
/// VMFS (at 1 and 2 MiB), and the ZFS vdev labels after the first (at 256 KiB, and 512 and
/// 256 KiB before the end of the device rounded down to 256 KiB).
pub(crate) fn far(disk: &mut Disk, start: u64, size: u64) -> io::Result<Option<&'static str>> {
    let mut buf = [0u8; 32];
    let read = |disk: &mut Disk, at: u64, buf: &mut [u8]| -> io::Result<bool> {
        if at.saturating_add(buf.len() as u64) > size {
            return Ok(false);
        }
        disk.read_at(start + at, buf)?;
        Ok(true)
    };
    if read(disk, 1 << 20, &mut buf[..4])? && u32_at(&buf, 0) == 0xC001_D00D {
        return Ok(Some("VMFS_volume_member"));
    }
    if read(disk, 2 << 20, &mut buf[..4])? && u32_at(&buf, 0) == 0x2FAB_F15E {
        return Ok(Some("VMFS"));
    }
    let label = 256 << 10;
    let end = size / label * label;
    for at in [label, end.saturating_sub(2 * label), end.saturating_sub(label)] {
        if at >= label && read(disk, at + (16 << 10), &mut buf)? && zfs_label(&buf) {
            return Ok(Some("zfs_member"));
        }
    }
    Ok(None)
}

/// File systems with their own module: each knows its signature.
const OWN: &[(Fs, Test)] = &[
    (Fs::Btrfs, super::btrfs::detect),
    (Fs::Xfs, super::xfs::detect),
    (Fs::F2fs, super::f2fs::detect),
    (Fs::Lvm, super::lvm::detect),
    (Fs::Swap, super::swap::detect),
    (Fs::Iso9660, super::iso::detect),
    (Fs::Udf, super::udf::detect),
    (Fs::Apfs, super::apfs::detect),
    (Fs::HfsPlus, super::hfsplus::detect),
];

/// Every signature found in `head` (the first bytes of a partition), the ones DD-GUI can
/// read first. More than one non-weak signature means the partition is ambiguous.
pub(crate) fn detect(head: &[u8]) -> Vec<Fs> {
    let mut found = Vec::new();
    found.extend(boot_sector(head));
    if ext(head) {
        found.push(Fs::Ext);
    }
    found.extend(OWN.iter().filter(|(_, test)| test(head)).map(|&(fs, _)| fs));
    found.extend(OTHERS.iter().filter(|(_, test)| test(head)).map(|&(name, _)| Fs::Other(name)));
    found
}

/// A file system whose first sector is a boot sector with a BIOS parameter block.
pub(crate) fn boot_sector(b: &[u8]) -> Option<Fs> {
    if at(b, 3, b"EXFAT   ") {
        Some(Fs::Exfat)
    } else if at(b, 3, b"NTFS    ") {
        Some(Fs::Ntfs)
    } else if at(b, 3, b"-FVE-FS-") {
        // BitLocker keeps a FAT-like header: never read it as FAT.
        Some(Fs::Other("BitLocker"))
    } else if at(b, 3, b"ReFS\0\0\0\0") {
        Some(Fs::Other("ReFS"))
    } else if fat(b) {
        Some(Fs::Fat)
    } else {
        None
    }
}

/// A FAT boot sector, checked the way libblkid does.
fn fat(b: &[u8]) -> bool {
    if b.len() < 512 {
        return false;
    }
    let named = at(b, 0x52, b"MSWIN")
        || at(b, 0x52, b"FAT32   ")
        || at(b, 0x36, b"MSDOS")
        || at(b, 0x36, b"FAT16   ")
        || at(b, 0x36, b"FAT12   ")
        || at(b, 0x36, b"FAT     ");
    let signed = b[510] == 0x55 && b[511] == 0xAA;
    if !named && !(signed && matches!(b[0], 0xEB | 0xE9 | 0x90)) {
        return false;
    }
    // OS/2 puts FAT-like headers on JFS and HPFS.
    if at(b, 0x36, b"JFS     ") || at(b, 0x36, b"HPFS    ") {
        return false;
    }
    let bps = u64::from(u16_at(b, 11));
    let spc = u64::from(b[13]);
    let reserved = u64::from(u16_at(b, 14));
    let fats = u64::from(b[16]);
    let media = b[21];
    if fats == 0 || reserved == 0 || !(media == 0xF0 || media >= 0xF8) || !spc.is_power_of_two() {
        return false;
    }
    if !bps.is_power_of_two() || !(512..=4096).contains(&bps) {
        return false;
    }
    let sectors = match u16_at(b, 19) {
        0 => u64::from(u32_at(b, 32)),
        n => u64::from(n),
    };
    let fat16 = u64::from(u16_at(b, 22));
    let fat_len = if fat16 != 0 { fat16 } else { u64::from(u32_at(b, 36)) };
    let root = (u64::from(u16_at(b, 17)) * 32).div_ceil(bps);
    let meta = reserved + fats * fat_len + root;
    fat_len != 0 && sectors > meta
}

/// An ext2/3/4 superblock (or an ext journal device) at 1024.
fn ext(b: &[u8]) -> bool {
    u16_at(b, 1024 + 0x38) == 0xEF53
        && u32_at(b, 1024 + 0x18) <= 6
        && u32_at(b, 1024 + 0x20) != 0
        && u32_at(b, 1024 + 0x28) != 0
}

/// Where an md RAID superblock sits at the end of `start..start + size` (metadata 0.90 and
/// 1.0), if there is one. What the start of such a device holds describes the array, so it
/// can only be read as is for mirrors: not worth guessing, it's copied in full.
pub(crate) fn md_at_end(disk: &mut Disk, start: u64, size: u64) -> io::Result<Option<u64>> {
    let sectors = size / 512;
    let mut buf = [0u8; 16];
    // 0.90: 64 KiB from the end, in a 64 KiB aligned block, in host byte order
    if sectors >= 256 {
        let pos = start + ((sectors & !127) - 128) * 512;
        disk.read_at(pos, &mut buf)?;
        let le = u32_at(&buf, 0) == MD_MAGIC && u32_at(&buf, 4) == 0 && u32_at(&buf, 8) == 90;
        let be = be32_at(&buf, 0) == MD_MAGIC && be32_at(&buf, 4) == 0 && be32_at(&buf, 8) == 90;
        if le || be {
            return Ok(Some(pos));
        }
    }
    // 1.0: 8 KiB from the end, 4 KiB aligned
    if sectors >= 32 {
        let pos = start + ((sectors - 16) & !7) * 512;
        disk.read_at(pos, &mut buf)?;
        if u32_at(&buf, 0) == MD_MAGIC && u32_at(&buf, 4) == 1 {
            return Ok(Some(pos));
        }
    }
    Ok(None)
}

/// Firmware ("fake") RAID metadata at the end of the drive: Intel Matrix (IMSM) or SNIA DDF.
/// The partition table then describes the array, not this drive.
pub(crate) fn fake_raid(disk: &mut Disk) -> io::Result<Option<&'static str>> {
    let sectors = disk.size() / 512;
    if sectors < 4 {
        return Ok(None);
    }
    let mut buf = [0u8; 32];
    disk.read_at((sectors - 2) * 512, &mut buf)?;
    if at(&buf, 0, b"Intel Raid ISM Cfg Sig. ") {
        return Ok(Some("isw_raid_member"));
    }
    disk.read_at((sectors - 1) * 512, &mut buf)?;
    if be32_at(&buf, 0) == 0xDE11_DE11 {
        return Ok(Some("ddf_raid_member"));
    }
    Ok(None)
}

/// For an ISO 9660 file system at the start of the drive (isohybrid images): its size, or
/// `Some(None)` when it can't be told.
pub(crate) fn iso_size(head: &[u8]) -> Option<Option<u64>> {
    if !at(head, 0x8001, b"CD001") {
        return None;
    }
    // The primary volume descriptor comes first.
    let blocks = u64::from(u32_at(head, 0x8050));
    let block = u64::from(u16_at(head, 0x8080));
    let ok = head.get(0x8000) == Some(&1) && blocks > 0 && block.is_power_of_two() && (512..=2048).contains(&block);
    Some(ok.then_some(blocks * block))
}
