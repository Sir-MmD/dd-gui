//! Smart copy: find which parts of a drive actually hold data.
//!
//! `analyze` reads the partition table (MBR with its logical partitions and BSD disklabels,
//! GPT, Apple partition map) and the allocation maps of what it understands:
//! - FAT12/16/32, exFAT, NTFS, ext2/3/4;
//! - btrfs (on one device), XFS (v4 and v5), F2FS;
//! - HFS+ and HFSX, APFS;
//! - LVM2 physical volumes: plain logical volumes are read like partitions of their own,
//!   the others (thin pools, RAID, snapshots, LVs on several PVs…) keep all their extents;
//! - Linux swap (its header only), ISO 9660 (its volume only), UDF.
//!
//! Anything it doesn't understand counts as used, so a smart copy never drops data a file
//! system needs; it only skips free space. Many more file systems and containers are
//! recognised by name (LUKS, BitLocker, ZFS, bcachefs, JFS, ReiserFS, VMFS…) and copied in
//! full.
//!
//! Always copied: the first and last MiB of the drive (boot code, partition tables, the
//! backup GPT, RAID metadata), every EBR, the first 64 KiB of each partition, and whatever
//! lies between the end of a file system and the end of its partition. A file system that
//! looks inconsistent, ambiguous, or not cleanly unmounted (a journal or log to replay, a
//! swap area holding a hibernation image) is copied in full.

mod apfs;
mod btrfs;
mod exfat;
mod ext;
mod f2fs;
mod fat;
mod hfsplus;
mod iso;
mod lvm;
mod ntfs;
mod partitions;
mod probe;
mod swap;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_apple;
#[cfg(test)]
mod tests_linux;
mod udf;
mod util;
mod xfs;

use probe::Fs;
use serde::Serialize;
use std::io::{self, Read, Seek};
use util::{Bad, Disk, MIB, Mode, Opts, Part};

/// A byte range on the drive: `start..start + len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Extent {
    pub start: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Table {
    None,
    Mbr,
    Gpt,
    /// An Apple partition map.
    Apm,
}

#[derive(Clone, Debug, Serialize)]
pub struct Partition {
    /// Numbered like the OS does (sdb1 is 1).
    pub index: u32,
    pub start: u64,
    pub size: u64,
    /// "vfat", "exfat", "ntfs", "ext4"… For file systems we can't read, whatever
    /// was recognised ("btrfs", "crypto_LUKS"…), or None when unknown.
    pub fs: Option<String>,
    pub label: Option<String>,
    /// Bytes a smart copy reads from this partition.
    pub used: u64,
    /// False when the file system isn't understood and gets copied in full.
    pub understood: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Layout {
    /// Size of the whole drive.
    pub size: u64,
    pub table: Table,
    pub partitions: Vec<Partition>,
    /// What a smart copy reads: sorted, non-overlapping, merged, 4 KiB aligned
    /// (clamped to `size`). Includes partition tables, boot areas and anything
    /// outside partitions that might matter.
    pub extents: Vec<Extent>,
}

impl Layout {
    /// Total bytes in `extents`. (The GUI sums the same from the `@layout` JSON.)
    #[allow(dead_code)]
    pub fn used(&self) -> u64 {
        self.extents.iter().map(|e| e.len).sum()
    }

    /// Merges the closest extents until at most `max` are left (for formats that can only
    /// list so many), and updates `Partition::used` to match. Nothing is ever dropped.
    #[allow(dead_code)]
    pub fn limit_extents(&mut self, max: usize) {
        let mut gap = util::GAP;
        while self.extents.len() > max.max(1) {
            gap = gap.saturating_mul(2);
            self.extents = util::normalize(std::mem::take(&mut self.extents), self.size, gap, 1);
        }
        for p in &mut self.partitions {
            p.used = util::overlap(&self.extents, p.start, p.size);
        }
    }
}

/// Maps a drive (or a raw disk image) of `size` bytes.
pub fn analyze<R: Read + Seek>(dev: &mut R, size: u64) -> io::Result<Layout> {
    run(dev, size, Opts::new(Mode::Copy)).map(|(layout, _)| layout)
}

/// Like `analyze`, for a preview while the drive's file systems may still be mounted:
/// allocation maps are trusted even when dirty or with a journal left to replay, where
/// `analyze` copies such file systems in full. Never copy by it: once everything is
/// unmounted, `analyze` again.
pub fn estimate<R: Read + Seek>(dev: &mut R, size: u64) -> io::Result<Layout> {
    run(dev, size, Opts::new(Mode::Estimate)).map(|(layout, _)| layout)
}

/// The analysis, plus why whatever gets copied in full does.
fn run<R: Read + Seek>(dev: &mut R, size: u64, opts: Opts) -> io::Result<(Layout, Vec<String>)> {
    let mut disk = Disk::new(dev, size);
    let scheme = partitions::read(&mut disk)?;
    let mut notes = Vec::new();
    // Boot code and partition tables at the start; the backup GPT and RAID metadata at the end.
    let edge = MIB.min(size);
    let mut all = vec![Extent { start: 0, len: edge }, Extent { start: size - edge, len: edge }];
    all.extend(&scheme.fixed);
    let mut partitions = Vec::new();
    for slot in &scheme.slots {
        let found = partition(&mut disk, slot, opts);
        if let Some(why) = found.why {
            notes.push(format!("partition {}: {why}", slot.index));
        }
        all.extend(found.used);
        partitions.push(Partition {
            index: slot.index,
            start: slot.start,
            size: slot.size,
            fs: found.fs,
            label: found.label,
            used: 0,
            understood: found.understood && scheme.whole.is_none(),
        });
    }
    if let Some(why) = scheme.whole {
        notes.push(format!("whole drive: {why}"));
        all.push(Extent { start: 0, len: size });
    }
    let extents = util::normalize(all, size, opts.gap, opts.align);
    for p in &mut partitions {
        p.used = util::overlap(&extents, p.start, p.size);
    }
    Ok((Layout { size, table: scheme.table, partitions, extents }, notes))
}

/// Bytes at the start of an understood partition that are copied regardless: boot sectors,
/// boot loaders installed there, the ext boot block.
const HEAD_KEPT: u64 = 64 * 1024;

struct Found {
    fs: Option<String>,
    label: Option<String>,
    understood: bool,
    /// Absolute.
    used: Vec<Extent>,
    /// Why it's copied in full.
    why: Option<String>,
}

fn fs_name(kind: Fs, head: &[u8]) -> &'static str {
    match kind {
        Fs::Fat => "vfat",
        Fs::Exfat => "exfat",
        Fs::Ntfs => "ntfs",
        Fs::Ext => ext::name(head),
        Fs::Btrfs => "btrfs",
        Fs::Xfs => "xfs",
        Fs::F2fs => "f2fs",
        Fs::Lvm => "LVM2_member",
        Fs::Swap => "swap",
        Fs::Iso9660 => "iso9660",
        Fs::Udf => "udf",
        Fs::Apfs => "apfs",
        Fs::HfsPlus => "hfsplus",
        Fs::Other(name) => name,
    }
}

fn partition(disk: &mut Disk, slot: &partitions::Slot, opts: Opts) -> Found {
    let (start, size) = (slot.start, slot.size);
    let in_full = |fs: Option<&str>, label: Option<String>, why: String| Found {
        fs: fs.map(str::to_owned),
        label,
        understood: false,
        used: vec![Extent { start, len: size }],
        why: Some(why),
    };
    let head = match disk.read_vec(start, size.min(probe::HEAD as u64) as usize) {
        Ok(head) => head,
        Err(err) => return in_full(None, None, format!("unreadable: {err}")),
    };
    let found = probe::detect(&head);
    // What lies further in says more than the first bytes (which may be stale).
    match probe::far(disk, start, size) {
        Ok(None) => {}
        Ok(Some(far)) => return in_full(Some(far), None, format!("{far} signature past the start")),
        Err(err) => return in_full(None, None, format!("unreadable: {err}")),
    }
    let strong: Vec<Fs> = found.iter().copied().filter(|f| !f.weak()).collect();
    let Some(&kind) = strong.first().or(found.first()) else {
        return in_full(None, None, "nothing recognised".into());
    };
    let name = fs_name(kind, &head);
    if strong.len() > 1 {
        return in_full(Some(name), None, format!("ambiguous, several signatures: {strong:?}"));
    }
    if let Fs::Other(_) = kind {
        return in_full(Some(name), None, "not a file system DD-GUI reads".into());
    }
    match probe::md_at_end(disk, start, size) {
        Ok(None) => {}
        Ok(Some(_)) => return in_full(Some("linux_raid_member"), None, "md RAID member".into()),
        Err(err) => return in_full(Some(name), None, format!("unreadable: {err}")),
    }
    let mut label = None;
    let mut part = Part::new(disk, start, size, opts);
    let result = match kind {
        Fs::Fat => fat::analyze(&mut part, &head, &mut label),
        Fs::Exfat => exfat::analyze(&mut part, &head, &mut label),
        Fs::Ntfs => ntfs::analyze(&mut part, &head, &mut label),
        Fs::Ext => ext::analyze(&mut part, &head, &mut label),
        Fs::Btrfs => btrfs::analyze(&mut part, &head, &mut label),
        Fs::Xfs => xfs::analyze(&mut part, &head, &mut label),
        Fs::F2fs => f2fs::analyze(&mut part, &head, &mut label),
        Fs::Lvm => lvm::analyze(&mut part, &head, &mut label),
        Fs::Swap => swap::analyze(&mut part, &head, &mut label),
        Fs::Iso9660 => iso::analyze(&mut part, &head, &mut label),
        Fs::Udf => udf::analyze(&mut part, &head, &mut label),
        Fs::Apfs => apfs::analyze(&mut part, &head, &mut label),
        Fs::HfsPlus => hfsplus::analyze(&mut part, &head, &mut label),
        Fs::Other(_) => Err(Bad("not a file system DD-GUI reads".into())),
    };
    let usage = match result {
        Ok(usage) => usage,
        Err(Bad(why)) => return in_full(Some(name), label, why),
    };
    let end = usage.end.min(size);
    let mut used: Vec<Extent> = usage
        .used
        .into_vec()
        .into_iter()
        .filter_map(|e| {
            let (from, to) = (e.start.min(size), e.start.saturating_add(e.len).min(size));
            (from < to).then(|| Extent { start: start + from, len: to - from })
        })
        .collect();
    used.push(Extent { start, len: HEAD_KEPT.min(size) });
    used.push(Extent { start: start + end, len: size - end });
    Found { fs: Some(name.into()), label, understood: true, used, why: None }
}
