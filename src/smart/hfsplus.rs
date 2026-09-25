//! HFS+ and HFSX: the allocation file is a bitmap with one bit per allocation block (the
//! most significant bit of its first byte is block 0). Its first eight extents are in the
//! volume header, any others in the extents overflow B-tree.
//!
//! Always kept, whatever the bitmap says: the boot blocks and the volume header (the first
//! 1536 bytes), the alternate volume header and the last 1024 bytes of the volume, the
//! special files the volume header lists (allocation, extents overflow, catalog, attributes
//! and startup files), every extent recorded in the extents overflow file, and the journal
//! with its info block.
//!
//! For a copy, the volume must have been cleanly unmounted, its journal must be empty, and
//! the bitmap must agree with the volume header's free block count. Otherwise (or when
//! anything looks off) it's copied in full.
//!
//! An HFS+ volume embedded in an HFS wrapper (a "BD" master directory block pointing at an
//! "H+" volume, as Mac OS 8 and 9 made them) is read where it is; the wrapper around it
//! is kept as is. Classic HFS is only recognised (see `classic`).

use super::util::{Bad, CHUNK, Mode, Part, Ranges, Res, Usage, at, be16_at, be32_at, bit_runs, ensure, u32_at, u64_at};

/// Volume attributes: cleanly unmounted.
const UNMOUNTED: u32 = 1 << 8;
/// Set while mounted by Linux, cleared on unmount (kHFSBootVolumeInconsistentBit).
const BOOT_INCONSISTENT: u32 = 1 << 11;
const JOURNALED: u32 = 1 << 13;
/// The volume needs checking (kHFSVolumeInconsistentBit).
const INCONSISTENT: u32 = 1 << 14;

/// Journal info block flags.
const JOURNAL_IN_FS: u32 = 1;
const JOURNAL_ON_OTHER_DEVICE: u32 = 2;
/// A new journal gets created at the next mount: there's nothing to replay.
const JOURNAL_NEED_INIT: u32 = 4;
/// Journal header magic numbers ("JNLx", and the older "JHDR"), in the byte order of the
/// machine that wrote the journal.
const JOURNAL_MAGIC: u32 = 0x4A4E_4C78;
const JOURNAL_OLD_MAGIC: u32 = 0x4A48_4452;
const JOURNAL_ENDIAN: u32 = 0x1234_5678;
/// The journal header's checksum covers the fields before `sequence_num`.
const JOURNAL_CHECKSUMMED: usize = 44;

/// Catalog node ID of the allocation file (keys its extents overflow records).
const ALLOCATION_FILE: u32 = 6;
/// B-tree nodes are at most this big.
const MAX_NODE: u64 = 32 << 10;
/// Allocation blocks are at most this big (they're 4 KiB in practice).
const MAX_BLOCK: u64 = 1 << 20;

/// Where the volume header is, relative to the volume.
const HEADER: u64 = 1024;
/// The boot blocks and the volume header.
const HEAD: u64 = 1536;

/// An HFS+ or HFSX volume header at 1 KiB, or an HFS wrapper holding an HFS+ volume.
pub(crate) fn detect(head: &[u8]) -> bool {
    plus(head, HEADER as usize) || (at(head, HEADER as usize, b"BD") && embeds(head))
}

/// Classic HFS: a master directory block at 1 KiB that doesn't embed an HFS+ volume.
/// Recognised by name only, as "hfs" (probe.rs lists it).
pub(crate) fn classic(head: &[u8]) -> bool {
    let mdb = HEADER as usize;
    let block = be32_at(head, mdb + 0x14);
    at(head, mdb, b"BD") && !embeds(head) && block != 0 && block.is_multiple_of(512) && be16_at(head, mdb + 0x12) != 0
}

/// A volume header signature: "H+" version 4, or "HX" (HFSX) version 5.
fn plus(b: &[u8], pos: usize) -> bool {
    (at(b, pos, b"H+") && be16_at(b, pos + 2) == 4) || (at(b, pos, b"HX") && be16_at(b, pos + 2) == 5)
}

/// Whether the HFS master directory block at 1 KiB says it embeds an HFS+ volume.
fn embeds(head: &[u8]) -> bool {
    at(head, HEADER as usize + 0x7C, b"H+") || at(head, HEADER as usize + 0x7C, b"HX")
}

fn be64_at(b: &[u8], pos: usize) -> u64 {
    u64::from(be32_at(b, pos)) << 32 | u64::from(be32_at(b, pos.saturating_add(4)))
}

/// What the volume header says.
struct Volume {
    /// Where the volume starts in the partition (past the wrapper, when it's embedded).
    offset: u64,
    block: u64,
    blocks: u64,
    free: u64,
    /// `blocks * block`.
    size: u64,
}

impl Volume {
    /// Partition offset of allocation block `b`.
    fn at(&self, b: u64) -> u64 {
        self.offset + b * self.block
    }
}

/// Extents in allocation blocks: (first, count).
type Extents = Vec<(u64, u64)>;

/// A file's data fork: its size and its extents.
struct Fork {
    logical: u64,
    blocks: u64,
    extents: Extents,
}

impl Fork {
    fn covered(&self) -> u64 {
        self.extents.iter().map(|&(_, n)| n).sum()
    }
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    // The volume at the start of the partition, or inside an HFS wrapper.
    let (offset, room) = if plus(head, HEADER as usize) { (0, p.size) } else { wrapper(head, p.size)? };
    let vh = p.read_vec(offset + HEADER, 512)?;
    ensure(plus(&vh, 0), "no HFS+ volume header in the wrapper")?;
    let block = u64::from(be32_at(&vh, 0x28));
    ensure(block.is_power_of_two() && (512..=MAX_BLOCK).contains(&block), "bad allocation block size")?;
    let blocks = u64::from(be32_at(&vh, 0x2C));
    let free = u64::from(be32_at(&vh, 0x30));
    ensure(free <= blocks, "more free blocks than blocks")?;
    let size = blocks * block;
    ensure(size >= HEAD + HEADER, "volume too small")?;
    ensure(size <= room, "file system larger than its partition")?;
    let v = Volume { offset, block, blocks, free, size };

    let attributes = be32_at(&vh, 0x04);
    if p.opts.mode == Mode::Copy {
        ensure(attributes & UNMOUNTED != 0, "not cleanly unmounted")?;
        ensure(attributes & (BOOT_INCONSISTENT | INCONSISTENT) == 0, "marked inconsistent")?;
    }

    let mut used = p.ranges();
    // Whatever surrounds an embedded volume (the HFS wrapper) stays as it is.
    used.add(0, offset);
    used.add(offset, HEAD);
    if attributes & JOURNALED != 0 {
        journal(p, &v, u64::from(be32_at(&vh, 0x0C)), &mut used)?;
    }

    // The special files: allocation, extents overflow, catalog, attributes, startup. Only
    // the extents overflow file can't have extents in itself.
    let mut allocation = fork(&vh, 0x70, &v)?;
    let extents = fork(&vh, 0xC0, &v)?;
    let catalog = fork(&vh, 0x110, &v)?;
    let attributes_file = fork(&vh, 0x160, &v)?;
    let startup = fork(&vh, 0x1B0, &v)?;
    ensure(extents.covered() == extents.blocks, "extents overflow file extents missing")?;
    for f in [&allocation, &extents, &catalog, &attributes_file, &startup] {
        for &(first, n) in &f.extents {
            used.add(v.at(first), n * v.block);
        }
    }

    // Every extent in the extents overflow file, among them the allocation file's other
    // extents (when it has more than eight).
    let more = overflow(p, &v, &extents, &mut used)?;
    if allocation.covered() < allocation.blocks {
        ensure(allocation.extents.len() == 8, "allocation file extents missing")?;
        for (first, list) in more {
            ensure(first == allocation.covered(), "allocation file extents don't line up")?;
            allocation.extents.extend(list);
        }
    } else {
        ensure(more.is_empty(), "stray allocation file extents")?;
    }
    ensure(allocation.covered() == allocation.blocks, "allocation file extents missing")?;

    // The bitmap.
    let need = blocks.div_ceil(8);
    ensure(allocation.logical >= need && allocation.blocks * block >= need, "allocation file too small")?;
    let mut done = 0;
    let mut set = 0;
    for &(first, n) in &allocation.extents {
        let bytes = n * block;
        let mut within = 0;
        while within < bytes && done < need {
            let len = (CHUNK as u64).min(bytes - within).min(need - done);
            let mut map = p.read_vec(v.at(first) + within, len as usize)?;
            // Most significant bit first, where `bit_runs` wants the least significant one.
            map.iter_mut().for_each(|b| *b = b.reverse_bits());
            let bits = (blocks - done * 8).min(len * 8);
            bit_runs(&map, bits, done * 8, |b, count| {
                used.add(v.at(b), count * block);
                set += count;
            });
            done += len;
            within += len;
        }
    }
    ensure(done == need, "allocation file too small")?;
    if p.opts.mode == Mode::Copy {
        ensure(blocks - set == v.free, "the allocation bitmap disagrees with the free block count")?;
    }

    // The alternate volume header and the reserved sector after it.
    used.add(offset + size - HEADER, HEADER);
    *label = volume_name(p, &v, &catalog);
    Ok(Usage { used, end: offset + size })
}

/// Where the HFS+ volume embedded in an HFS wrapper is: its offset and the room it has.
fn wrapper(head: &[u8], part_size: u64) -> Res<(u64, u64)> {
    let mdb = HEADER as usize;
    ensure(at(head, mdb, b"BD") && embeds(head), "not HFS+")?;
    // Allocation blocks of the wrapper: their size, and where the first one is (in sectors).
    let block = u64::from(be32_at(head, mdb + 0x14));
    ensure(block != 0 && block.is_multiple_of(512), "bad HFS wrapper allocation block size")?;
    let first = u64::from(be16_at(head, mdb + 0x1C)) * 512;
    let start = u64::from(be16_at(head, mdb + 0x7E));
    let count = u64::from(be16_at(head, mdb + 0x80));
    let offset = first + start * block;
    let len = count * block;
    let inside = offset >= HEAD && offset.checked_add(len).is_some_and(|end| end <= part_size);
    ensure(len > 0 && inside, "embedded HFS+ volume outside its wrapper")?;
    Ok((offset, len))
}

/// A fork from the volume header (`HFSPlusForkData` at `pos`), checked against the volume.
fn fork(vh: &[u8], pos: usize, v: &Volume) -> Res<Fork> {
    let logical = be64_at(vh, pos);
    let blocks = u64::from(be32_at(vh, pos + 12));
    let mut extents = Vec::new();
    let mut ended = false;
    for i in 0..8 {
        let first = u64::from(be32_at(vh, pos + 16 + 8 * i));
        let n = u64::from(be32_at(vh, pos + 20 + 8 * i));
        if n == 0 {
            ended = true;
            continue;
        }
        ensure(!ended && first + n <= v.blocks, "bad special file extent")?;
        extents.push((first, n));
    }
    let f = Fork { logical, blocks, extents };
    ensure(logical <= blocks * v.block && f.covered() <= blocks, "special file larger than its extents")?;
    Ok(f)
}

/// The journal info block and the journal are kept. For a copy, the journal must be empty:
/// replaying it could allocate blocks the bitmap on disk calls free.
fn journal(p: &mut Part, v: &Volume, info: u64, used: &mut Ranges) -> Res<()> {
    ensure(info > 0 && info < v.blocks, "journal info block outside the volume")?;
    let jib = p.read_vec(v.at(info), 52)?;
    used.add(v.at(info), v.block);
    let flags = be32_at(&jib, 0);
    ensure(flags & JOURNAL_ON_OTHER_DEVICE == 0, "journal on another device")?;
    ensure(flags & JOURNAL_IN_FS != 0, "journal outside the volume")?;
    let (start, len) = (be64_at(&jib, 36), be64_at(&jib, 44));
    let inside = start >= HEAD && start.checked_add(len).is_some_and(|end| end <= v.size);
    ensure(len >= 512 && inside, "journal outside the volume")?;
    used.add(v.offset + start, len);
    if p.opts.mode == Mode::Estimate || flags & JOURNAL_NEED_INIT != 0 {
        return Ok(());
    }
    let mut h = p.read_vec(v.offset + start, 48)?;
    // In the byte order of the machine that wrote it.
    let big = be32_at(&h, 4) == JOURNAL_ENDIAN;
    let field32 = |h: &[u8], pos| if big { be32_at(h, pos) } else { u32_at(h, pos) };
    let field64 = |h: &[u8], pos| if big { be64_at(h, pos) } else { u64_at(h, pos) };
    ensure(field32(&h, 4) == JOURNAL_ENDIAN, "bad journal header")?;
    let magic = field32(&h, 0);
    ensure(magic == JOURNAL_MAGIC || magic == JOURNAL_OLD_MAGIC, "bad journal header")?;
    let (first, next) = (field64(&h, 8), field64(&h, 16));
    if magic == JOURNAL_MAGIC {
        let stored = field32(&h, 36);
        h[36..40].fill(0);
        ensure(journal_checksum(&h[..JOURNAL_CHECKSUMMED]) == stored, "journal header checksum mismatch")?;
    }
    ensure(first == next, "journal needs replaying (not cleanly unmounted)")
}

/// The journal header checksum (`calc_checksum` in xnu's vfs_journal.c).
fn journal_checksum(b: &[u8]) -> u32 {
    !b.iter().fold(0u32, |c, &x| (c << 8) ^ c.wrapping_add(u32::from(x)))
}

/// A B-tree file's header record.
struct Tree {
    node: u64,
    nodes: u64,
    depth: u16,
    leaf_records: u64,
    first_leaf: u64,
}

/// Reads `len` bytes at `pos` in a file with extents `ext`.
fn read_file(p: &mut Part, v: &Volume, ext: &[(u64, u64)], pos: u64, len: u64) -> Res<Vec<u8>> {
    let mut out = Vec::with_capacity(len as usize);
    let mut file_at = 0;
    for &(first, n) in ext {
        let bytes = n * v.block;
        let want = pos + out.len() as u64;
        if want < file_at + bytes && (out.len() as u64) < len {
            let within = want - file_at;
            let n = (bytes - within).min(len - out.len() as u64);
            out.extend(p.read_vec(v.at(first) + within, n as usize)?);
        }
        file_at += bytes;
    }
    ensure(out.len() as u64 == len, "B-tree node past the end of its file")?;
    Ok(out)
}

fn tree(p: &mut Part, v: &Volume, f: &Fork) -> Res<Tree> {
    let head = read_file(p, v, &f.extents, 0, 512)?;
    ensure(head[8] == 1 && be16_at(&head, 10) == 3, "bad B-tree header node")?;
    let node = u64::from(be16_at(&head, 32));
    let nodes = u64::from(be32_at(&head, 36));
    ensure(node.is_power_of_two() && (512..=MAX_NODE).contains(&node), "bad B-tree node size")?;
    ensure(nodes > 0 && nodes * node <= f.blocks * v.block, "B-tree larger than its file")?;
    let t = Tree {
        node,
        nodes,
        depth: be16_at(&head, 14),
        leaf_records: u64::from(be32_at(&head, 20)),
        first_leaf: u64::from(be32_at(&head, 24)),
    };
    let empty = t.depth == 0 && t.first_leaf == 0 && t.leaf_records == 0;
    ensure(empty || (t.depth > 0 && t.first_leaf > 0 && t.first_leaf < nodes), "bad B-tree header")?;
    Ok(t)
}

/// Record `i` of a B-tree node: where it starts, and how far it may go.
fn record(node: &[u8], i: usize) -> Res<(usize, usize)> {
    let count = usize::from(be16_at(node, 10));
    let table = node.len().checked_sub(2 * (count + 1)).filter(|_| i < count);
    let table = table.ok_or_else(|| Bad("bad B-tree node".into()))?;
    let start = usize::from(be16_at(node, node.len() - 2 * (i + 1)));
    ensure((14..table).contains(&start), "bad B-tree record offset")?;
    Ok((start, table))
}

/// Walks the leaves of the extents overflow file: every extent there is used. Returns the
/// allocation file's extra extents, by the file block they start at, in order.
fn overflow(p: &mut Part, v: &Volume, f: &Fork, used: &mut Ranges) -> Res<Vec<(u64, Extents)>> {
    let t = tree(p, v, f)?;
    let mut allocation = Vec::new();
    let (mut n, mut seen, mut records) = (t.first_leaf, 0, 0);
    while n != 0 {
        seen += 1;
        ensure(n < t.nodes && seen <= t.nodes, "bad extents overflow leaf chain")?;
        let node = read_file(p, v, &f.extents, n * t.node, t.node)?;
        ensure(node[8] == 0xFF && node[9] == 1, "bad extents overflow leaf")?;
        let count = usize::from(be16_at(&node, 10));
        for i in 0..count {
            let (at, limit) = record(&node, i)?;
            // Key: length (10), fork type, pad, file ID, first file block; then 8 extents.
            ensure(at + 76 <= limit && be16_at(&node, at) == 10, "bad extents overflow record")?;
            let (fork_type, file, first) = (node[at + 2], be32_at(&node, at + 4), u64::from(be32_at(&node, at + 8)));
            let mut list = Vec::new();
            for e in 0..8 {
                let start = u64::from(be32_at(&node, at + 12 + 8 * e));
                let len = u64::from(be32_at(&node, at + 16 + 8 * e));
                if len == 0 {
                    break;
                }
                ensure(start + len <= v.blocks, "extent outside the volume")?;
                used.add(v.at(start), len * v.block);
                list.push((start, len));
            }
            if file == ALLOCATION_FILE && fork_type == 0 {
                allocation.push((first, list));
            }
        }
        records += count as u64;
        n = u64::from(be32_at(&node, 0));
    }
    ensure(records == t.leaf_records, "extents overflow record count mismatch")?;
    allocation.sort_by_key(|&(first, _)| first);
    Ok(allocation)
}

/// The volume's name: the root folder's key, first in the catalog. None when it can't be
/// told (it's only a label).
fn volume_name(p: &mut Part, v: &Volume, catalog: &Fork) -> Option<String> {
    let t = tree(p, v, catalog).ok().filter(|t| t.depth > 0)?;
    let node = read_file(p, v, &catalog.extents, t.first_leaf * t.node, t.node).ok()?;
    let (at, limit) = record(&node, 0).ok().filter(|_| node[8] == 0xFF)?;
    // Key: length, parent ID (1 for the root folder), name (length, UTF-16BE).
    let key = usize::from(be16_at(&node, at));
    let len = usize::from(be16_at(&node, at + 6));
    let data = at + 2 + key;
    if be32_at(&node, at + 2) != 1 || len > 255 || key < 6 + 2 * len || data + 12 > limit {
        return None;
    }
    // A folder record for the root folder (ID 2).
    if be16_at(&node, data) != 1 || be32_at(&node, data + 8) != 2 {
        return None;
    }
    let name: Vec<u16> = (0..len).map(|i| be16_at(&node, at + 8 + 2 * i)).collect();
    let name = String::from_utf16_lossy(&name);
    let name = name.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!name.is_empty()).then(|| name.to_owned())
}
