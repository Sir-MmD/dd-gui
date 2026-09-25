//! Recognising what a partition (or a whole drive) holds, by signature.

use super::util::{Disk, at, be32_at, u16_at, u32_at};
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

/// Signatures of file systems (and containers) that are recognised but not read.
const OTHERS: &[(&str, Test)] = &[
    ("crypto_LUKS", |h| at(h, 0, b"LUKS\xba\xbe")),
    ("squashfs", |h| at(h, 0, b"hsqs")),
    // md RAID with its superblock at the start (metadata 1.1 and 1.2)
    ("linux_raid_member", |h| [0, 4096].iter().any(|&o| u32_at(h, o) == MD_MAGIC && u32_at(h, o + 4) == 1)),
];

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
