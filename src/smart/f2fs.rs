//! F2FS: the segment information table (SIT) holds a bitmap of valid blocks per segment of the
//! main area. The superblocks, checkpoint packs, SIT, NAT and segment summary area before the
//! main area are kept whole.
//!
//! The valid checkpoint pack is the one with the highest version whose first and last blocks
//! match and pass their CRCs. It says which of the two copies of each SIT block is current,
//! and holds SIT entries not yet written back (the journal in its summaries). It must record
//! a clean unmount, and no node blocks may wait to be rolled forward after it (fsync'd data).
//! The valid block counts must add up to what it records.

use super::util::{Bad, CHUNK, Mode, Part, Ranges, Res, Usage, bit_runs, crc32_le, ensure, u16_at, u32_at, u64_at};

const MAGIC: u32 = 0xF2F5_2010;
const BLOCK: u64 = 4096;
/// Blocks per segment.
const SEGMENT: u64 = 512;
/// SIT entries (74 bytes) per SIT block.
const SIT_ENTRIES: u64 = BLOCK / 74;

/// An F2FS superblock at 1 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    u32_at(head, 1024) == MAGIC
}

// Superblock features.
const FEATURE_BLKZONED: u32 = 0x2;
const FEATURE_SB_CHKSUM: u32 = 0x800;
const FEATURE_RO: u32 = 0x4000;
/// Features that don't change how blocks are accounted for: encrypt, atomic_write,
/// extra_attr, project_quota, inode_checksum, flexible_inline_xattr, quota, inode_crtime,
/// lost_found, verity, sb_checksum, casefold, compression.
const FEATURES_OK: u32 = 0x3FFD;

// Checkpoint flags.
const CP_UMOUNT: u32 = 0x1;
const CP_COMPACT_SUM: u32 = 0x4;
const CP_ERROR: u32 = 0x8;
const CP_FSCK: u32 = 0x10;
const CP_FASTBOOT: u32 = 0x20;
const CP_CRC_RECOVERY: u32 = 0x40;
const CP_NOCRC_RECOVERY: u32 = 0x200;
const CP_LARGE_NAT_BITMAP: u32 = 0x400;
const CP_QUOTA_NEED_FSCK: u32 = 0x800;
const CP_DISABLED: u32 = 0x1000;
const CP_DISABLED_QUICK: u32 = 0x2000;
const CP_RESIZEFS: u32 = 0x4000;

/// Layout of the file system, in blocks.
struct F2fs {
    cp: u64,
    sit: u64,
    /// SIT blocks per copy.
    sit_blocks: u64,
    main: u64,
    /// Segments in the main area.
    segments: u64,
    cp_payload: u64,
}

/// A valid checkpoint pack: its blocks, header first.
struct Checkpoint {
    start: u64,
    header: Vec<u8>,
}

impl Checkpoint {
    fn flags(&self) -> u32 {
        u32_at(&self.header, 132)
    }

    fn total(&self) -> u64 {
        u64::from(u32_at(&self.header, 136))
    }
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let sb = superblocks(head)?;
    let name: Vec<u16> = sb[124..1148].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
    let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
    let name = name.trim().to_owned();
    *label = (!name.is_empty()).then_some(name);
    let fs = layout(sb, p.size)?;
    let cp = checkpoint(p, &fs)?;
    let flags = cp.flags();
    ensure(
        flags & (CP_ERROR | CP_FSCK | CP_RESIZEFS | CP_DISABLED | CP_DISABLED_QUICK) == 0,
        "checkpoint flags errors",
    )?;
    if p.opts.mode == Mode::Copy {
        ensure(flags & CP_UMOUNT != 0, "not cleanly unmounted")?;
        ensure(flags & CP_QUOTA_NEED_FSCK == 0, "quota needs fsck")?;
        roll_forward(p, &fs, &cp)?;
    }
    // Per segment: the SIT entry (valid block count and bitmap), journal entries first.
    let journal = sit_journal(p, &cp)?;
    let bitmap = sit_bitmap(p, &fs, &cp)?;
    let mut used = p.ranges();
    used.add(0, fs.main * BLOCK);
    let mut valid = 0u64;
    let sit_blocks = fs.segments.div_ceil(SIT_ENTRIES);
    // SIT blocks are read in runs taken from the same copy.
    let mut b = 0;
    while b < sit_blocks {
        let copy = bit(&bitmap, b);
        let mut n = 1;
        while b + n < sit_blocks && n < CHUNK as u64 / BLOCK && bit(&bitmap, b + n) == copy {
            n += 1;
        }
        let at = fs.sit + b + if copy { fs.sit_blocks } else { 0 };
        let data = p.read_vec(at * BLOCK, (n * BLOCK) as usize)?;
        for (i, block) in data.as_chunks::<{ BLOCK as usize }>().0.iter().enumerate() {
            let first = (b + i as u64) * SIT_ENTRIES;
            for (j, entry) in block.as_chunks::<74>().0.iter().enumerate() {
                let segno = first + j as u64;
                if segno >= fs.segments {
                    break;
                }
                let entry = journal.iter().find(|(s, _)| u64::from(*s) == segno).map_or(&entry[..], |(_, e)| &e[..]);
                valid += segment(&fs, segno, entry, &mut used)?;
            }
        }
        b += n;
    }
    if p.opts.mode == Mode::Copy {
        ensure(valid == u64_at(&cp.header, 16), "valid block count doesn't match the SIT")?;
    }
    Ok(Usage { used, end: (fs.main + fs.segments * SEGMENT) * BLOCK })
}

/// The superblock to go by: the first copy, which the second must agree with when valid.
fn superblocks(head: &[u8]) -> Res<&[u8]> {
    let first = head.get(1024..4096).ok_or_else(|| Bad("partition too small for F2FS".into()))?;
    ensure(superblock_ok(first), "bad superblock")?;
    if let Some(second) = head.get(4096 + 1024..2 * 4096)
        && superblock_ok(second)
    {
        // The layout (everything up to the UUID) must be the same.
        ensure(first[..108] == second[..108], "superblocks disagree")?;
    }
    Ok(first)
}

fn superblock_ok(sb: &[u8]) -> bool {
    if u32_at(sb, 0) != MAGIC {
        return false;
    }
    if u32_at(sb, 2180) & FEATURE_SB_CHKSUM != 0 {
        let at = u32_at(sb, 32) as usize;
        return at == 3068 && crc32_le(MAGIC, &sb[..at]) == u32_at(sb, at);
    }
    true
}

fn layout(sb: &[u8], part_size: u64) -> Res<F2fs> {
    let features = u32_at(sb, 2180);
    ensure(features & FEATURE_BLKZONED == 0, "zoned F2FS")?;
    ensure(features & FEATURE_RO == 0, "read-only F2FS image")?;
    ensure(features & !FEATURES_OK == 0, "unsupported features")?;
    // Several devices: the first entry names one.
    ensure(sb[2201] == 0, "F2FS on several devices")?;
    let log_sector = u32_at(sb, 8);
    ensure((9..=12).contains(&log_sector) && u32_at(sb, 12) == 12 - log_sector, "bad sector size")?;
    ensure(u32_at(sb, 16) == 12 && u32_at(sb, 20) == 9, "unsupported block or segment size")?;
    let blocks = u64_at(sb, 36);
    ensure(blocks.checked_mul(BLOCK).is_some_and(|n| n <= part_size), "file system larger than its partition")?;
    let field = |at| u64::from(u32_at(sb, at));
    let (segment_count, ckpt, sit, nat, ssa, main) = (field(48), field(52), field(56), field(60), field(64), field(68));
    let (seg0, cp_at, sit_at, nat_at, ssa_at, main_at) =
        (field(72), field(76), field(80), field(84), field(88), field(92));
    ensure(ckpt == 2 && sit >= 2 && sit % 2 == 0 && nat >= 2 && ssa >= 1 && main >= 1, "bad layout")?;
    // The areas follow each other.
    let areas = [(cp_at, ckpt), (sit_at, sit), (nat_at, nat), (ssa_at, ssa), (main_at, main)];
    let mut next = seg0;
    for (at, segments) in areas {
        ensure(at == next, "areas out of place")?;
        next = at + segments * SEGMENT;
    }
    ensure(seg0 >= 2 && ckpt + sit + nat + ssa + main <= segment_count, "bad layout")?;
    ensure(seg0 + segment_count * SEGMENT <= blocks, "segments past the end")?;
    // One SIT entry per main segment, in each copy.
    let sit_blocks = sit / 2 * SEGMENT;
    ensure(main.div_ceil(SIT_ENTRIES) <= sit_blocks, "SIT too small")?;
    Ok(F2fs { cp: cp_at, sit: sit_at, sit_blocks, main: main_at, segments: main, cp_payload: field(1664) })
}

/// A checkpoint block's CRC, at the offset it gives.
fn cp_block_ok(b: &[u8]) -> bool {
    let at = u32_at(b, 164) as usize;
    (at == 4092 || at == 192) && crc32_le(MAGIC, &b[..at]) == u32_at(b, at)
}

/// The current checkpoint pack: of the two, the valid one with the highest version.
fn checkpoint(p: &mut Part, fs: &F2fs) -> Res<Checkpoint> {
    let mut best: Option<Checkpoint> = None;
    for start in [fs.cp, fs.cp + SEGMENT] {
        let header = p.read_vec(start * BLOCK, BLOCK as usize)?;
        if !cp_block_ok(&header) {
            continue;
        }
        let total = u64::from(u32_at(&header, 136));
        if !(2..=SEGMENT).contains(&total) {
            continue;
        }
        let footer = p.read_vec((start + total - 1) * BLOCK, BLOCK as usize)?;
        if !cp_block_ok(&footer) || u64_at(&footer, 0) != u64_at(&header, 0) {
            continue;
        }
        let version = u64_at(&header, 0);
        if best.as_ref().is_none_or(|b| version > u64_at(&b.header, 0)) {
            best = Some(Checkpoint { start, header });
        }
    }
    let cp = best.ok_or_else(|| Bad("no valid checkpoint".into()))?;
    ensure(u64::from(u32_at(&cp.header, 140)) < cp.total(), "bad checkpoint pack")?;
    Ok(cp)
}

/// The SIT version bitmap: bit n (most significant first) set when the second copy of SIT
/// block n is the current one.
fn sit_bitmap(p: &mut Part, fs: &F2fs, cp: &Checkpoint) -> Res<Vec<u8>> {
    let size = u32_at(&cp.header, 156) as usize;
    ensure(size as u64 == fs.sit_blocks / 8, "bad SIT bitmap size")?;
    let nat_size = u32_at(&cp.header, 160) as usize;
    if cp.flags() & CP_LARGE_NAT_BITMAP != 0 {
        let at = 192 + nat_size + 4;
        return cp.header.get(at..at + size).map(<[u8]>::to_vec).ok_or_else(|| Bad("bad SIT bitmap".into()));
    }
    if fs.cp_payload > 0 {
        // In the payload blocks right after the header.
        ensure((size as u64).div_ceil(BLOCK) <= fs.cp_payload, "bad SIT bitmap size")?;
        return Ok(p.read_vec((cp.start + 1) * BLOCK, size)?);
    }
    cp.header.get(192..192 + size).map(<[u8]>::to_vec).ok_or_else(|| Bad("bad SIT bitmap".into()))
}

fn bit(map: &[u8], n: u64) -> bool {
    map.get((n / 8) as usize).is_some_and(|b| b & (0x80 >> (n % 8)) != 0)
}

/// SIT entries recorded in the checkpoint rather than the SIT: the journal of the cold data
/// log's summary. (segment, entry) pairs.
fn sit_journal(p: &mut Part, cp: &Checkpoint) -> Res<Vec<(u32, Vec<u8>)>> {
    const JOURNAL: usize = 507;
    let flags = cp.flags();
    let block = if flags & CP_COMPACT_SUM != 0 {
        // Compacted: the NAT journal, then the SIT journal, at the start of the summaries.
        let at = cp.start + u64::from(u32_at(&cp.header, 140));
        p.read_vec(at * BLOCK, BLOCK as usize)?.split_off(JOURNAL)
    } else {
        // One summary block per log: the cold data one (third), with node summaries after
        // the data ones on a clean unmount.
        let from_end = if flags & (CP_UMOUNT | CP_FASTBOOT) != 0 { 7 } else { 4 };
        ensure(cp.total() > from_end, "bad checkpoint pack")?;
        let at = cp.start + cp.total() - from_end + 2;
        p.read_vec(at * BLOCK, BLOCK as usize)?.split_off(512 * 7)
    };
    let n = usize::from(u16_at(&block, 0));
    ensure(n <= (JOURNAL - 2) / 78, "bad SIT journal")?;
    Ok(block[2..2 + n * 78].as_chunks::<78>().0.iter().map(|e| (u32_at(e, 0), e[4..].to_vec())).collect())
}

/// Marks the valid blocks of main segment `segno` per its SIT entry; returns how many.
fn segment(fs: &F2fs, segno: u64, entry: &[u8], used: &mut Ranges) -> Res<u64> {
    let count = u64::from(u16_at(entry, 0) & 0x3FF);
    let map = &entry[2..66];
    ensure(u64::from(map.iter().map(|b| b.count_ones()).sum::<u32>()) == count, "SIT entry count mismatch")?;
    if count > 0 {
        // The bitmap numbers blocks from the most significant bit.
        let map: Vec<u8> = map.iter().map(|b| b.reverse_bits()).collect();
        let base = (fs.main + segno * SEGMENT) * BLOCK;
        bit_runs(&map, SEGMENT, 0, |b, n| used.add(base + b * BLOCK, n * BLOCK));
    }
    Ok(count)
}

/// Fails when node blocks written after the checkpoint (by fsync) would be rolled forward at
/// the next mount: the kernel looks at the warm node log's next block for one.
fn roll_forward(p: &mut Part, fs: &F2fs, cp: &Checkpoint) -> Res<()> {
    let segno = u64::from(u32_at(&cp.header, 36 + 4));
    let offset = u64::from(u16_at(&cp.header, 68 + 2));
    ensure(segno < fs.segments && offset <= SEGMENT, "bad current segment")?;
    if offset == SEGMENT {
        return Ok(());
    }
    let node = p.read_vec((fs.main + segno * SEGMENT + offset) * BLOCK, BLOCK as usize)?;
    // The node footer's checkpoint version, as is_recoverable_dnode() compares it.
    let found = u64_at(&node, 4072 + 12);
    let version = u64_at(&cp.header, 0);
    let flags = cp.flags();
    let recoverable = if flags & CP_NOCRC_RECOVERY != 0 {
        found << 32 == version << 32
    } else if flags & CP_CRC_RECOVERY != 0 {
        let crc = u64::from(u32_at(&cp.header, u32_at(&cp.header, 164) as usize));
        found == version | crc << 32
    } else {
        found == version
    };
    ensure(!recoverable, "fsync'd data to roll forward (not cleanly unmounted)")
}
