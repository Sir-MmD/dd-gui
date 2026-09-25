//! btrfs, on one device: the chunk tree maps the file system's logical addresses onto the
//! device, and the extent tree says which of them hold something.
//!
//! Device space no chunk covers is free. Metadata and system chunks are kept whole. In data
//! chunks, only the extents the extent tree records are used, wherever each copy of the chunk
//! is (DUP). Kept too: the first MiB (btrfs never allocates it; boot loaders may live there)
//! and every superblock copy.
//!
//! Every tree block read is checked: its checksum (CRC32C or xxHash64), its address, its
//! owner, level and generation, and the order of its keys. The block groups must match the
//! chunks, and in each data block group the extents must add up to what it says it uses.
//! A log tree (fsync'd changes to replay) means it wasn't cleanly unmounted: copied in full.
//! So is a file system on several devices, zoned, or with features that change where things
//! are (extent tree v2, RAID stripe tree, remapping).

use super::util::MIB;
use super::util::{Bad, Mode, Part, Ranges, Res, Usage, at, crc32c, ensure, u16_at, u32_at, u64_at};

const SUPER: u64 = 64 << 10;
const SUPER_SIZE: u64 = 4096;
const MAGIC: &[u8] = b"_BHRfS_M";

/// A btrfs superblock at 64 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0x10040, MAGIC)
}

// Tree block layout.
const HEADER: usize = 101;
const ITEM: usize = 25;
const KEY_PTR: usize = 33;
const MAX_LEVEL: u8 = 7;

// Trees (owners) and item types.
const ROOT_TREE: u64 = 1;
const EXTENT_TREE: u64 = 2;
const CHUNK_TREE: u64 = 3;
const BLOCK_GROUP_TREE: u64 = 11;
const FIRST_CHUNK_TREE: u64 = 256;
const ROOT_ITEM: u8 = 132;
const EXTENT_ITEM: u8 = 168;
const METADATA_ITEM: u8 = 169;
const EXTENT_REF_V0: u8 = 180;
const BLOCK_GROUP_ITEM: u8 = 192;
const CHUNK_ITEM: u8 = 228;

// Chunk (and block group) types.
const DATA: u64 = 1;
const SYSTEM: u64 = 2;
const METADATA: u64 = 4;
const RAID0: u64 = 1 << 3;
const RAID1: u64 = 1 << 4;
const DUP: u64 = 1 << 5;
const RAID10: u64 = 1 << 6;
const RAID5: u64 = 1 << 7;
const RAID6: u64 = 1 << 8;
const RAID1C3: u64 = 1 << 9;
const RAID1C4: u64 = 1 << 10;
const PROFILES: u64 = RAID0 | RAID1 | DUP | RAID10 | RAID5 | RAID6 | RAID1C3 | RAID1C4;

// Superblock flags: written, relocated, error, seeding, and those of unfinished conversions
// or metadata dumps (these stop the analysis).
const FLAG_ERROR: u64 = 1 << 2;
const FLAGS_OK: u64 = 1 | 2 | FLAG_ERROR | 1 << 32;
/// Incompatible features that don't move things: mixed backrefs, default subvolume, mixed
/// block groups, LZO, ZSTD, big metadata, extended inode refs, RAID5/6, skinny metadata,
/// no holes, metadata UUID, RAID1C34, simple quotas.
const INCOMPAT_OK: u64 = 0xFFF | 1 << 16;
const INCOMPAT_METADATA_UUID: u64 = 1 << 10;
/// Read-only compatible: free space tree (and its valid flag), verity, block group tree.
const COMPAT_RO_OK: u64 = 0xF;
const COMPAT_RO_BLOCK_GROUP_TREE: u64 = 1 << 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    objectid: u64,
    kind: u8,
    offset: u64,
}

fn key(b: &[u8], at: usize) -> Key {
    Key { objectid: u64_at(b, at), kind: b.get(at + 8).copied().unwrap_or(0), offset: u64_at(b, at + 9) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Csum {
    Crc32c,
    XxHash,
}

impl Csum {
    /// Whether the checksum at the start of `b` covers the rest of it.
    fn ok(self, b: &[u8]) -> bool {
        match self {
            Csum::Crc32c => !crc32c(!0, &b[32..]) == u32_at(b, 0),
            Csum::XxHash => xxh64(&b[32..]) == u64_at(b, 0),
        }
    }
}

/// A chunk: `len` bytes of logical address space at `logical`, stored in `stripes` (device
/// offsets) of `stripe_len` bytes each.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chunk {
    logical: u64,
    len: u64,
    kind: u64,
    stripes: Vec<u64>,
    stripe_len: u64,
}

impl Chunk {
    /// Whether each stripe holds the whole chunk, in order (single, DUP, RAID1*): then logical
    /// addresses map straight onto each of them.
    fn linear(&self) -> bool {
        self.stripe_len == self.len
    }

    /// Only data (not mixed with metadata).
    fn data(&self) -> bool {
        self.kind & (DATA | METADATA | SYSTEM) == DATA
    }

    fn end(&self) -> u64 {
        self.logical + self.len
    }
}

struct Btrfs {
    nodesize: usize,
    sectorsize: u64,
    csum: Csum,
    /// What tree blocks carry: the metadata UUID.
    fsid: [u8; 16],
    generation: u64,
    devid: u64,
    /// The device's size, as btrfs uses it.
    size: u64,
    /// Sorted by logical address.
    chunks: Vec<Chunk>,
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let sb =
        head.get(SUPER as usize..(SUPER + SUPER_SIZE) as usize).ok_or_else(|| Bad("partition too small".into()))?;
    let mut fs = superblock(sb, p.size)?;
    let name = &sb[0x12B..0x22B];
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(256)]).trim().to_owned();
    *label = (!name.is_empty()).then_some(name);
    let mode = p.opts.mode;
    let flags = u64_at(sb, 0x38);
    if mode == Mode::Copy {
        ensure(flags & FLAG_ERROR == 0, "file system has errors")?;
        ensure(u64_at(sb, 0x60) == 0, "log tree to replay (not cleanly unmounted)")?;
    }
    mirrors(p, &fs, sb, mode)?;

    // The chunk tree, found through the system chunks the superblock lists.
    fs.chunks = system_chunks(&fs, sb)?;
    let mut chunks = Vec::new();
    let chunk_root = (u64_at(sb, 0x58), sb[0xC7], u64_at(sb, 0xA4));
    walk(p, &fs, chunk_root, CHUNK_TREE, |k, d| {
        if k.kind == CHUNK_ITEM {
            ensure(k.objectid == FIRST_CHUNK_TREE, "bad chunk item")?;
            chunks.push(chunk(&fs, k.offset, d)?);
        }
        Ok(())
    })?;
    for c in &fs.chunks {
        ensure(chunks.contains(c), "system chunks differ from the chunk tree")?;
    }
    fs.chunks = chunks;
    check_overlaps(&fs)?;

    // The extent tree (and the block group tree), found through the root tree.
    let (mut extent_root, mut bg_root) = (None, None);
    let root = (u64_at(sb, 0x50), sb[0xC6], fs.generation);
    walk(p, &fs, root, ROOT_TREE, |k, d| {
        if k.kind == ROOT_ITEM && k.offset == 0 && matches!(k.objectid, EXTENT_TREE | BLOCK_GROUP_TREE) {
            ensure(d.len() >= 239, "bad root item")?;
            let root = (u64_at(d, 176), d[238], u64_at(d, 160));
            if k.objectid == EXTENT_TREE { extent_root = Some(root) } else { bg_root = Some(root) }
        }
        Ok(())
    })?;
    let extent_root = extent_root.ok_or_else(|| Bad("no extent tree".into()))?;
    let bg_tree = u64_at(sb, 0xB4) & COMPAT_RO_BLOCK_GROUP_TREE != 0;
    ensure(bg_root.is_some() == bg_tree, "block group tree mismatch")?;

    let gap = p.opts.gap;
    let mut data: Vec<(usize, Ranges, u64)> = Vec::new();
    let mut groups: Vec<(u64, u64, u64, u64)> = Vec::new();
    let mut cursor = 0;
    let mut next = 0;
    walk(p, &fs, extent_root, EXTENT_TREE, |k, d| {
        match k.kind {
            EXTENT_ITEM | METADATA_ITEM => {
                while fs.chunks.get(cursor).is_some_and(|c| c.end() <= k.objectid) {
                    cursor += 1;
                }
                let c = fs.chunks.get(cursor).filter(|c| c.logical <= k.objectid);
                let c = c.ok_or_else(|| Bad("extent outside any chunk".into()))?;
                if k.kind == METADATA_ITEM {
                    ensure(!c.data(), "metadata in a data chunk")?;
                    return Ok(());
                }
                let end = k.objectid.checked_add(k.offset).filter(|&end| k.offset > 0 && end <= c.end());
                let end = end.ok_or_else(|| Bad("extent crosses its chunk".into()))?;
                if c.data() {
                    ensure(k.objectid >= next, "overlapping extents")?;
                    next = end;
                    if data.last().is_none_or(|(i, _, _)| *i != cursor) {
                        data.push((cursor, Ranges::new(gap), 0));
                    }
                    if let Some((_, ranges, sum)) = data.last_mut() {
                        ranges.add(k.objectid - c.logical, k.offset);
                        *sum += k.offset;
                    }
                }
            }
            BLOCK_GROUP_ITEM if !bg_tree => groups.push(group(k, d)?),
            EXTENT_REF_V0 => return Err(Bad("old extent format".into())),
            _ => {}
        }
        Ok(())
    })?;
    if let Some(bg_root) = bg_root {
        walk(p, &fs, bg_root, BLOCK_GROUP_TREE, |k, d| {
            if k.kind == BLOCK_GROUP_ITEM {
                groups.push(group(k, d)?);
            }
            Ok(())
        })?;
    }
    // One block group per chunk, and each data one using what its extents add up to.
    ensure(groups.len() == fs.chunks.len(), "block groups don't match the chunks")?;
    for ((logical, len, _, kind), c) in groups.iter().zip(&fs.chunks) {
        ensure(*logical == c.logical && *len == c.len && *kind == c.kind, "block groups don't match the chunks")?;
    }
    if mode == Mode::Copy {
        let total = groups.iter().try_fold(0u64, |total, g| total.checked_add(g.2));
        ensure(total == Some(u64_at(sb, 0x78)), "used bytes don't match the block groups")?;
        for (i, _) in fs.chunks.iter().enumerate().filter(|(_, c)| c.data()) {
            let sum = data.iter().find(|(j, _, _)| *j == i).map_or(0, |(_, _, sum)| *sum);
            ensure(sum == groups[i].2, "extents don't match their block group")?;
        }
    }

    let mut used = p.ranges();
    used.add(0, MIB.min(fs.size));
    for mirror in [SUPER, 64 * MIB, 256 << 30] {
        if mirror + SUPER_SIZE <= fs.size {
            used.add(mirror, SUPER_SIZE);
        }
    }
    let mut extents = data.into_iter().peekable();
    for (i, c) in fs.chunks.iter().enumerate() {
        let ranges = match extents.peek() {
            Some((j, _, _)) if *j == i => extents.next().map(|(_, r, _)| r.into_vec()),
            _ => None,
        };
        for &stripe in &c.stripes {
            match (&ranges, c.data() && c.linear()) {
                (Some(ranges), true) => ranges.iter().for_each(|e| used.add(stripe + e.start, e.len)),
                (None, true) => {}
                _ => used.add(stripe, c.stripe_len),
            }
        }
    }
    Ok(Usage { used, end: fs.size })
}

fn superblock(sb: &[u8], part_size: u64) -> Res<Btrfs> {
    ensure(at(sb, 0x40, MAGIC) && u64_at(sb, 0x30) == SUPER, "not btrfs")?;
    let csum = match u16_at(sb, 0xC4) {
        0 => Csum::Crc32c,
        1 => Csum::XxHash,
        _ => return Err(Bad("unsupported checksum type".into())),
    };
    ensure(csum.ok(sb), "superblock checksum mismatch")?;
    ensure(u64_at(sb, 0x38) & !FLAGS_OK == 0, "unfinished conversion, or a metadata dump")?;
    ensure(u64_at(sb, 0x88) == 1, "several devices")?;
    let incompat = u64_at(sb, 0xBC);
    ensure(incompat & !INCOMPAT_OK == 0, "unsupported features")?;
    ensure(u64_at(sb, 0xB4) & !COMPAT_RO_OK == 0, "unsupported read-only features")?;
    let sectorsize = u64::from(u32_at(sb, 0x90));
    let nodesize = u64::from(u32_at(sb, 0x94));
    ensure(sectorsize.is_power_of_two() && (512..=65536).contains(&sectorsize), "bad sector size")?;
    ensure(nodesize.is_power_of_two() && nodesize >= sectorsize.max(4096) && nodesize <= 65536, "bad node size")?;
    ensure(u32_at(sb, 0xA0) <= 2048, "bad system chunk array")?;
    // The device item: this device.
    let dev = &sb[0xC9..];
    let size = u64_at(dev, 8);
    ensure(size == u64_at(sb, 0x70) && size <= part_size && size >= MIB, "device size mismatch")?;
    let fsid: [u8; 16] = if incompat & INCOMPAT_METADATA_UUID != 0 { &sb[0x23B..0x24B] } else { &sb[0x20..0x30] }
        .try_into()
        .unwrap_or_default();
    Ok(Btrfs {
        nodesize: nodesize as usize,
        sectorsize,
        csum,
        fsid,
        generation: u64_at(sb, 0x48),
        devid: u64_at(dev, 0),
        size,
        chunks: Vec::new(),
    })
}

/// The superblock copies at 64 MiB and 256 GiB, where the device has room for them, must be
/// valid, of this file system, and (unless estimating) as recent as the first.
fn mirrors(p: &mut Part, fs: &Btrfs, sb: &[u8], mode: Mode) -> Res<()> {
    for at in [64 * MIB, 256 << 30] {
        if at + SUPER_SIZE > fs.size {
            continue;
        }
        let m = p.read_vec(at, SUPER_SIZE as usize)?;
        let ok = super::util::at(&m, 0x40, MAGIC)
            && u64_at(&m, 0x30) == at
            && fs.csum.ok(&m)
            && m[0x20..0x30] == sb[0x20..0x30];
        ensure(ok, "bad superblock copy")?;
        if mode == Mode::Copy {
            ensure(u64_at(&m, 0x48) == fs.generation, "superblock copies of different generations")?;
        }
    }
    Ok(())
}

/// A chunk item's mapping, checked.
fn chunk(fs: &Btrfs, logical: u64, d: &[u8]) -> Res<Chunk> {
    let n = u64::from(u16_at(d, 44));
    let sub = u64::from(u16_at(d, 46));
    ensure(n >= 1 && d.len() as u64 == 48 + 32 * n, "bad chunk item")?;
    let (len, kind) = (u64_at(d, 0), u64_at(d, 24));
    ensure(len > 0 && len.is_multiple_of(fs.sectorsize) && logical.is_multiple_of(fs.sectorsize), "bad chunk")?;
    ensure(logical.checked_add(len).is_some(), "bad chunk")?;
    ensure(
        kind & (DATA | METADATA | SYSTEM) != 0 && kind & !(DATA | METADATA | SYSTEM | PROFILES) == 0,
        "bad chunk type",
    )?;
    let profile = kind & PROFILES;
    let stripe_len = match profile {
        0 if n == 1 => len,
        DUP if n == 2 => len,
        RAID1 if n >= 2 => len,
        RAID1C3 if n >= 3 => len,
        RAID1C4 if n >= 4 => len,
        RAID0 => len.div_ceil(n),
        RAID10 if sub >= 2 && n % sub == 0 => len.div_ceil(n / sub),
        RAID5 if n >= 2 => len.div_ceil(n - 1),
        RAID6 if n >= 3 => len.div_ceil(n - 2),
        _ => return Err(Bad("bad chunk profile".into())),
    };
    let mut stripes = Vec::with_capacity(n as usize);
    for s in d[48..].as_chunks::<32>().0 {
        ensure(u64_at(s, 0) == fs.devid, "chunk on another device")?;
        let offset = u64_at(s, 8);
        ensure(offset.checked_add(stripe_len).is_some_and(|end| end <= fs.size), "chunk past the end of the device")?;
        stripes.push(offset);
    }
    // One stripe of RAID0 is as good as single.
    let stripe_len = if profile == RAID0 && n == 1 { len } else { stripe_len };
    Ok(Chunk { logical, len, kind, stripes, stripe_len })
}

/// The system chunks the superblock lists, to find the chunk tree with.
fn system_chunks(fs: &Btrfs, sb: &[u8]) -> Res<Vec<Chunk>> {
    let array = &sb[0x32B..0x32B + u32_at(sb, 0xA0) as usize];
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut at = 0;
    while at < array.len() {
        let k = key(array, at);
        let n = usize::from(u16_at(array, at + 17 + 44));
        let item = array.get(at + 17..at + 17 + 48 + 32 * n).ok_or_else(|| Bad("bad system chunk array".into()))?;
        ensure(k.kind == CHUNK_ITEM && k.objectid == FIRST_CHUNK_TREE, "bad system chunk array")?;
        let c = chunk(fs, k.offset, item)?;
        ensure(chunks.last().is_none_or(|last| last.end() <= c.logical), "bad system chunk array")?;
        chunks.push(c);
        at += 17 + item.len();
    }
    Ok(chunks)
}

/// Chunks must not overlap, in logical addresses nor on the device.
fn check_overlaps(fs: &Btrfs) -> Res<()> {
    let logical_ok = fs.chunks.windows(2).all(|w| w[0].end() <= w[1].logical);
    let mut physical: Vec<(u64, u64)> =
        fs.chunks.iter().flat_map(|c| c.stripes.iter().map(move |&s| (s, s + c.stripe_len))).collect();
    physical.sort_unstable();
    let physical_ok = physical.windows(2).all(|w| w[0].1 <= w[1].0);
    ensure(logical_ok && physical_ok, "overlapping chunks")
}

/// (logical, length, used, flags) of a block group item.
fn group(k: Key, d: &[u8]) -> Res<(u64, u64, u64, u64)> {
    ensure(d.len() >= 24 && u64_at(d, 8) == FIRST_CHUNK_TREE, "bad block group item")?;
    let used = u64_at(d, 0);
    ensure(used <= k.offset, "block group uses more than it has")?;
    Ok((k.objectid, k.offset, used, u64_at(d, 16)))
}

impl Btrfs {
    /// Where the tree block at `logical` is on the device (its first copy).
    fn physical(&self, logical: u64) -> Res<u64> {
        let i = self.chunks.partition_point(|c| c.end() <= logical);
        let end = logical.checked_add(self.nodesize as u64);
        let c = self.chunks.get(i).filter(|c| c.logical <= logical && end.is_some_and(|end| end <= c.end()));
        let c = c.ok_or_else(|| Bad("tree block outside the chunks".into()))?;
        ensure(c.linear() && c.kind & (METADATA | SYSTEM) != 0, "tree block outside metadata")?;
        Ok(c.stripes[0] + (logical - c.logical))
    }
}

/// Visits every item of a tree in key order: (root address, level, generation) and the owner
/// its blocks must name.
fn walk(
    p: &mut Part,
    fs: &Btrfs,
    root: (u64, u8, u64),
    owner: u64,
    mut visit: impl FnMut(Key, &[u8]) -> Res<()>,
) -> Res<()> {
    let (addr, level, generation) = root;
    ensure(level <= MAX_LEVEL, "bad tree level")?;
    let mut stack: Vec<(u64, u8, u64, Option<Key>)> = vec![(addr, level, generation, None)];
    let mut last: Option<Key> = None;
    // No tree has more blocks than fit on the device.
    let mut budget = fs.size / fs.nodesize as u64 + 1;
    while let Some((addr, level, generation, first)) = stack.pop() {
        budget = budget.checked_sub(1).ok_or_else(|| Bad("tree too large".into()))?;
        let b = node(p, fs, addr, level, generation, owner)?;
        let n = u32_at(&b, 0x60) as usize;
        if level == 0 {
            ensure(n <= (fs.nodesize - HEADER) / ITEM, "bad leaf")?;
            ensure(n > 0 || first.is_none(), "empty leaf")?;
            for i in 0..n {
                let at = HEADER + i * ITEM;
                let k = key(&b, at);
                ensure(i > 0 || first.is_none_or(|f| f == k), "leaf doesn't start with its parent's key")?;
                ensure(last.is_none_or(|l| l < k), "keys out of order")?;
                last = Some(k);
                let (offset, size) = (u32_at(&b, at + 17) as usize, u32_at(&b, at + 21) as usize);
                let start = HEADER + offset;
                ensure(offset >= n * ITEM && start + size <= fs.nodesize, "item outside its leaf")?;
                visit(k, &b[start..start + size])?;
            }
        } else {
            ensure(n >= 1 && n <= (fs.nodesize - HEADER) / KEY_PTR, "bad node")?;
            let keys: Vec<Key> = (0..n).map(|i| key(&b, HEADER + i * KEY_PTR)).collect();
            ensure(keys.windows(2).all(|w| w[0] < w[1]), "keys out of order")?;
            ensure(first.is_none_or(|f| f == keys[0]), "node doesn't start with its parent's key")?;
            ensure(last.is_none_or(|l| l < keys[0]), "keys out of order")?;
            for (i, k) in keys.iter().enumerate().rev() {
                let at = HEADER + i * KEY_PTR;
                stack.push((u64_at(&b, at + 17), level - 1, u64_at(&b, at + 25), Some(*k)));
            }
        }
    }
    Ok(())
}

/// Reads and checks a tree block.
fn node(p: &mut Part, fs: &Btrfs, addr: u64, level: u8, generation: u64, owner: u64) -> Res<Vec<u8>> {
    ensure(addr.is_multiple_of(fs.sectorsize), "misaligned tree block")?;
    let b = p.read_vec(fs.physical(addr)?, fs.nodesize)?;
    ensure(fs.csum.ok(&b), "tree block checksum mismatch")?;
    ensure(b[0x20..0x30] == fs.fsid && u64_at(&b, 0x30) == addr, "tree block of another file system, or misplaced")?;
    ensure(b[0x64] == level && u64_at(&b, 0x58) == owner, "tree block level or owner mismatch")?;
    ensure(u64_at(&b, 0x50) == generation && generation <= fs.generation, "tree block generation mismatch")?;
    Ok(b)
}

/// xxHash64 with seed 0.
fn xxh64(data: &[u8]) -> u64 {
    const P1: u64 = 0x9E37_79B1_85EB_CA87;
    const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    const P3: u64 = 0x1656_67B1_9E37_79F9;
    const P4: u64 = 0x85EB_CA77_C2B2_AE63;
    const P5: u64 = 0x27D4_EB2F_1656_67C5;
    let round = |acc: u64, input: u64| acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1);
    let merge = |acc: u64, v: u64| (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4);
    let (stripes, rest) = data.as_chunks::<32>();
    let mut h = if stripes.is_empty() {
        P5
    } else {
        let mut v = [P1.wrapping_add(P2), P2, 0, P1.wrapping_neg()];
        for s in stripes {
            for (i, lane) in v.iter_mut().enumerate() {
                *lane = round(*lane, u64_at(s, 8 * i));
            }
        }
        let h = v[0].rotate_left(1).wrapping_add(v[1].rotate_left(7)).wrapping_add(v[2].rotate_left(12));
        v.iter().fold(h.wrapping_add(v[3].rotate_left(18)), |h, &lane| merge(h, lane))
    };
    h = h.wrapping_add(data.len() as u64);
    let (words, rest) = rest.as_chunks::<8>();
    for w in words {
        h = (h ^ round(0, u64::from_le_bytes(*w))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    let (halves, rest) = rest.as_chunks::<4>();
    for w in halves {
        h = (h ^ u64::from(u32::from_le_bytes(*w)).wrapping_mul(P1)).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
    }
    for &b in rest {
        h = (h ^ u64::from(b).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ h >> 32
}

#[cfg(test)]
mod tests {
    #[test]
    fn xxh64_known_values() {
        assert_eq!(super::xxh64(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(super::xxh64(b"a"), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(super::xxh64(b"abc"), 0x44BC_2CF5_AD77_0999);
        let long: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        // Every tail length through the stripe loop.
        for n in [31, 32, 33, 63, 64, 100, 1000] {
            let _ = super::xxh64(&long[..n]);
        }
    }
}
