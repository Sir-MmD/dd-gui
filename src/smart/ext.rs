//! ext2, ext3 and ext4: each block group's bitmap says which blocks are used. Groups whose
//! bitmap was never initialized (BLOCK_UNINIT) hold nothing but metadata.

use super::util::{
    Bad, CHUNK, Mode, Part, Ranges, Res, Usage, bit_runs, crc16, crc32c, ensure, u16_at, u32_at, u64_at,
};

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
/// Incompatible features known not to change how blocks are accounted for: filetype,
/// recover, meta_bg, extents, 64bit, mmp, flex_bg, ea_inode, dirdata, csum_seed, largedir,
/// inline_data, encrypt, casefold.
const INCOMPAT_KNOWN: u32 = 0x3_F7D6;
const RO_SPARSE_SUPER: u32 = 0x1;
const RO_GDT_CSUM: u32 = 0x10;
const RO_BIGALLOC: u32 = 0x200;
const RO_METADATA_CSUM: u32 = 0x400;
/// The same for read-only features. Not snapshots (0x80) nor replicas (0x800).
const RO_KNOWN: u32 = 0x1_F77F;
const BG_BLOCK_UNINIT: u16 = 0x2;
/// More groups than this (128 TiB with 4 KiB blocks) and the file system is copied in full.
const MAX_GROUPS: u64 = 1 << 20;

/// ext2, ext3 or ext4, the way blkid tells them apart ("jbd" for an external journal).
pub(crate) fn name(head: &[u8]) -> &'static str {
    let sb = head.get(1024..).unwrap_or_default();
    let (compat, incompat, ro) = (u32_at(sb, 0x5C), u32_at(sb, 0x60), u32_at(sb, 0x64));
    if incompat & INCOMPAT_JOURNAL_DEV != 0 {
        "jbd"
    } else if ro & !0x7 != 0 || incompat & !0x16 != 0 {
        "ext4"
    } else if compat & COMPAT_HAS_JOURNAL != 0 {
        "ext3"
    } else {
        "ext2"
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Csum {
    None,
    /// uninit_bg: CRC-16 over the UUID, the group number and the descriptor.
    Crc16([u8; 16]),
    /// metadata_csum: CRC-32C from this seed.
    Crc32c(u32),
}

struct Fs {
    block: u64,
    blocks: u64,
    first: u64,
    per_group: u64,
    /// log2 of blocks per cluster (bigalloc).
    cluster_bits: u32,
    clusters_per_group: u64,
    groups: u64,
    desc_size: usize,
    per_block: u64,
    desc_blocks: u64,
    reserved_gdt: u64,
    meta_bg: bool,
    first_meta_bg: u64,
    itable_blocks: u64,
    sparse_super: bool,
    /// sparse_super2: the only two groups with backups.
    backups: Option<[u64; 2]>,
    csum: Csum,
}

/// What a group descriptor says about its block bitmap.
struct Bitmap {
    block: u64,
    group: u64,
    free: u64,
    csum: u32,
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let sb = head.get(1024..2048).ok_or_else(|| Bad("partition too small for ext".into()))?;
    ensure(u16_at(sb, 0x38) == 0xEF53, "not ext")?;
    let name = &sb[0x78..0x88];
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(16)]).trim().to_owned();
    *label = (!name.is_empty()).then_some(name);
    let fs = superblock(sb, p.size, p.opts.mode)?;
    let mode = p.opts.mode;

    let table = fs.descriptors(p)?;
    let mut used = p.ranges();
    // Metadata of every group, where it actually sits (flex_bg moves it into other groups).
    let mut meta = Vec::new();
    let mut bitmaps = Vec::new();
    for g in 0..fs.groups {
        let d = &table[g as usize * fs.desc_size..][..fs.desc_size];
        ensure(fs.descriptor_ok(g, d), "group descriptor checksum mismatch")?;
        let hi = |off| if fs.desc_size >= 64 { u64::from(u32_at(d, off)) << 32 } else { 0 };
        let block_bitmap = u64::from(u32_at(d, 0x00)) | hi(0x20);
        let inode_bitmap = u64::from(u32_at(d, 0x04)) | hi(0x24);
        let inode_table = u64::from(u32_at(d, 0x08)) | hi(0x28);
        let inside = |b: u64, n: u64| b >= fs.first && b.checked_add(n).is_some_and(|end| end <= fs.blocks);
        let ok = inside(block_bitmap, 1) && inside(inode_bitmap, 1) && inside(inode_table, fs.itable_blocks);
        ensure(ok, "group metadata outside the file system")?;
        meta.extend([(block_bitmap, 1), (inode_bitmap, 1), (inode_table, fs.itable_blocks)]);
        fs.mark_super(g, &mut meta);
        if fs.csum != Csum::None && u16_at(d, 0x12) & BG_BLOCK_UNINIT != 0 {
            // Nothing but the metadata marked above: that's what the kernel assumes too.
            ensure(g != 0, "group 0 marked uninitialized")?;
        } else {
            let hi16 = |off| if fs.desc_size >= 64 { u32::from(u16_at(d, off)) << 16 } else { 0 };
            let free = u64::from(u32::from(u16_at(d, 0x0C)) | hi16(0x2C));
            let csum = u32::from(u16_at(d, 0x18)) | hi16(0x38);
            bitmaps.push(Bitmap { block: block_bitmap, group: g, free, csum });
        }
    }
    if let Some(mmp) = (fs.mmp_block(sb)).filter(|&b| b >= fs.first && b < fs.blocks) {
        meta.push((mmp, 1));
    }
    meta.sort_unstable();
    for (b, n) in meta {
        used.add(b * fs.block, n * fs.block);
    }

    // Block bitmaps, read in batches of neighbours (flex_bg packs them together).
    bitmaps.sort_unstable_by_key(|b| b.block);
    let span_max = (CHUNK as u64 / fs.block).max(1);
    let mut i = 0;
    while i < bitmaps.len() {
        let first = bitmaps[i].block;
        let mut j = i + 1;
        while j < bitmaps.len() && bitmaps[j].block - bitmaps[j - 1].block <= 16 && bitmaps[j].block - first < span_max
        {
            j += 1;
        }
        let span = bitmaps[j - 1].block - first + 1;
        let buf = p.read_vec(first * fs.block, (span * fs.block) as usize)?;
        for b in &bitmaps[i..j] {
            let map = &buf[((b.block - first) * fs.block) as usize..][..fs.block as usize];
            fs.group_blocks(b, map, mode, &mut used)?;
        }
        i = j;
    }
    Ok(Usage { used, end: fs.blocks * fs.block })
}

fn superblock(sb: &[u8], part_size: u64, mode: Mode) -> Res<Fs> {
    let (compat, incompat, ro) = (u32_at(sb, 0x5C), u32_at(sb, 0x60), u32_at(sb, 0x64));
    ensure(incompat & INCOMPAT_JOURNAL_DEV == 0, "external journal device")?;
    ensure(incompat & !INCOMPAT_KNOWN == 0 && ro & !RO_KNOWN == 0, "unsupported features")?;
    let rev = u32_at(sb, 0x4C);
    ensure(rev <= 1 && (rev == 1 || compat | incompat | ro == 0), "unknown revision")?;
    let uuid: [u8; 16] = sb[0x68..0x78].try_into().unwrap_or_default();
    let csum = if ro & RO_METADATA_CSUM != 0 {
        ensure(sb[0x175] == 1, "unknown checksum type")?;
        ensure(crc32c(!0, &sb[..0x3FC]) == u32_at(sb, 0x3FC), "superblock checksum mismatch")?;
        Csum::Crc32c(if incompat & INCOMPAT_CSUM_SEED != 0 { u32_at(sb, 0x270) } else { crc32c(!0, &uuid) })
    } else if ro & RO_GDT_CSUM != 0 {
        Csum::Crc16(uuid)
    } else {
        Csum::None
    };
    if mode == Mode::Copy {
        // A journal to replay may allocate blocks the bitmaps on disk call free.
        ensure(incompat & INCOMPAT_RECOVER == 0, "journal needs recovery (not cleanly unmounted)")?;
        let state = u16_at(sb, 0x3A);
        ensure(state & 1 != 0 && state & 2 == 0, "not cleanly unmounted, or has errors")?;
    }
    let log_block = u32_at(sb, 0x18);
    ensure(log_block <= 6, "bad block size")?;
    let block = 1024u64 << log_block;
    let is64 = incompat & INCOMPAT_64BIT != 0;
    let blocks = u64::from(u32_at(sb, 0x04)) | if is64 { u64::from(u32_at(sb, 0x150)) << 32 } else { 0 };
    let first = u64::from(u32_at(sb, 0x14));
    let bigalloc = ro & RO_BIGALLOC != 0;
    ensure(first == u64::from(block == 1024 && !bigalloc), "bad first data block")?;
    let per_group = u64::from(u32_at(sb, 0x20));
    let (cluster_bits, clusters_per_group) = if bigalloc {
        let log_cluster = u32_at(sb, 0x1C);
        ensure(log_cluster >= log_block && log_cluster - log_block <= 16, "bad cluster size")?;
        let bits = log_cluster - log_block;
        let cpg = u64::from(u32_at(sb, 0x24));
        ensure(cpg > 0 && cpg <= 8 * block && per_group == cpg << bits, "bad clusters per group")?;
        (bits, cpg)
    } else {
        ensure(per_group > 0 && per_group <= 8 * block, "bad blocks per group")?;
        (0, per_group)
    };
    ensure(blocks > first, "no blocks")?;
    ensure(blocks.checked_mul(block).is_some_and(|n| n <= part_size), "file system larger than its partition")?;
    let groups = (blocks - first).div_ceil(per_group);
    ensure(groups <= MAX_GROUPS, "too many groups")?;
    let inodes_per_group = u64::from(u32_at(sb, 0x28));
    ensure(inodes_per_group > 0 && inodes_per_group <= 8 * block, "bad inodes per group")?;
    ensure(u64::from(u32_at(sb, 0x00)) == groups * inodes_per_group, "inode count doesn't match the groups")?;
    let inode_size = if rev == 0 { 128 } else { u64::from(u16_at(sb, 0x58)) };
    ensure(inode_size.is_power_of_two() && (128..=block).contains(&inode_size), "bad inode size")?;
    let desc_size = if is64 { usize::from(u16_at(sb, 0xFE)) } else { 32 };
    ensure(desc_size.is_power_of_two() && (32..=1024).contains(&desc_size), "bad group descriptor size")?;
    ensure(!is64 || desc_size >= 64, "64bit with small group descriptors")?;
    let per_block = block / desc_size as u64;
    let desc_blocks = groups.div_ceil(per_block);
    let meta_bg = incompat & INCOMPAT_META_BG != 0;
    let first_meta_bg = if meta_bg { u64::from(u32_at(sb, 0x104)) } else { desc_blocks };
    ensure(first_meta_bg <= desc_blocks, "bad first meta block group")?;
    Ok(Fs {
        block,
        blocks,
        first,
        per_group,
        cluster_bits,
        clusters_per_group,
        groups,
        desc_size,
        per_block,
        desc_blocks,
        reserved_gdt: u64::from(u16_at(sb, 0xCE)),
        meta_bg,
        first_meta_bg,
        itable_blocks: (inodes_per_group * inode_size).div_ceil(block),
        sparse_super: ro & RO_SPARSE_SUPER != 0,
        backups: (compat & COMPAT_SPARSE_SUPER2 != 0)
            .then(|| [u64::from(u32_at(sb, 0x24C)), u64::from(u32_at(sb, 0x250))]),
        csum,
    })
}

impl Fs {
    fn group_first(&self, g: u64) -> u64 {
        self.first + g * self.per_group
    }

    fn mmp_block(&self, sb: &[u8]) -> Option<u64> {
        (u32_at(sb, 0x60) & INCOMPAT_MMP != 0).then(|| u64_at(sb, 0x168))
    }

    /// Whether group `g` holds a backup of the superblock (and, without meta_bg, of the
    /// group descriptors).
    fn has_super(&self, g: u64) -> bool {
        let power_of = |mut n: u64, base: u64| {
            while n.is_multiple_of(base) {
                n /= base;
            }
            n == 1
        };
        match self.backups {
            _ if g == 0 => true,
            Some(backups) => backups.contains(&g),
            None if g == 1 || !self.sparse_super => true,
            None => g % 2 == 1 && (power_of(g, 3) || power_of(g, 5) || power_of(g, 7)),
        }
    }

    /// Where descriptor block `i` is.
    fn desc_block(&self, i: u64) -> u64 {
        // The superblock is block 1 with 1 KiB blocks (even under bigalloc), else in block 0.
        let sb_block = u64::from(self.block == 1024);
        if i < self.first_meta_bg {
            return sb_block + 1 + i;
        }
        // meta_bg: in the first group of each meta group, after its superblock backup.
        let g = i * self.per_block;
        let mut after = u64::from(self.has_super(g));
        if self.block == 1024 && i == 0 && self.first == 0 {
            after += 1;
        }
        self.group_first(g) + after
    }

    fn descriptors(&self, p: &mut Part) -> Res<Vec<u8>> {
        let len = self.groups as usize * self.desc_size;
        ensure(len <= 64 << 20, "group descriptor table too large")?;
        let span_max = (CHUNK as u64 / self.block).max(1);
        let mut table = Vec::with_capacity(len);
        let mut i = 0;
        while i < self.desc_blocks {
            let start = self.desc_block(i);
            let mut n = 1;
            while i + n < self.desc_blocks && n < span_max && self.desc_block(i + n) == start + n {
                n += 1;
            }
            ensure(start.checked_add(n).is_some_and(|end| end <= self.blocks), "descriptors outside the file system")?;
            table.extend(p.read_vec(start * self.block, (n * self.block) as usize)?);
            i += n;
        }
        table.truncate(len);
        Ok(table)
    }

    fn descriptor_ok(&self, g: u64, d: &[u8]) -> bool {
        let group = (g as u32).to_le_bytes();
        let computed = match self.csum {
            Csum::None => return true,
            Csum::Crc32c(seed) => {
                let c = crc32c(crc32c(seed, &group), &d[..0x1E]);
                crc32c(crc32c(c, &[0, 0]), &d[0x20..]) as u16
            }
            Csum::Crc16(uuid) => {
                let c = crc16(crc16(crc16(!0, &uuid), &group), &d[..0x1E]);
                crc16(c, &d[0x20..])
            }
        };
        computed == u16_at(d, 0x1E)
    }

    /// Marks the superblock backup and group descriptor copies group `g` holds, the way
    /// e2fsprogs accounts for them (in blocks, as `(start, count)`).
    fn mark_super(&self, g: u64, meta: &mut Vec<(u64, u64)>) {
        let mut start = self.group_first(g);
        if start == 0 && self.block == 1024 {
            // 1 KiB blocks with bigalloc: block 0 is the boot block, the superblock is block 1.
            start = 1;
            meta.push((0, 1));
        }
        let has_super = self.has_super(g);
        if has_super {
            meta.push((start, 1));
        }
        if g / self.per_block < self.first_meta_bg {
            if has_super {
                let n = if self.meta_bg { self.first_meta_bg } else { self.desc_blocks + self.reserved_gdt };
                let n = n.min(self.blocks.saturating_sub(start + 1));
                meta.push((start + 1, n));
            }
        } else if matches!(g % self.per_block, 0 | 1) || g % self.per_block == self.per_block - 1 {
            meta.push((start + u64::from(has_super), 1));
        }
    }

    /// Marks what a group's block bitmap says is used, after checking it against its
    /// checksum and the free count in its descriptor.
    fn group_blocks(&self, b: &Bitmap, map: &[u8], mode: Mode, used: &mut Ranges) -> Res<()> {
        let start = self.group_first(b.group);
        let bits = (self.blocks - start).min(self.per_group).div_ceil(1 << self.cluster_bits);
        if mode == Mode::Copy {
            if let Csum::Crc32c(seed) = self.csum {
                let bytes = (self.clusters_per_group / 8) as usize;
                let crc = crc32c(seed, &map[..bytes.min(map.len())]);
                let crc = if self.desc_size >= 64 { crc } else { crc & 0xFFFF };
                ensure(crc == b.csum, "block bitmap checksum mismatch")?;
            }
            let set: u64 = map[..(bits as usize).div_ceil(8)]
                .iter()
                .enumerate()
                .map(|(i, &byte)| {
                    let valid = (bits - 8 * i as u64).min(8);
                    u64::from((byte & (0xFFu16 >> (8 - valid)) as u8).count_ones())
                })
                .sum();
            ensure(bits - set <= b.free, "block bitmap frees more than its group descriptor says")?;
        }
        let unit = self.block << self.cluster_bits;
        let base = start * self.block;
        bit_runs(map, bits, 0, |c, n| used.add(base + c * unit, n * unit));
        Ok(())
    }
}
