//! Partition tables: MBR (with logical partitions in extended ones, and BSD disklabels in
//! BSD slices), GPT, Apple partition maps, or none at all.
//!
//! Partitions are numbered the way Linux numbers them:
//! - GPT: by entry, from 1.
//! - MBR: primary partitions 1 to 4, logical partitions from 5 on (in chain order).
//! - BSD disklabels (in MBR partitions of type 0xA5 FreeBSD, 0xA6 OpenBSD, 0xA9 NetBSD):
//!   their partitions come after the logical ones, in label order. The slice itself isn't
//!   looked into then: what no BSD partition covers is free, except its boot area and label.
//! - Apple partition maps: by map entry, from 1 (the map itself is usually 1). Only
//!   entries that hold data are listed: free space (Apple_Free) is skipped, and the map and
//!   the drivers are copied as they are.
//!
//! Like Linux, a GPT (or else an MBR) takes precedence over an Apple partition map on the
//! same drive, as on hybrid CD images. The map's partitions are then copied as they are.

use super::probe;
use super::util::{Disk, at, be16_at, be32_at, crc32, u16_at, u32_at, u64_at};
use super::{Extent, Table};
use std::io;

/// How an Apple partition map shows in `Layout::table`.
pub(crate) const APM: Table = Table::Apm;

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
    let apm = apple_map(disk, head);
    let signed = at(mbr, 510, &[0x55, 0xAA]);
    let e = entries(mbr);
    // Without a valid GPT after all, the 0xEE entry is copied in full as an unknown partition.
    if signed
        && e.iter().any(|x| x.kind == 0xEE)
        && let Some(mut gpt) = gpt(disk, &e)
    {
        if let Some(apm) = &apm {
            apm.keep(&mut gpt);
        }
        return Ok(gpt);
    }
    let valid = signed && e.iter().all(|x| x.boot == 0 || x.boot == 0x80);
    let partitions = valid && e.iter().any(|x| x.used() && x.start.checked_mul(512).is_some_and(|p| p < size));
    let boot_sector = probe::boot_sector(mbr).is_some();
    Ok(match (boot_sector, partitions, apm) {
        (true, true, _) => {
            let mut scheme = Scheme::floppy(size);
            scheme.whole = Some("both a boot sector and a partition table");
            scheme
        }
        (false, true, apm) => {
            let mut scheme = mbr_table(disk, &e);
            if let Some(apm) = apm {
                apm.keep(&mut scheme);
            }
            scheme
        }
        (_, false, Some(apm)) => apm.scheme(),
        (false, false, None) if valid && probe::detect(head).is_empty() => Scheme::new(Table::Mbr),
        _ => Scheme::floppy(size),
    })
}

fn mbr_table(disk: &mut Disk, e: &[Entry; 4]) -> Scheme {
    let size = disk.size();
    let (ss, unsure) = sector_size(disk, e);
    let mut scheme = Scheme::new(Table::Mbr);
    let mut next = 5;
    let mut labels = Vec::new();
    for (i, x) in e.iter().enumerate().filter(|(_, x)| x.used()) {
        let Some((start, len)) = x.bytes(x.start, ss, size) else { continue };
        if x.extended() {
            logical(disk, x, ss, &mut scheme, &mut next);
        } else if let Some(label) = bsd_label(disk, x, ss) {
            labels.push(label);
        } else {
            scheme.slots.push(Slot { index: i as u32 + 1, start, size: len });
        }
    }
    // BSD partitions are numbered after the logical ones.
    for label in labels {
        label.add(&mut scheme, &mut next);
    }
    // Like Linux lists them: primary partitions, then logical ones, then BSD ones.
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
        let fs = !probe::detect(&head).is_empty() || (bsd_flavour(x.kind).is_some() && bsd_magic(&head, ss).is_some());
        if (x.extended() && at(&head, 510, &[0x55, 0xAA])) || (!x.extended() && fs) {
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

// Apple partition maps.

/// Map entries read at most.
const MAX_APM_ENTRIES: u64 = 256;

/// What an Apple partition map entry holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApmKind {
    /// Apple_Free: nothing.
    Free,
    /// The map itself, and drivers: copied as they are.
    Kept,
    /// A partition to look into.
    Data,
}

struct ApmEntry {
    index: u32,
    start: u64,
    size: u64,
    kind: ApmKind,
}

struct AppleMap {
    entries: Vec<ApmEntry>,
    /// Block 0, the map's entries, and the drivers block 0 lists.
    fixed: Vec<Extent>,
}

/// An Apple partition map: a driver descriptor ("ER") in block 0 with the block size (512,
/// or 2048 on CDs), then map entries ("PM") in blocks 1 to N, N being in every entry. Like
/// Linux, the map ends early at the first block that isn't an entry.
fn apple_map(disk: &mut Disk, head: &[u8]) -> Option<AppleMap> {
    let size = disk.size();
    let bs = u64::from(be16_at(head, 2));
    if !at(head, 0, b"ER") || !matches!(bs, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let mut entry = [0u8; 512];
    disk.read_at(bs, &mut entry).ok()?;
    let count = u64::from(be32_at(&entry, 4));
    if !at(&entry, 0, b"PM") || !(1..=MAX_APM_ENTRIES).contains(&count) {
        return None;
    }
    let mut map = AppleMap { entries: Vec::new(), fixed: vec![Extent { start: 0, len: bs }] };
    for slot in 1..=count {
        let pos = slot * bs;
        if pos + 512 > size || disk.read_at(pos, &mut entry).is_err() || !at(&entry, 0, b"PM") {
            break;
        }
        map.fixed.push(Extent { start: pos, len: bs });
        let (first, blocks) = (u64::from(be32_at(&entry, 8)), u64::from(be32_at(&entry, 12)));
        let Some(start) = first.checked_mul(bs).filter(|&start| start < size) else { continue };
        let end = first.saturating_add(blocks).saturating_mul(bs).min(size);
        if end > start {
            map.entries.push(ApmEntry { index: slot as u32, start, size: end - start, kind: apm_kind(&entry[48..80]) });
        }
    }
    // Drivers, as block 0 lists them: first block, size in 512-byte sectors, type.
    for i in 0..usize::from(be16_at(head, 16)).min(61) {
        let (block, sectors) = (u64::from(be32_at(head, 18 + 8 * i)), u64::from(be16_at(head, 22 + 8 * i)));
        if let Some(start) = block.checked_mul(bs).filter(|&start| start < size) {
            map.fixed.push(Extent { start, len: (sectors * 512).max(bs).min(size - start) });
        }
    }
    Some(map)
}

/// By the entry's type (`pmParType`), compared the way Apple does: ignoring case.
fn apm_kind(kind: &[u8]) -> ApmKind {
    let name = kind.split(|&c| c == 0).next().unwrap_or_default();
    let prefix = |p: &str| name.get(..p.len()).is_some_and(|n| n.eq_ignore_ascii_case(p.as_bytes()));
    let is = |t: &str| name.eq_ignore_ascii_case(t.as_bytes());
    if is("Apple_Free") {
        ApmKind::Free
    } else if is("Apple_partition_map") || prefix("Apple_Driver") || is("Apple_Patches") || is("Apple_FWDriver") {
        ApmKind::Kept
    } else {
        ApmKind::Data
    }
}

impl AppleMap {
    /// The drive's scheme when the map is the only partition table.
    fn scheme(self) -> Scheme {
        let mut scheme = Scheme::new(APM);
        scheme.fixed = self.fixed;
        for e in self.entries {
            match e.kind {
                ApmKind::Free => {}
                ApmKind::Kept => scheme.fixed.push(Extent { start: e.start, len: e.size }),
                ApmKind::Data => scheme.slots.push(Slot { index: e.index, start: e.start, size: e.size }),
            }
        }
        scheme
    }

    /// Next to a GPT or an MBR that's used instead, as on hybrid CD images: the map's
    /// partitions are kept as they are. Their file systems often share blocks with the
    /// ones the other table shows, which then can't tell what the others need.
    fn keep(&self, scheme: &mut Scheme) {
        scheme.fixed.extend(&self.fixed);
        let used = self.entries.iter().filter(|e| e.kind != ApmKind::Free);
        scheme.fixed.extend(used.map(|e| Extent { start: e.start, len: e.size }));
    }
}

// BSD disklabels.

const BSD_MAGIC: u32 = 0x8256_4557;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Free,
    Open,
    Net,
}

/// MBR partition types of BSD slices.
fn bsd_flavour(kind: u8) -> Option<Flavour> {
    match kind {
        0xA5 => Some(Flavour::Free),
        0xA6 => Some(Flavour::Open),
        0xA9 => Some(Flavour::Net),
        _ => None,
    }
}

/// Whether a disklabel sits in sector 1 of a slice whose first bytes are `head`: its byte
/// order then (true for big-endian).
fn bsd_magic(head: &[u8], ss: u64) -> Option<bool> {
    let at = usize::try_from(ss).ok()?;
    let le = u32_at(head, at) == BSD_MAGIC && u32_at(head, at + 132) == BSD_MAGIC;
    let be = be32_at(head, at) == BSD_MAGIC && be32_at(head, at + 132) == BSD_MAGIC;
    if le {
        Some(false)
    } else if be {
        Some(true)
    } else {
        None
    }
}

/// A disklabel that can be trusted, in bytes on the drive.
struct BsdLabel {
    /// The slice's boot code and the label.
    boot: Extent,
    /// The partitions Linux lists, in order (empty ones take a number too).
    listed: Vec<(u64, u64)>,
    /// Partitions inside the slice that Linux doesn't list (of the "unused" type, or past
    /// the 8 or 16 it reads): copied as they are.
    kept: Vec<Extent>,
}

impl BsdLabel {
    fn add(self, scheme: &mut Scheme, next: &mut u32) {
        scheme.fixed.push(self.boot);
        scheme.fixed.extend(self.kept);
        for (start, size) in self.listed {
            if size > 0 {
                scheme.slots.push(Slot { index: *next, start, size });
            }
            *next += 1;
        }
    }
}

/// The disklabel of a BSD slice, when there's one to trust: its magic twice, its checksum,
/// the slice's sector size, and partitions that are either inside the slice or clear of it.
/// Offsets are relative to the slice for FreeBSD when the raw partition "c" starts at 0 (as
/// Linux reads them), absolute otherwise. A file system signature on the slice itself (a
/// label left over from before) makes it untrustworthy too. None: the slice is looked into
/// as a partition of its own.
fn bsd_label(disk: &mut Disk, x: &Entry, ss: u64) -> Option<BsdLabel> {
    let flavour = bsd_flavour(x.kind)?;
    let size = disk.size();
    let (first, sectors) = (x.start, x.sectors);
    let slice = first.checked_mul(ss).filter(|&start| start < size)?;
    let slice_len = sectors.saturating_mul(ss).min(size - slice);
    let head = disk.read_vec(slice, slice_len.min(probe::HEAD as u64) as usize).ok()?;
    let big = bsd_magic(&head, ss)?;
    let l = head.get(ss as usize..ss as usize + 512)?;
    let r16 = |at| if big { be16_at(l, at) } else { u16_at(l, at) };
    let r32 = |at| u64::from(if big { be32_at(l, at) } else { u32_at(l, at) });
    let count = usize::from(r16(138));
    let end = 148 + 16 * count;
    // The checksum makes the label's 16-bit words XOR to zero, partitions included.
    let xor = l.get(..end)?.as_chunks::<2>().0.iter().fold(0, |x, &w| x ^ u16::from_le_bytes(w));
    if count == 0 || xor != 0 || r32(40) != ss {
        return None;
    }
    // OpenBSD keeps high 16 bits of offsets and sizes in version 1 labels.
    let wide = match (flavour, r16(114)) {
        (Flavour::Open, 0) => false,
        (Flavour::Open, 1) => true,
        (Flavour::Open, _) => return None,
        _ => false,
    };
    let base = if flavour == Flavour::Free && r32(148 + 2 * 16 + 4) == 0 { first } else { 0 };
    let listed_max = if flavour == Flavour::Open { 16 } else { 8 };
    let slice_end = first.checked_add(sectors)?;
    let bytes = |start: u64, len: u64| {
        let from = start.saturating_mul(ss).min(size);
        (from, len.saturating_mul(ss).min(size - from))
    };
    let mut listed = Vec::new();
    let mut kept = Vec::new();
    for i in 0..count {
        let e = 148 + 16 * i;
        let high = |at| if wide { u64::from(r16(at)) << 32 } else { 0 };
        let len = r32(e) | high(e + 10);
        let start = base.checked_add(r32(e + 4) | high(e + 8))?;
        let end = start.checked_add(len)?;
        let raw = start == first && len == sectors;
        let inside = start >= first && end <= slice_end;
        if i < listed_max && l[e + 12] != 0 && !raw && inside {
            listed.push(bytes(start, len));
        } else if len == 0 || raw || end <= first || start >= slice_end || (start <= first && end >= slice_end) {
            // Empty, the slice itself, clear of it, or around it (the whole drive).
        } else if inside {
            let (start, len) = bytes(start, len);
            kept.push(Extent { start, len });
        } else {
            // Across the slice's edge: the label doesn't add up.
            return None;
        }
    }
    if !probe::detect(&head).iter().all(|&f| f == probe::Fs::Other("ufs")) || zfs(disk, slice, slice_len) {
        return None;
    }
    let boot = r32(140).clamp(64 << 10, 1 << 20).min(slice_len);
    Some(BsdLabel { boot: Extent { start: slice, len: boot }, listed, kept })
}

/// ZFS uberblocks where the first vdev label keeps them (128 to 256 KiB in): ZFS on the
/// slice itself (or on a BSD partition at its start, which then gets copied in full).
fn zfs(disk: &mut Disk, start: u64, len: u64) -> bool {
    const MAGIC: u64 = 0x00BA_B10C;
    if len < 256 << 10 {
        return false;
    }
    let Ok(ring) = disk.read_vec(start + (128 << 10), 128 << 10) else { return true };
    ring.as_chunks::<1024>().0.iter().any(|u| u64_at(u, 0) == MAGIC || u64_at(u, 0).swap_bytes() == MAGIC)
}
