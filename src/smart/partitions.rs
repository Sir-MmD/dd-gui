//! Partition tables: MBR (with logical partitions in extended ones), GPT, or none at all.

use super::probe;
use super::util::{Disk, at, crc32, u32_at, u64_at};
use super::{Extent, Table};
use std::io;

/// A partition to look into.
pub(crate) struct Slot {
    /// Numbered like the OS does (0 for a file system on the whole drive).
    pub index: u32,
    pub start: u64,
    pub size: u64,
}

pub(crate) struct Scheme {
    pub table: Table,
    pub slots: Vec<Slot>,
    /// Copied whatever the partitions hold: tables, EBRs, GPT copies, isohybrid images.
    pub fixed: Vec<Extent>,
    /// Why nothing on the drive can be trusted to be free (fake RAID, ambiguous layouts),
    /// so all of it gets copied.
    pub whole: Option<&'static str>,
}

impl Scheme {
    fn new(table: Table) -> Self {
        Scheme { table, slots: Vec::new(), fixed: Vec::new(), whole: None }
    }

    /// No partition table: a file system, or something unknown, starting at 0.
    fn floppy(size: u64) -> Self {
        let mut scheme = Scheme::new(Table::None);
        scheme.slots.push(Slot { index: 0, start: 0, size });
        scheme
    }
}

#[derive(Clone, Copy)]
struct Entry {
    boot: u8,
    kind: u8,
    /// In logical sectors, like `sectors`.
    start: u64,
    sectors: u64,
}

impl Entry {
    fn used(&self) -> bool {
        self.sectors > 0
    }

    fn extended(&self) -> bool {
        matches!(self.kind, 0x05 | 0x0F | 0x85)
    }

    /// Byte range for sectors of `ss` bytes, clamped to the drive; None when it starts past the end.
    fn bytes(&self, first: u64, ss: u64, size: u64) -> Option<(u64, u64)> {
        let start = first.checked_mul(ss).filter(|&start| start < size)?;
        let end = first.saturating_add(self.sectors).saturating_mul(ss).min(size);
        Some((start, end - start))
    }
}

/// The four entries of an MBR or EBR sector.
fn entries(sector: &[u8]) -> [Entry; 4] {
    std::array::from_fn(|i| {
        let e = 0x1BE + 16 * i;
        Entry {
            boot: sector.get(e).copied().unwrap_or(0),
            kind: sector.get(e + 4).copied().unwrap_or(0),
            start: u64::from(u32_at(sector, e + 8)),
            sectors: u64::from(u32_at(sector, e + 12)),
        }
    })
}

/// Logical partitions followed at most (Linux stops at a similar count).
const MAX_LOGICAL: usize = 256;
/// Largest GPT entry array read (the usual one is 16 KiB).
const MAX_GPT_ENTRIES: u64 = 4 << 20;

pub(crate) fn read(disk: &mut Disk) -> io::Result<Scheme> {
    let size = disk.size();
    let head = disk.read_vec(0, size.min(probe::HEAD as u64) as usize)?;
    let mut scheme = scan(disk, &head)?;
    if let Some(kind) = probe::fake_raid(disk)? {
        scheme.whole = Some(kind);
    }
    // md metadata at the end of the drive, outside any partition: the table describes an array.
    if let Some(pos) = probe::md_at_end(disk, 0, size)?
        && !scheme.slots.iter().any(|s| s.start <= pos && pos - s.start < s.size)
    {
        scheme.whole = Some("linux_raid_member");
    }
    // isohybrid: an ISO 9660 image spans the start of the drive, across the partitions.
    if scheme.table != Table::None {
        match probe::iso_size(&head) {
            Some(Some(len)) => scheme.fixed.push(Extent { start: 0, len: len.min(size) }),
            Some(None) => scheme.whole = Some("ISO 9660 image of unknown size"),
            None => {}
        }
    }
    Ok(scheme)
}

fn scan(disk: &mut Disk, head: &[u8]) -> io::Result<Scheme> {
    let size = disk.size();
    let Some(mbr) = head.get(..512) else { return Ok(Scheme::floppy(size)) };
    let signed = at(mbr, 510, &[0x55, 0xAA]);
    let e = entries(mbr);
    // Without a valid GPT after all, the 0xEE entry is copied in full as an unknown partition.
    if signed
        && e.iter().any(|x| x.kind == 0xEE)
        && let Some(gpt) = gpt(disk, &e)
    {
        return Ok(gpt);
    }
    let valid = signed && e.iter().all(|x| x.boot == 0 || x.boot == 0x80);
    let partitions = valid && e.iter().any(|x| x.used() && x.start.checked_mul(512).is_some_and(|p| p < size));
    let boot_sector = probe::boot_sector(mbr).is_some();
    Ok(match (boot_sector, partitions) {
        (true, true) => {
            let mut scheme = Scheme::floppy(size);
            scheme.whole = Some("both a boot sector and a partition table");
            scheme
        }
        (false, true) => mbr_table(disk, &e),
        (false, false) if valid && probe::detect(head).is_empty() => Scheme::new(Table::Mbr),
        _ => Scheme::floppy(size),
    })
}

fn mbr_table(disk: &mut Disk, e: &[Entry; 4]) -> Scheme {
    let size = disk.size();
    let (ss, unsure) = sector_size(disk, e);
    let mut scheme = Scheme::new(Table::Mbr);
    let mut next = 5;
    for (i, x) in e.iter().enumerate().filter(|(_, x)| x.used()) {
        let Some((start, len)) = x.bytes(x.start, ss, size) else { continue };
        if x.extended() {
            logical(disk, x, ss, &mut scheme, &mut next);
        } else {
            scheme.slots.push(Slot { index: i as u32 + 1, start, size: len });
        }
    }
    // Like Linux lists them: primary partitions, then logical ones.
    scheme.slots.sort_by_key(|s| s.index);
    if unsure {
        // Nothing recognisable either way: this may be a drive with 4096-byte sectors, so
        // whatever the partitions would cover then gets copied too.
        scheme.fixed.extend(
            e.iter()
                .filter(|x| x.used())
                .filter_map(|x| x.bytes(x.start, 4096, size))
                .map(|(start, len)| Extent { start, len }),
        );
    }
    scheme
}

/// MBR entries count logical sectors, which may be 4096 bytes. Picks the size under which
/// file systems (or EBRs) show up where the entries point. The flag is set when neither
/// does and 4096-byte sectors would fit the drive.
fn sector_size(disk: &mut Disk, e: &[Entry; 4]) -> (u64, bool) {
    let small = evidence(disk, e, 512);
    let large = evidence(disk, e, 4096);
    if large > small {
        return (4096, false);
    }
    let size = disk.size();
    let fits = e.iter().filter(|x| x.used()).all(|x| x.start.checked_mul(4096).is_some_and(|p| p < size));
    (512, small == 0 && fits)
}

fn evidence(disk: &mut Disk, e: &[Entry; 4], ss: u64) -> usize {
    let size = disk.size();
    let mut found = 0;
    for x in e.iter().filter(|x| x.used()) {
        let Some((pos, len)) = x.bytes(x.start, ss, size) else { continue };
        let len = len.min(if x.extended() { 512 } else { probe::HEAD as u64 }) as usize;
        let Ok(head) = disk.read_vec(pos, len) else { continue };
        if (x.extended() && at(&head, 510, &[0x55, 0xAA])) || (!x.extended() && !probe::detect(&head).is_empty()) {
            found += 1;
        }
    }
    found
}

/// Follows the chain of EBRs in an extended partition, the way Linux does.
fn logical(disk: &mut Disk, ext: &Entry, ss: u64, scheme: &mut Scheme, next: &mut u32) {
    let size = disk.size();
    let ext_end = ext.start.saturating_add(ext.sectors);
    let (mut this, mut this_sectors) = (ext.start, ext.sectors);
    let mut seen = Vec::new();
    while seen.len() < MAX_LOGICAL && !seen.contains(&this) {
        seen.push(this);
        let Some(pos) = this.checked_mul(ss).filter(|p| p.checked_add(512).is_some_and(|end| end <= size)) else {
            return;
        };
        let mut sector = [0u8; 512];
        if disk.read_at(pos, &mut sector).is_err() {
            // What's left of the extended partition can't be told apart: keep all of it.
            let end = ext_end.saturating_mul(ss).min(size);
            scheme.fixed.push(Extent { start: pos, len: end.saturating_sub(pos) });
            return;
        }
        scheme.fixed.push(Extent { start: pos, len: ss });
        if !at(&sector, 510, &[0x55, 0xAA]) {
            return;
        }
        let e = entries(&sector);
        for (i, x) in e.iter().enumerate().filter(|(_, x)| x.used() && !x.extended()) {
            let first = this.saturating_add(x.start);
            // The 3rd and 4th entries often hold garbage: only trusted when they fit.
            let fits = x.start.saturating_add(x.sectors) <= this_sectors
                && first >= ext.start
                && first.saturating_add(x.sectors) <= ext_end;
            if i >= 2 && !fits {
                continue;
            }
            if let Some((start, len)) = x.bytes(first, ss, size) {
                scheme.slots.push(Slot { index: *next, start, size: len });
                *next += 1;
            }
        }
        match e.iter().find(|x| x.used() && x.extended()) {
            Some(link) => (this, this_sectors) = (ext.start.saturating_add(link.start), link.sectors),
            None => return,
        }
    }
}

/// A valid GPT header (with its entries) at `lba`, for `ss`-byte sectors.
struct Gpt {
    ss: u64,
    lba: u64,
    header: [u8; 512],
    entries: Vec<u8>,
}

impl Gpt {
    fn entry_size(&self) -> u64 {
        u64::from(u32_at(&self.header, 84))
    }

    fn entries_at(&self) -> u64 {
        u64_at(&self.header, 72)
    }

    fn entries_len(&self) -> u64 {
        self.entries.len() as u64
    }
}

fn gpt_at(disk: &mut Disk, ss: u64, lba: u64) -> Option<Gpt> {
    let size = disk.size();
    let mut header = [0u8; 512];
    disk.read_at(lba.checked_mul(ss)?, &mut header).ok()?;
    let header_size = u32_at(&header, 12) as usize;
    if !at(&header, 0, b"EFI PART") || !(92..=512).contains(&header_size) {
        return None;
    }
    let mut zeroed = header;
    zeroed[16..20].fill(0);
    let sectors = size / ss;
    let (first, last) = (u64_at(&header, 40), u64_at(&header, 48));
    if crc32(&zeroed[..header_size]) != u32_at(&header, 16)
        || u64_at(&header, 24) != lba
        || first > last
        || last >= sectors
    {
        return None;
    }
    let count = u64::from(u32_at(&header, 80));
    let entry_size = u64::from(u32_at(&header, 84));
    if entry_size < 128 || entry_size % 128 != 0 || !(entry_size / 128).is_power_of_two() {
        return None;
    }
    let len = count.checked_mul(entry_size).filter(|&len| len <= MAX_GPT_ENTRIES)?;
    let entries = disk.read_vec(u64_at(&header, 72).checked_mul(ss)?, len as usize).ok()?;
    (crc32(&entries) == u32_at(&header, 88)).then_some(Gpt { ss, lba, header, entries })
}

fn gpt(disk: &mut Disk, mbr: &[Entry; 4]) -> Option<Scheme> {
    let size = disk.size();
    // The primary header at LBA 1, else the backup at the last LBA; 512 or 4096-byte sectors.
    let g = [512, 4096].into_iter().find_map(|ss| gpt_at(disk, ss, 1)).or_else(|| {
        [512, 4096].into_iter().find_map(|ss| (size / ss).checked_sub(1).and_then(|last| gpt_at(disk, ss, last)))
    })?;
    let ss = g.ss;
    let mut scheme = Scheme::new(Table::Gpt);
    scheme.fixed.push(Extent { start: g.lba * ss, len: ss });
    scheme.fixed.push(Extent { start: g.entries_at().saturating_mul(ss), len: g.entries_len() });
    // The other copy: its header, and its entries (found through it, else where they belong).
    let alt = u64_at(&g.header, 32);
    if let Some(pos) = alt.checked_mul(ss).filter(|&pos| pos < size) {
        scheme.fixed.push(Extent { start: pos, len: ss });
        let entries = match gpt_at(disk, ss, alt) {
            Some(other) => other.entries_at().saturating_mul(ss),
            None if g.lba == 1 => pos.saturating_sub(g.entries_len()),
            None => 2 * ss,
        };
        scheme.fixed.push(Extent { start: entries, len: g.entries_len() });
    }
    for (i, e) in g.entries.chunks_exact(g.entry_size() as usize).enumerate() {
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (first, last) = (u64_at(e, 32), u64_at(e, 40));
        let Some(start) = first.checked_mul(ss).filter(|&start| start < size && first <= last) else { continue };
        let end = last.saturating_add(1).saturating_mul(ss).min(size);
        scheme.slots.push(Slot { index: i as u32 + 1, start, size: end - start });
    }
    // A hybrid MBR may also describe space no GPT partition covers: keep that as is.
    for x in mbr.iter().filter(|x| x.used() && x.kind != 0xEE && !x.extended()) {
        let Some((start, len)) = x.bytes(x.start, ss, size) else { continue };
        if !scheme.slots.iter().any(|s| s.start <= start && start + len <= s.start + s.size) {
            scheme.fixed.push(Extent { start, len });
        }
    }
    Some(scheme)
}
