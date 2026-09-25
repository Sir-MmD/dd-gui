//! APFS: the container's space manager keeps one bitmap block per chunk of blocks (a bit
//! per block), listed in chunk-info blocks, themselves listed in the space manager or, in
//! big containers, in chunk-info address blocks. Volumes share the container's blocks, so
//! this is all there is to it, encrypted volumes included (the space manager isn't
//! encrypted).
//!
//! The container superblock in block 0 locates the checkpoint descriptor area; the latest
//! valid checkpoint there (highest transaction ID, good checksum) is the one read. Its
//! checkpoint map locates the ephemeral objects in the checkpoint data area: the space
//! manager and its free queues.
//!
//! Always kept, whatever the bitmaps say: block 0, both checkpoint areas (the reaper and the
//! rest of the ephemeral objects are in there), the space manager's internal pool (where
//! the bitmaps and chunk-info blocks are) and its bitmap ring, every chunk-info and bitmap
//! block, whatever the free queues hold (freed, but not reusable yet: older checkpoints may
//! still need it), the object map and its trees, the volume superblocks, the EFI driver,
//! and the key lockers.
//!
//! Every object read is checked against its checksum (bitmaps have none: each must agree
//! with its chunk's free count instead). For a copy, block 0 must match the latest
//! checkpoint, as it does after a clean unmount. Fusion drives (two devices), containers
//! being resized, and anything unexpected are copied in full.

use super::util::{Bad, CHUNK, Mode, Part, Ranges, Res, Usage, at, bit_runs, ensure, u16_at, u32_at, u64_at};
use std::collections::HashMap;

// Object types (the low 16 bits of `o_type`) and storage flags.
const NX_SUPERBLOCK: u32 = 0x01;
const BTREE: u32 = 0x02;
const BTREE_NODE: u32 = 0x03;
const SPACEMAN: u32 = 0x05;
const SPACEMAN_CAB: u32 = 0x06;
const SPACEMAN_CIB: u32 = 0x07;
const SPACEMAN_FREE_QUEUE: u32 = 0x09;
const OMAP: u32 = 0x0B;
const CHECKPOINT_MAP: u32 = 0x0C;
const FS: u32 = 0x0D;
const OMAP_SNAPSHOT: u32 = 0x13;
const EFI_JUMPSTART: u32 = 0x14;
const EPHEMERAL: u32 = 0x8000_0000;
const PHYSICAL: u32 = 0x4000_0000;

const INCOMPAT_VERSION2: u64 = 0x2;
const INCOMPAT_FUSION: u64 = 0x100;
const CHECKPOINT_MAP_LAST: u32 = 1;
/// Space manager flag: the structure has a size field (and allocation zones).
const SM_VERSIONED: u32 = 1;
/// The space manager's size without that flag.
const SM_SIZE: u64 = 0x150;
/// Object map value flags that can't be on a volume superblock's mapping: deleted,
/// encrypted, without a header.
const OMAP_VALUE_ODD: u32 = 0x1 | 0x4 | 0x8;
const EFI_MAGIC: u32 = 0x5244_534A;
const MAX_VOLUMES: usize = 100;
/// Ghost records (no value) in B-trees that allow them: a free queue's single blocks.
const GHOST: u16 = 0xFFFF;

/// Largest checkpoint descriptor area read, looking for the latest checkpoint.
const MAX_DESCRIPTORS: u64 = 256 << 20;
/// Largest ephemeral object read (the space manager may take a few blocks).
const MAX_EPHEMERAL: u64 = 16 << 20;
/// Nodes read at most in one B-tree, and how deep it may be.
const MAX_NODES: usize = 1 << 17;
const MAX_DEPTH: u16 = 16;

/// A container superblock in block 0.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 32, b"NXSB")
}

/// The Fletcher-64 checksum over everything after the checksum field, as stored in it.
fn checksum(o: &[u8]) -> u64 {
    const MOD: u64 = 0xFFFF_FFFF;
    let (mut lo, mut hi) = (0u64, 0u64);
    for chunk in o.get(8..).unwrap_or_default().chunks(1024) {
        // Sums of up to 1024 words fit in 64 bits before they're reduced.
        for w in chunk.as_chunks::<4>().0 {
            lo += u64::from(u32::from_le_bytes(*w));
            hi += lo;
        }
        lo %= MOD;
        hi %= MOD;
    }
    let c1 = MOD - (lo + hi) % MOD;
    let c2 = MOD - (lo + c1) % MOD;
    c2 << 32 | c1
}

fn checked(o: &[u8]) -> bool {
    o.len() >= 32 && checksum(o) == u64_at(o, 0)
}

/// A checkpoint mapping: where an ephemeral object is.
struct Mapping {
    kind: u32,
    subtype: u32,
    size: u64,
    addr: u64,
}

/// The container, as its latest checkpoint has it.
struct Nx {
    block: u64,
    blocks: u64,
    xid: u64,
    /// The latest checkpoint's container superblock.
    latest: Vec<u8>,
    data_base: u64,
    data_blocks: u64,
    ephemeral: HashMap<u64, Mapping>,
}

impl Nx {
    /// A physical object: `kind` with the physical storage flag, its address as its ID.
    fn physical(&self, p: &mut Part, addr: u64, kind: u32, subtype: u32) -> Res<Vec<u8>> {
        ensure(addr > 0 && addr < self.blocks, "object outside the container")?;
        let o = p.read_vec(addr * self.block, self.block as usize)?;
        ensure(checked(&o), "object checksum mismatch")?;
        let expected = u64_at(&o, 8) == addr && u32_at(&o, 24) == kind | PHYSICAL && u32_at(&o, 28) == subtype;
        ensure(expected && u64_at(&o, 16) <= self.xid, "unexpected object")?;
        Ok(o)
    }

    /// An ephemeral object of the checkpoint, from the checkpoint data area (where it may
    /// wrap around the end).
    fn ephemeral(&self, p: &mut Part, oid: u64, kind: u32, subtype: u32) -> Res<Vec<u8>> {
        let m = self.ephemeral.get(&oid).ok_or_else(|| Bad("ephemeral object missing from the checkpoint".into()))?;
        ensure(m.kind == kind | EPHEMERAL && m.subtype == subtype, "unexpected ephemeral object")?;
        let mut o = Vec::with_capacity(m.size as usize);
        let mut at = m.addr - self.data_base;
        while (o.len() as u64) < m.size {
            let n = (self.data_blocks - at).min((m.size - o.len() as u64) / self.block);
            o.extend(p.read_vec((self.data_base + at) * self.block, (n * self.block) as usize)?);
            at = (at + n) % self.data_blocks;
        }
        ensure(checked(&o), "object checksum mismatch")?;
        let expected = u64_at(&o, 8) == oid && u32_at(&o, 24) == m.kind && u32_at(&o, 28) == m.subtype;
        ensure(expected && u64_at(&o, 16) == self.xid, "unexpected ephemeral object")?;
        Ok(o)
    }

    /// Keeps blocks `first..first + n`, which must be in the container.
    fn keep(&self, used: &mut Ranges, first: u64, n: u64) -> Res<()> {
        ensure(first.checked_add(n).is_some_and(|end| end <= self.blocks), "range outside the container")?;
        used.add(first * self.block, n * self.block);
        Ok(())
    }
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    ensure(at(head, 32, b"NXSB"), "not APFS")?;
    let block = u64::from(u32_at(head, 36));
    ensure(block.is_power_of_two() && (4096..=65536).contains(&block), "bad block size")?;
    let zero = p.read_vec(0, block as usize)?;
    ensure(checked(&zero), "container superblock checksum mismatch")?;
    ensure(u64_at(&zero, 8) == 1 && u32_at(&zero, 24) & 0xFFFF == NX_SUPERBLOCK, "bad container superblock")?;
    // Every block address checked against this is then a byte offset inside the partition.
    let fits = u64_at(&zero, 0x28).checked_mul(block).is_some_and(|size| size <= p.size);
    ensure(fits, "file system larger than its partition")?;
    let nx = checkpoint(p, &zero)?;
    let sb = &nx.latest;
    let incompat = u64_at(sb, 0x40);
    let fusion = [0x500, 0x508, 0x548, 0x550, 0x558, 0x560].iter().any(|&at| u64_at(sb, at) != 0);
    ensure(incompat & INCOMPAT_FUSION == 0 && !fusion, "Fusion drive (two devices)")?;
    ensure(incompat == INCOMPAT_VERSION2, "unsupported APFS version or features")?;
    ensure(u64_at(sb, 0x4E0) == 0 && u64_at(sb, 0x4E8) == 0, "container being resized")?;
    ensure(u32_at(sb, 0xB0) == 0 && u64_at(sb, 0x540) == 0, "test container")?;

    let mut used = p.ranges();
    used.add(0, block);
    // Both checkpoint areas.
    nx.keep(&mut used, u64_at(sb, 0x70), u64::from(u32_at(sb, 0x68)))?;
    nx.keep(&mut used, nx.data_base, nx.data_blocks)?;
    spaceman(p, &nx, u64_at(sb, 0x98), &mut used)?;
    let volumes = object_map(p, &nx, u64_at(sb, 0xA0), &mut used)?;
    *label = volume_names(p, &nx, &volumes, &mut used)?;
    let efi = u64_at(sb, 0x4F8);
    if efi != 0 {
        let o = nx.physical(p, efi, EFI_JUMPSTART, 0)?;
        used.add(efi * block, block);
        let n = u64::from(u32_at(&o, 0x2C));
        ensure(u32_at(&o, 0x20) == EFI_MAGIC && 0xB0 + 16 * n <= block, "bad EFI driver record")?;
        for i in 0..n as usize {
            nx.keep(&mut used, u64_at(&o, 0xB0 + 16 * i), u64_at(&o, 0xB8 + 16 * i))?;
        }
    }
    // The key lockers (encrypted, so there's no telling what's in them).
    for at in [0x510, 0x570] {
        nx.keep(&mut used, u64_at(sb, at), u64_at(sb, at + 8))?;
    }
    Ok(Usage { used, end: nx.blocks * block })
}

/// Finds the latest valid checkpoint in the descriptor area, and reads its checkpoint map.
fn checkpoint(p: &mut Part, zero: &[u8]) -> Res<Nx> {
    let block = u64::from(u32_at(zero, 0x24));
    let blocks = u64_at(zero, 0x28);
    let (desc_blocks, data_blocks) = (u32_at(zero, 0x68), u32_at(zero, 0x6C));
    let (desc_base, data_base) = (u64_at(zero, 0x70), u64_at(zero, 0x78));
    // With the high bit set, an area is a B-tree of blocks here and there.
    ensure(desc_blocks >> 31 == 0 && data_blocks >> 31 == 0, "checkpoint areas in a tree")?;
    let (desc_blocks, data_blocks) = (u64::from(desc_blocks), u64::from(data_blocks));
    let inside = |base: u64, n: u64| base > 0 && n > 0 && base.checked_add(n).is_some_and(|end| end <= blocks);
    ensure(inside(desc_base, desc_blocks) && inside(data_base, data_blocks), "checkpoint areas outside the container")?;
    ensure(desc_blocks * block <= MAX_DESCRIPTORS, "checkpoint descriptor area too large")?;

    // The container superblock with the highest transaction ID.
    let mut latest: Option<(u64, u64, Vec<u8>)> = None;
    let per_read = (CHUNK as u64 / block).max(1);
    let mut i = 0;
    while i < desc_blocks {
        let n = per_read.min(desc_blocks - i);
        let buf = p.read_vec((desc_base + i) * block, (n * block) as usize)?;
        for (k, o) in buf.chunks_exact(block as usize).enumerate() {
            let superblock = at(o, 32, b"NXSB") && u32_at(o, 24) == NX_SUPERBLOCK | EPHEMERAL && u64_at(o, 8) == 1;
            if !superblock || !checked(o) {
                continue;
            }
            let xid = u64_at(o, 16);
            match &latest {
                Some((best, _, _)) if *best > xid => {}
                Some((best, _, _)) if *best == xid => return Err(Bad("two checkpoints with the same ID".into())),
                _ => latest = Some((xid, i + k as u64, o.to_vec())),
            }
        }
        i += n;
    }
    let (xid, index, sb) = latest.ok_or_else(|| Bad("no valid checkpoint".into()))?;
    // Block 0 is written on unmount; an older one means the container wasn't unmounted.
    if p.opts.mode == Mode::Copy {
        ensure(u64_at(zero, 16) == xid, "not cleanly unmounted")?;
    }
    // The same container: block size and count, checkpoint areas.
    ensure(
        sb[0x20..0x30] == zero[0x20..0x30] && sb[0x68..0x80] == zero[0x68..0x80],
        "checkpoint doesn't match block 0",
    )?;

    // The checkpoint's map blocks come right before its superblock (the area is a ring).
    let len = u64::from(u32_at(&sb, 0x8C));
    ensure((1..=desc_blocks).contains(&len), "bad checkpoint length")?;
    let mut ephemeral = HashMap::new();
    for k in 0..len - 1 {
        let addr = desc_base + (index + desc_blocks - (len - 1) + k) % desc_blocks;
        let o = p.read_vec(addr * block, block as usize)?;
        let map = u64_at(&o, 8) == addr && u32_at(&o, 24) == CHECKPOINT_MAP | PHYSICAL && u64_at(&o, 16) == xid;
        ensure(checked(&o) && map, "bad checkpoint map")?;
        let (flags, count) = (u32_at(&o, 0x20), u64::from(u32_at(&o, 0x24)));
        let last = k == len - 2;
        ensure(
            flags == if last { CHECKPOINT_MAP_LAST } else { 0 } && 0x28 + 40 * count <= block,
            "bad checkpoint map",
        )?;
        for e in (0..count as usize).map(|e| 0x28 + 40 * e) {
            let m = Mapping {
                kind: u32_at(&o, e),
                subtype: u32_at(&o, e + 4),
                size: u64::from(u32_at(&o, e + 8)),
                addr: u64_at(&o, e + 32),
            };
            let n = m.size / block;
            let fits = m.size.is_multiple_of(block) && n > 0 && n <= data_blocks && m.size <= MAX_EPHEMERAL;
            let placed = m.addr >= data_base && m.addr - data_base < data_blocks;
            ensure(fits && placed, "bad checkpoint mapping")?;
            ensure(ephemeral.insert(u64_at(&o, e + 24), m).is_none(), "an ephemeral object mapped twice")?;
        }
    }
    Ok(Nx { block, blocks, xid, latest: sb, data_base, data_blocks, ephemeral })
}

/// The space manager: its bitmaps, its internal pool, its free queues.
fn spaceman(p: &mut Part, nx: &Nx, oid: u64, used: &mut Ranges) -> Res<()> {
    let sm = nx.ephemeral(p, oid, SPACEMAN, 0)?;
    let bs = nx.block;
    // A bitmap block per chunk; chunk infos (32 bytes) and addresses after a 40-byte header.
    let (per_chunk, chunks_per_cib, cibs_per_cab) = (8 * bs, (bs - 0x28) / 32, (bs - 0x28) / 8);
    let field = |at| u64::from(u32_at(&sm, at));
    let geometry = [(0x20, bs), (0x24, per_chunk), (0x28, chunks_per_cib), (0x2C, cibs_per_cab)];
    ensure(geometry.iter().all(|&(at, v)| field(at) == v), "unexpected space manager geometry")?;
    let flags = u32_at(&sm, 0x90);
    ensure(flags & !SM_VERSIONED == 0, "unknown space manager flags")?;
    let size = if flags & SM_VERSIONED != 0 { field(0x154) } else { SM_SIZE };
    ensure(size >= SM_SIZE && size <= sm.len() as u64, "bad space manager size")?;
    // The second device's (a Fusion drive's) counts.
    ensure((0x60..0x80).step_by(8).all(|at| u64_at(&sm, at) == 0), "Fusion drive (two devices)")?;
    let (blocks, chunks, cibs, cabs, free) =
        (u64_at(&sm, 0x30), u64_at(&sm, 0x38), field(0x40), field(0x44), u64_at(&sm, 0x48));
    // Address blocks only come in when at least two are needed.
    let cabs_needed = match cibs.div_ceil(cibs_per_cab) {
        1 => 0,
        n => n,
    };
    let counts = blocks == nx.blocks
        && chunks == blocks.div_ceil(per_chunk)
        && cibs == chunks.div_ceil(chunks_per_cib)
        && cabs == cabs_needed
        && free <= blocks;
    ensure(counts, "space manager counts don't add up")?;

    // The chunk-info blocks, listed in the space manager or in address blocks.
    let offset = field(0x50);
    let listed = if cabs > 0 { cabs } else { cibs };
    let fits = offset.is_multiple_of(8) && offset >= size && offset + 8 * listed <= sm.len() as u64;
    ensure(fits, "bad space manager address array")?;
    let listed: Vec<u64> = (0..listed).map(|i| u64_at(&sm, (offset + 8 * i) as usize)).collect();
    let mut cib_addrs = Vec::new();
    if cabs == 0 {
        cib_addrs = listed;
    } else {
        for (i, &addr) in listed.iter().enumerate() {
            let cab = nx.physical(p, addr, SPACEMAN_CAB, 0)?;
            nx.keep(used, addr, 1)?;
            let n = u64::from(u32_at(&cab, 0x24));
            let full = i as u64 + 1 < cabs;
            let ok =
                u64::from(u32_at(&cab, 0x20)) == i as u64 && n > 0 && n <= cibs_per_cab && (!full || n == cibs_per_cab);
            ensure(ok, "bad chunk-info address block")?;
            cib_addrs.extend((0..n as usize).map(|k| u64_at(&cab, 0x28 + 8 * k)));
        }
        ensure(cib_addrs.len() as u64 == cibs, "chunk-info blocks missing")?;
    }
    // Every chunk, in order: (bitmap block, first block, blocks, free blocks).
    let mut bitmaps = Vec::new();
    let (mut next, mut free_total) = (0, 0);
    for (i, &addr) in cib_addrs.iter().enumerate() {
        let cib = nx.physical(p, addr, SPACEMAN_CIB, 0)?;
        nx.keep(used, addr, 1)?;
        let n = u64::from(u32_at(&cib, 0x24));
        let full = i as u64 + 1 < cibs;
        let ok =
            u64::from(u32_at(&cib, 0x20)) == i as u64 && n > 0 && n <= chunks_per_cib && (!full || n == chunks_per_cib);
        ensure(ok, "bad chunk-info block")?;
        for c in (0..n as usize).map(|k| 0x28 + 32 * k) {
            let (first, count, chunk_free) =
                (u64_at(&cib, c + 8), u64::from(u32_at(&cib, c + 16)), u64::from(u32_at(&cib, c + 20)));
            let bitmap = u64_at(&cib, c + 24);
            // All chunks but the last are full size.
            let ok =
                first == next && count > 0 && count <= per_chunk && (count == per_chunk || first + count == blocks);
            ensure(ok && chunk_free <= count, "bad chunk info")?;
            next += count;
            free_total += chunk_free;
            if bitmap == 0 {
                // Never used: all free.
                ensure(chunk_free == count, "a chunk without a bitmap isn't free")?;
            } else {
                nx.keep(used, bitmap, 1)?;
                bitmaps.push((bitmap, first, count, chunk_free));
            }
        }
    }
    ensure(next == blocks, "chunks don't cover the container")?;
    let copy = p.opts.mode == Mode::Copy;
    ensure(!copy || free_total == free, "free block counts don't add up")?;

    // The bitmaps, read in batches of neighbours.
    bitmaps.sort_unstable_by_key(|b| b.0);
    ensure(bitmaps.windows(2).all(|w| w[0].0 != w[1].0), "a bitmap block shared by two chunks")?;
    let span_max = (CHUNK as u64 / bs).max(1);
    let mut i = 0;
    while i < bitmaps.len() {
        let first = bitmaps[i].0;
        let mut j = i + 1;
        while j < bitmaps.len() && bitmaps[j].0 - bitmaps[j - 1].0 <= 16 && bitmaps[j].0 - first < span_max {
            j += 1;
        }
        let span = bitmaps[j - 1].0 - first + 1;
        let buf = p.read_vec(first * bs, (span * bs) as usize)?;
        for &(addr, start, count, chunk_free) in &bitmaps[i..j] {
            let map = &buf[((addr - first) * bs) as usize..][..bs as usize];
            let mut set = 0;
            bit_runs(map, count, start, |b, n| {
                used.add(b * bs, n * bs);
                set += n;
            });
            ensure(!copy || count - set == chunk_free, "a bitmap disagrees with its free count")?;
        }
        i = j;
    }

    // The internal pool (bitmaps and chunk-info blocks, in several versions), its bitmaps.
    nx.keep(used, u64_at(&sm, 0xB0), u64_at(&sm, 0x98))?;
    nx.keep(used, u64_at(&sm, 0xA8), field(0xA4))?;
    // Free queues: the internal pool's, the main device's, the second device's.
    for q in 0..3 {
        let (count, tree) = (u64_at(&sm, 0xC8 + 40 * q), u64_at(&sm, 0xD0 + 40 * q));
        if q == 2 {
            ensure(count == 0 && tree == 0, "Fusion drive (two devices)")?;
            continue;
        }
        let mut ranges = Vec::new();
        if tree != 0 {
            walk(p, nx, tree, &FREE_QUEUE, &mut |key, value| {
                ranges.push((u64_at(key, 8), value.map_or(1, |v| u64_at(v, 0))));
                Ok(())
            })?;
        }
        let mut total = 0u64;
        for (first, n) in ranges {
            nx.keep(used, first, n)?;
            total = total.saturating_add(n);
        }
        ensure(!copy || total == count, "free queue count mismatch")?;
    }
    Ok(())
}

/// A B-tree with fixed-size keys and values.
struct TreeKind {
    subtype: u32,
    key: usize,
    value: usize,
    /// Its nodes are ephemeral objects of the checkpoint (else physical).
    ephemeral: bool,
    /// Leaf records may have no value.
    ghosts: bool,
}

const FREE_QUEUE: TreeKind =
    TreeKind { subtype: SPACEMAN_FREE_QUEUE, key: 16, value: 8, ephemeral: true, ghosts: true };
const OBJECT_MAP: TreeKind = TreeKind { subtype: OMAP, key: 16, value: 16, ephemeral: false, ghosts: false };
const SNAPSHOTS: TreeKind = TreeKind { subtype: OMAP_SNAPSHOT, key: 8, value: 16, ephemeral: false, ghosts: false };

/// What `walk` calls for each leaf record: key, value (None for a ghost record).
type Visit<'a> = dyn FnMut(&[u8], Option<&[u8]>) -> Res<()> + 'a;

/// Calls `visit` with every leaf record, depth first. Returns the blocks of the tree's
/// physical nodes.
fn walk(p: &mut Part, nx: &Nx, root: u64, kind: &TreeKind, visit: &mut Visit) -> Res<Vec<u64>> {
    let mut nodes = Vec::new();
    // Node IDs, with the level they must be at (None for the root).
    let mut todo = vec![(root, None::<u16>)];
    let mut seen = 0;
    while let Some((oid, level)) = todo.pop() {
        seen += 1;
        ensure(seen <= MAX_NODES, "B-tree too large")?;
        let root = level.is_none();
        let t = if root { BTREE } else { BTREE_NODE };
        let node =
            if kind.ephemeral { nx.ephemeral(p, oid, t, kind.subtype)? } else { nx.physical(p, oid, t, kind.subtype)? };
        if !kind.ephemeral {
            nodes.push(oid);
        }
        ensure(node.len() as u64 == nx.block, "bad B-tree node size")?;
        // Flags: root, leaf, fixed-size keys and values.
        let (flags, lvl) = (u16_at(&node, 0x20), u16_at(&node, 0x22));
        let leaf = flags & 2 != 0;
        ensure(flags & !7 == 0 && flags & 4 != 0 && (flags & 1 != 0) == root, "unexpected B-tree node")?;
        ensure(leaf == (lvl == 0) && lvl < MAX_DEPTH && level.is_none_or(|l| l == lvl), "bad B-tree node level")?;
        let count = u32_at(&node, 0x24) as usize;
        let n = |at| usize::from(u16_at(&node, at));
        // The table of contents, keys from the start, values from the end (before the
        // tree's info in the root).
        let toc = 0x38 + n(0x28);
        let keys = toc + n(0x2A);
        let free = keys + n(0x2C);
        let data = free + n(0x2E);
        let end = node.len() - if root { 40 } else { 0 };
        ensure(data <= end && count as u64 * 4 <= n(0x2A) as u64, "bad B-tree node")?;
        if root {
            let info = [(4, nx.block), (8, kind.key as u64), (12, kind.value as u64)];
            ensure(info.iter().all(|&(at, v)| u64::from(u32_at(&node, end + at)) == v), "unexpected B-tree")?;
        }
        let value_len = if leaf { kind.value } else { 8 };
        for i in 0..count {
            let (k, v) = (n(toc + 4 * i), u16_at(&node, toc + 4 * i + 2));
            ensure(keys + k + kind.key <= free, "B-tree key out of bounds")?;
            let key = &node[keys + k..keys + k + kind.key];
            if leaf && kind.ghosts && v == GHOST {
                visit(key, None)?;
                continue;
            }
            let at = end.checked_sub(usize::from(v)).filter(|&at| at >= data && at + value_len <= end);
            let value = &node[at.ok_or_else(|| Bad("B-tree value out of bounds".into()))?..][..value_len];
            if leaf {
                visit(key, Some(value))?;
            } else {
                todo.push((u64_at(value, 0), Some(lvl - 1)));
            }
        }
    }
    Ok(nodes)
}

/// The container's object map, whose trees are kept. Returns where each volume's
/// superblock is (the latest version the checkpoint has).
fn object_map(p: &mut Part, nx: &Nx, oid: u64, used: &mut Ranges) -> Res<Vec<(u64, u64)>> {
    let o = nx.physical(p, oid, OMAP, 0)?;
    nx.keep(used, oid, 1)?;
    ensure(u32_at(&o, 0x28) == BTREE | PHYSICAL, "unexpected object map tree")?;
    ensure(u64_at(&o, 0x48) == 0 && u64_at(&o, 0x50) == 0, "object map being reverted")?;
    let volumes: Vec<u64> = (0..MAX_VOLUMES).map(|i| u64_at(&nx.latest, 0xB8 + 8 * i)).filter(|&v| v != 0).collect();
    // Volume ID → (transaction, flags, size, address).
    let mut found: HashMap<u64, (u64, u32, u32, u64)> = HashMap::new();
    let nodes = walk(p, nx, u64_at(&o, 0x30), &OBJECT_MAP, &mut |key, value| {
        let (oid, xid) = (u64_at(key, 0), u64_at(key, 8));
        if let Some(v) = value.filter(|_| xid <= nx.xid && volumes.contains(&oid)) {
            let record = (xid, u32_at(v, 0), u32_at(v, 4), u64_at(v, 8));
            if found.get(&oid).is_none_or(|old| old.0 < xid) {
                found.insert(oid, record);
            }
        }
        Ok(())
    })?;
    for n in nodes {
        nx.keep(used, n, 1)?;
    }
    let snapshots = u64_at(&o, 0x38);
    if snapshots != 0 {
        for n in walk(p, nx, snapshots, &SNAPSHOTS, &mut |_, _| Ok(()))? {
            nx.keep(used, n, 1)?;
        }
    }
    volumes
        .iter()
        .map(|v| {
            let &(_, flags, size, addr) =
                found.get(v).ok_or_else(|| Bad("volume missing from the object map".into()))?;
            ensure(flags & OMAP_VALUE_ODD == 0 && u64::from(size) == nx.block, "unexpected volume superblock mapping")?;
            Ok((*v, addr))
        })
        .collect()
}

/// Reads the volume superblocks (kept): the name of the main volume, the way macOS shows
/// a container's volumes: the system volume first, then user volumes, then the data volume.
fn volume_names(p: &mut Part, nx: &Nx, volumes: &[(u64, u64)], used: &mut Ranges) -> Res<Option<String>> {
    let mut names = Vec::new();
    for &(oid, addr) in volumes {
        ensure(addr > 0 && addr < nx.blocks, "volume superblock outside the container")?;
        let o = p.read_vec(addr * nx.block, nx.block as usize)?;
        // A virtual object, with its own ID.
        let volume = u64_at(&o, 8) == oid && u32_at(&o, 24) & 0xC000_FFFF == FS && at(&o, 0x20, b"APSB");
        ensure(checked(&o) && volume, "bad volume superblock")?;
        nx.keep(used, addr, 1)?;
        let name = &o[0x2C0..0x3C0];
        let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
        let rank = match u16_at(&o, 0x3C4) {
            0x0001 => 0,
            0x0000 | 0x0002 => 1,
            0x0040 => 2,
            _ => 3,
        };
        let name = name.trim();
        if !name.is_empty() {
            names.push((rank, name.to_owned()));
        }
    }
    Ok(names.into_iter().min_by_key(|(rank, _)| *rank).map(|(_, name)| name))
}
