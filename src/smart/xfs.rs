//! XFS (v4 and v5): each allocation group's free space B+tree (by block number) lists its free
//! extents; everything else is used, the AG headers and the internal log included.
//!
//! The log must be clean: its last record an unmount record, right before the head. An
//! external log or a realtime device is copied in full. On v5 file systems, the superblock,
//! the AG headers and every B+tree block read are checked against their CRCs. The count of
//! free blocks each AGF records must match both free space trees.

use super::util::{Bad, Mode, Part, Ranges, Res, Usage, at, be16_at, be32_at, crc32c, ensure, u32_at};

/// An XFS superblock at the start.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0, b"XFSB")
}

fn be64_at(b: &[u8], at: usize) -> u64 {
    u64::from(be32_at(b, at)) << 32 | u64::from(be32_at(b, at + 4))
}

/// XFS's CRC32C: over the whole buffer with the checksum field taken as zero, inverted.
fn crc_ok(b: &[u8], at: usize) -> bool {
    let Some(end) = at.checked_add(4).filter(|&end| end <= b.len()) else {
        return false;
    };
    let crc = crc32c(crc32c(crc32c(!0, &b[..at]), &[0; 4]), &b[end..]);
    !crc == u32_at(b, at)
}

const VERSION_SHARED: u16 = 0x0200;
const VERSION_MOREBITS: u16 = 0x8000;
const VERSION_DIRV2: u16 = 0x2000;
const VERSION_EXTFLG: u16 = 0x1000;
const VERSION_LOGV2: u16 = 0x0400;
/// features2 bits of v4: lazy counters, attr2, 32-bit project IDs, file types.
const FEATURES2_OK: u32 = 0x02 | 0x08 | 0x80 | 0x200;
/// v5 read-only compatible features: finobt, rmapbt, reflink, inobtcount.
const RO_COMPAT_OK: u32 = 0xF;
/// v5 incompatible features that don't change how free space is found: ftype, sparse inodes,
/// meta_uuid, bigtime, nrext64, exchange-range, parent pointers, metadir.
const INCOMPAT_OK: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x20 | 0x40 | 0x80 | 0x100;
const INCOMPAT_META_UUID: u32 = 0x4;
/// Log incompatible features: logged extended attributes.
const LOG_INCOMPAT_OK: u32 = 0x1;
/// More allocation groups than this and the file system is copied in full.
const MAX_AGS: u64 = 1 << 20;
const MAX_LEVELS: u32 = 16;
const NULL_BLOCK: u32 = u32::MAX;

struct Xfs {
    block: u64,
    sect: u64,
    agblocks: u64,
    agcount: u64,
    dblocks: u64,
    v5: bool,
    /// What metadata blocks carry: the metadata UUID.
    uuid: [u8; 16],
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let sb = head.get(..512).ok_or_else(|| Bad("partition too small for XFS".into()))?;
    let name = &sb[108..120];
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(12)]).trim().to_owned();
    *label = (!name.is_empty()).then_some(name);
    let (fs, log) = superblock(sb, head, p.size)?;
    if p.opts.mode == Mode::Copy {
        log_clean(p, &fs, log)?;
    }
    let mut used = p.ranges();
    for agno in 0..fs.agcount {
        allocation_group(p, &fs, agno, &mut used)?;
    }
    Ok(Usage { used, end: fs.dblocks * fs.block })
}

/// The internal log: where it starts (bytes), its size in 512-byte blocks, and whether it's
/// a version 2 log.
struct Log {
    start: u64,
    blocks: u64,
    v2: bool,
    uuid: [u8; 16],
}

fn superblock(sb: &[u8], head: &[u8], part_size: u64) -> Res<(Xfs, Log)> {
    ensure(at(sb, 0, b"XFSB"), "not XFS")?;
    let version = be16_at(sb, 100);
    let v5 = match version & 0xF {
        4 => false,
        5 => true,
        _ => return Err(Bad("unsupported XFS version".into())),
    };
    let blocklog = u32::from(sb[120]);
    let sectlog = u32::from(sb[121]);
    ensure((9..=16).contains(&blocklog) && (9..=15).contains(&sectlog) && sectlog <= blocklog, "bad block size")?;
    let block = 1u64 << blocklog;
    let sect = 1u64 << sectlog;
    ensure(u64::from(be32_at(sb, 4)) == block && u64::from(be16_at(sb, 102)) == sect, "bad block size")?;
    if v5 {
        let sb_sector = head.get(..sect as usize).ok_or_else(|| Bad("partition too small for XFS".into()))?;
        ensure(crc_ok(sb_sector, 224), "superblock checksum mismatch")?;
        let (compat_ro, incompat, log_incompat) = (be32_at(sb, 212), be32_at(sb, 216), be32_at(sb, 220));
        ensure(compat_ro & !RO_COMPAT_OK == 0, "unsupported read-only features")?;
        ensure(incompat & !INCOMPAT_OK == 0, "unsupported features (or needs repair)")?;
        ensure(log_incompat & !LOG_INCOMPAT_OK == 0, "unsupported log features")?;
    } else {
        ensure(version & VERSION_SHARED == 0, "shared XFS")?;
        ensure(version & VERSION_DIRV2 != 0 && version & VERSION_EXTFLG != 0, "old XFS format")?;
        let features2 = be32_at(sb, 200);
        ensure(version & VERSION_MOREBITS == 0 || features2 & !FEATURES2_OK == 0, "unsupported features")?;
    }
    ensure(sb[126] == 0, "mkfs didn't finish")?;
    ensure(be64_at(sb, 16) == 0 && be64_at(sb, 24) == 0, "realtime device")?;
    let dblocks = be64_at(sb, 8);
    let agblocks = u64::from(be32_at(sb, 84));
    let agcount = u64::from(be32_at(sb, 88));
    let agblklog = u32::from(sb[124]);
    ensure(agblocks >= 64 && (1..=MAX_AGS).contains(&agcount), "bad allocation groups")?;
    ensure(agblklog == 64 - (agblocks - 1).leading_zeros(), "bad allocation group size")?;
    let full = agcount.checked_mul(agblocks).ok_or_else(|| Bad("bad allocation groups".into()))?;
    ensure(dblocks <= full && dblocks > full - agblocks, "block count doesn't match the allocation groups")?;
    ensure(dblocks.checked_mul(block).is_some_and(|n| n <= part_size), "file system larger than its partition")?;
    // The internal log.
    let logstart = be64_at(sb, 48);
    ensure(logstart != 0, "external log")?;
    let (log_ag, log_agbno) = (logstart >> agblklog, logstart & ((1 << agblklog) - 1));
    let log_blocks = u64::from(be32_at(sb, 96));
    let first = log_ag.checked_mul(agblocks).and_then(|b| b.checked_add(log_agbno));
    let inside = first.and_then(|b| b.checked_add(log_blocks)).is_some_and(|end| end <= dblocks);
    ensure(log_ag < agcount && log_agbno < agblocks && log_blocks >= 64 && inside, "log outside the file system")?;
    let uuid: [u8; 16] = sb[32..48].try_into().unwrap_or_default();
    let meta_uuid = if v5 && be32_at(sb, 216) & INCOMPAT_META_UUID != 0 {
        sb[248..264].try_into().unwrap_or_default()
    } else {
        uuid
    };
    let fs = Xfs { block, sect, agblocks, agcount, dblocks, v5, uuid: meta_uuid };
    let log = Log {
        start: first.unwrap_or(0) * block,
        blocks: log_blocks * block / 512,
        v2: v5 || version & VERSION_LOGV2 != 0,
        uuid,
    };
    Ok((fs, log))
}

// The log: a ring of 512-byte blocks, each stamped with the cycle (pass over the ring) that
// wrote it. Records start with a header block.

const LOG_MAGIC: u32 = 0xFEED_BABE;
/// How far writes may have landed out of order around the head: 8 in-core logs of up to
/// 256 KiB. Twice that, in 512-byte blocks, is checked on each side.
const LOG_WINDOW: u64 = 2 * 8 * (256 << 10) / 512;

/// The cycle a log block was written in.
fn cycle(b: &[u8]) -> u32 {
    if be32_at(b, 0) == LOG_MAGIC { be32_at(b, 4) } else { be32_at(b, 0) }
}

/// Reads `n` log blocks from block `first` on (wrapping around the end of the log).
fn log_blocks(p: &mut Part, log: &Log, first: u64, n: u64) -> Res<Vec<u8>> {
    let first = first % log.blocks;
    let n = n.min(log.blocks);
    let direct = n.min(log.blocks - first);
    let mut data = p.read_vec(log.start + first * 512, (direct * 512) as usize)?;
    if direct < n {
        data.extend(p.read_vec(log.start, ((n - direct) * 512) as usize)?);
    }
    Ok(data)
}

fn cycle_at(p: &mut Part, log: &Log, b: u64) -> Res<u32> {
    Ok(cycle(&log_blocks(p, log, b, 1)?))
}

/// Checks the log was cleanly unmounted: the head (where the cycle drops) is found like the
/// kernel finds it, the blocks around it must say the same without exception, and the last
/// record before it must be a lone unmount record that ends right at the head.
fn log_clean(p: &mut Part, fs: &Xfs, log: Log) -> Res<()> {
    let n = log.blocks;
    let first = cycle_at(p, &log, 0)?;
    let last = cycle_at(p, &log, n - 1)?;
    // (head, cycle of the blocks before it, cycle of the blocks from it on)
    let (head, before, after) = if first == last {
        // The last pass ended right at the end of the log.
        (0, first, first)
    } else {
        ensure(first == last.wrapping_add(1), "log cycles don't follow each other")?;
        // The first block of the previous pass, by bisection.
        let (mut lo, mut hi) = (0, n - 1);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            match cycle_at(p, &log, mid)? {
                c if c == first => lo = mid,
                c if c == last => hi = mid,
                _ => return Err(Bad("log cycles out of order".into())),
            }
        }
        (hi, first, last)
    };
    let window = LOG_WINDOW.min(n / 2).max(1);
    // Before the head (this pass only), and from it on (up to the end of the log).
    let back = if head == 0 { window } else { window.min(head) };
    let from = if head == 0 { n - back } else { head - back };
    let before_blocks = log_blocks(p, &log, from, back)?;
    ensure(before_blocks.as_chunks::<512>().0.iter().all(|b| cycle(b) == before), "torn writes before the log head")?;
    let ahead = if head == 0 { window } else { window.min(n - head) };
    let after_blocks = log_blocks(p, &log, head, ahead)?;
    ensure(after_blocks.as_chunks::<512>().0.iter().all(|b| cycle(b) == after), "log written past its head")?;
    // The last record header before the head.
    let found = before_blocks.as_chunks::<512>().0.iter().rposition(|b| be32_at(b, 0) == LOG_MAGIC);
    let at = found.ok_or_else(|| Bad("no log record near the head (log not clean)".into()))?;
    let at = (from + at as u64) % n;
    let rh = &before_blocks[((at + n - from) % n * 512) as usize..][..512];
    let version = be32_at(rh, 8);
    let len = u64::from(be32_at(rh, 12));
    let size = u64::from(be32_at(rh, 320));
    ensure(be32_at(rh, 4) == before && matches!(version, 1 | 2) && len <= 256 << 10, "bad log record")?;
    ensure(rh[304..320] == log.uuid, "log from another file system")?;
    let header_blocks = if log.v2 && version & 2 != 0 && size > 32 << 10 {
        ensure(size <= 256 << 10, "bad log record")?;
        size.div_ceil(32 << 10)
    } else {
        1
    };
    let data = (at + header_blocks) % n;
    ensure((data + len.div_ceil(512)) % n == head, "log not clean (records after the last unmount)")?;
    ensure(be32_at(rh, 40) == 1, "log not clean (no unmount record)")?;
    let record = log_blocks(p, &log, data, len.div_ceil(512).max(1))?;
    // The operation header: client XFS_LOG (0xAA), flag XLOG_UNMOUNT_TRANS (0x20).
    ensure(record[8] == 0xAA && record[9] & 0x20 != 0, "log not clean (no unmount record)")?;
    // Written by the kernel, the record has a CRC (mkfs leaves it zero).
    let crc = u32_at(rh, 32);
    if fs.v5 && crc != 0 {
        let extra = if len > 32 << 10 { log_blocks(p, &log, at + 1, len.div_ceil(32 << 10) - 1)? } else { Vec::new() };
        let payload = &record[..len as usize];
        // The header's size differs between i386 and other architectures (328 or 324 bytes).
        let ok = [328usize, 324].iter().any(|&h| {
            let mut c = crc32c(crc32c(crc32c(!0, &rh[..32]), &[0; 4]), &rh[36..h]);
            for x in extra.as_chunks::<512>().0 {
                c = crc32c(c, &x[..4 + 256]);
            }
            !crc32c(c, payload) == crc
        });
        ensure(ok, "log record checksum mismatch")?;
    }
    Ok(())
}

/// Marks what allocation group `agno` uses: all but the free extents its bnobt lists.
fn allocation_group(p: &mut Part, fs: &Xfs, agno: u64, used: &mut Ranges) -> Res<()> {
    let ag_start = agno * fs.agblocks;
    let len = fs.agblocks.min(fs.dblocks - ag_start);
    let base = ag_start * fs.block;
    // The superblock copy, AGF, AGI and AGFL: one sector each.
    let headers = p.read_vec(base, 4 * fs.sect as usize)?;
    let (agf, agi, agfl) =
        (&headers[fs.sect as usize..], &headers[2 * fs.sect as usize..], &headers[3 * fs.sect as usize..]);
    let (agf, agi, agfl) = (&agf[..fs.sect as usize], &agi[..fs.sect as usize], &agfl[..fs.sect as usize]);
    ensure(at(agf, 0, b"XAGF") && be32_at(agf, 4) == 1 && u64::from(be32_at(agf, 8)) == agno, "bad AGF")?;
    ensure(at(agi, 0, b"XAGI") && be32_at(agi, 4) == 1 && u64::from(be32_at(agi, 8)) == agno, "bad AGI")?;
    ensure(u64::from(be32_at(agf, 12)) == len && u64::from(be32_at(agi, 12)) == len, "bad AG length")?;
    if fs.v5 {
        ensure(crc_ok(agf, 216) && agf[64..80] == fs.uuid, "AGF checksum mismatch")?;
        ensure(crc_ok(agi, 312) && agi[296..312] == fs.uuid, "AGI checksum mismatch")?;
        ensure(at(agfl, 0, b"XAFL") && u64::from(be32_at(agfl, 4)) == agno, "bad AGFL")?;
        ensure(crc_ok(agfl, 32) && agfl[8..24] == fs.uuid, "AGFL checksum mismatch")?;
    }
    used.add(base, (4 * fs.sect).max(fs.block).min(len * fs.block));
    let freeblks = u64::from(be32_at(agf, 52));
    // Free extents by block number; the gaps between them are used.
    let (magic, root, levels) = (if fs.v5 { b"AB3B" } else { b"ABTB" }, be32_at(agf, 16), be32_at(agf, 28));
    let mut next = 0u64;
    let (mut free, mut records) = (0u64, 0u64);
    leaves(p, fs, agno, len, root, levels, magic, |rec| {
        let (start, count) = (u64::from(be32_at(rec, 0)), u64::from(be32_at(rec, 4)));
        ensure(count > 0 && start >= next && start + count <= len, "free space records out of order")?;
        used.add(base + next * fs.block, (start - next) * fs.block);
        next = start + count;
        (free, records) = (free + count, records + 1);
        Ok(())
    })?;
    used.add(base + next * fs.block, (len - next) * fs.block);
    if p.opts.mode == Mode::Copy {
        ensure(free == freeblks, "free block count doesn't match the free space tree")?;
        // The other free space tree, by size, must list as much.
        let (magic, root, levels) = (if fs.v5 { b"AB3C" } else { b"ABTC" }, be32_at(agf, 20), be32_at(agf, 32));
        let (mut by_size, mut by_size_records) = (0u64, 0u64);
        let mut prev = (0u64, 0u64);
        leaves(p, fs, agno, len, root, levels, magic, |rec| {
            let (start, count) = (u64::from(be32_at(rec, 0)), u64::from(be32_at(rec, 4)));
            ensure(count > 0 && (count, start) > prev && start + count <= len, "free space records out of order")?;
            prev = (count, start);
            (by_size, by_size_records) = (by_size.saturating_add(count), by_size_records + 1);
            Ok(())
        })?;
        ensure(by_size == free && by_size_records == records, "free space trees disagree")?;
    }
    Ok(())
}

/// Calls `f` for each record (8 bytes) of a short-form B+tree of allocation group `agno`
/// (`len` blocks long), in order: down its left edge, then along the leaves.
#[allow(clippy::too_many_arguments)]
fn leaves(
    p: &mut Part,
    fs: &Xfs,
    agno: u64,
    len: u64,
    root: u32,
    levels: u32,
    magic: &[u8; 4],
    mut f: impl FnMut(&[u8]) -> Res<()>,
) -> Res<()> {
    ensure((1..=MAX_LEVELS).contains(&levels), "bad free space tree height")?;
    let header = if fs.v5 { 56 } else { 16 };
    let bs = fs.block as usize;
    let mut level = levels - 1;
    let mut blk = root;
    loop {
        let b = tree_block(p, fs, agno, len, blk, magic, level)?;
        ensure(be32_at(&b, 8) == NULL_BLOCK, "free space tree's left edge has a left sibling")?;
        if level == 0 {
            break;
        }
        let max = (bs - header) / 12;
        ensure((1..=max).contains(&usize::from(be16_at(&b, 6))), "bad free space tree node")?;
        blk = be32_at(&b, header + max * 8);
        level -= 1;
    }
    let mut prev = NULL_BLOCK;
    for _ in 0..len {
        let b = tree_block(p, fs, agno, len, blk, magic, 0)?;
        ensure(be32_at(&b, 8) == prev, "free space tree siblings don't match")?;
        let n = usize::from(be16_at(&b, 6));
        ensure(n <= (bs - header) / 8, "bad free space tree leaf")?;
        for rec in b[header..header + n * 8].as_chunks::<8>().0 {
            f(rec)?;
        }
        match be32_at(&b, 12) {
            NULL_BLOCK => return Ok(()),
            right => (prev, blk) = (blk, right),
        }
    }
    Err(Bad("free space tree leaves loop".into()))
}

/// Reads and checks block `agbno` of allocation group `agno` as a B+tree block.
#[allow(clippy::too_many_arguments)]
fn tree_block(p: &mut Part, fs: &Xfs, agno: u64, len: u64, agbno: u32, magic: &[u8; 4], level: u32) -> Res<Vec<u8>> {
    ensure(agbno != 0 && u64::from(agbno) < len, "free space tree block outside its AG")?;
    let linear = agno * fs.agblocks + u64::from(agbno);
    let b = p.read_vec(linear * fs.block, fs.block as usize)?;
    ensure(at(&b, 0, magic) && u32::from(be16_at(&b, 4)) == level, "bad free space tree block")?;
    if fs.v5 {
        ensure(crc_ok(&b, 52), "free space tree block checksum mismatch")?;
        let daddr = linear * (fs.block / 512);
        ensure(
            be64_at(&b, 16) == daddr && b[32..48] == fs.uuid && u64::from(be32_at(&b, 48)) == agno,
            "misplaced tree block",
        )?;
    }
    Ok(b)
}
