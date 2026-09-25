//! UDF: the partition's unallocated space bitmap (or table) says which of its blocks are free.
//! Everything outside the partition is kept: the volume recognition sequence, the anchors,
//! both volume descriptor sequences and the integrity sequence.
//!
//! One physical partition only: virtual (VAT), sparable and metadata partitions (UDF 2.50)
//! are copied in full, and so is a volume whose integrity descriptor says it's still open
//! (not cleanly unmounted). Without any free space information (read-only media, most UDF
//! bridge discs), everything up to the last block the volume records is kept, along with an
//! ISO 9660 volume sharing the partition.

use super::util::{Bad, CHUNK, Mode, Part, Ranges, Res, Usage, at, be16_at, be32_at, bit_runs, ensure, u16_at, u32_at};

/// A UDF volume recognition sequence (NSR02 or NSR03) from 32 KiB on.
pub(crate) fn detect(head: &[u8]) -> bool {
    (0..8).any(|k| at(head, 0x8001 + k * 2048, b"NSR02") || at(head, 0x8001 + k * 2048, b"NSR03"))
}

// Descriptor tag identifiers.
const AVDP: u16 = 2;
const VDP: u16 = 3;
const PD: u16 = 5;
const LVD: u16 = 6;
const TD: u16 = 8;
const LVID: u16 = 9;
const USE: u16 = 263;
const SBD: u16 = 264;

/// Descriptors read in one sequence, and pointers followed, at most.
const MAX_SEQUENCE: u64 = 256;
const MAX_HOPS: u32 = 16;

/// CRC-CCITT (polynomial 0x1021, not reflected, from 0), as descriptor tags use it.
fn crc_itu(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |crc, &b| {
        let mut crc = crc ^ (u16::from(b) << 8);
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
        crc
    })
}

/// Whether `d` starts with a valid descriptor tag of kind `id`, recorded at block `location`.
fn tagged(d: &[u8], id: u16, location: u64) -> bool {
    if d.len() < 16 || u16_at(d, 0) != id || u64::from(u32_at(d, 12)) != location {
        return false;
    }
    let sum = d[..16].iter().enumerate().filter(|&(i, _)| i != 4).fold(0u8, |s, (_, &b)| s.wrapping_add(b));
    if sum != d[4] || !matches!(u16_at(d, 2), 2 | 3) {
        return false;
    }
    // Like Linux, the CRC is checked when it covers no more than the block at hand.
    let len = usize::from(u16_at(d, 10));
    match d.get(16..16 + len) {
        Some(body) => crc_itu(body) == u16_at(d, 8),
        None => true,
    }
}

/// (length in bytes, first block) of the extent_ad at `at`.
fn extent(d: &[u8], at: usize) -> (u64, u64) {
    (u64::from(u32_at(d, at)), u64::from(u32_at(d, at + 4)))
}

/// What a partition descriptor says.
struct Partition {
    number: u16,
    start: u64,
    blocks: u64,
    access: u32,
    /// Unallocated space table and bitmap: (length in bytes, first block in the partition).
    table: (u64, u64),
    bitmap: (u64, u64),
    /// Freed space (for rewritable media) or a partition integrity table.
    other_space: bool,
}

/// The volume as far as free space goes.
struct Volume {
    block: u64,
    /// The partition, in blocks.
    start: u64,
    blocks: u64,
}

impl Volume {
    /// Byte offset of block `b` of the partition.
    fn at(&self, b: u64) -> u64 {
        (self.start + b) * self.block
    }
}

pub(crate) fn analyze(p: &mut Part, _head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let (block, anchor) = find_anchor(p)?;
    let volume_blocks = p.size / block;
    // Block ranges to keep whatever the partition's allocation says: (first, count).
    let mut kept: Vec<(u64, u64)> = vec![(256, 1)];
    let main = extent(&anchor, 16);
    let reserve = extent(&anchor, 24);
    kept.extend([main, reserve].iter().map(|&(len, at)| (at, len.div_ceil(block))));
    // The main volume descriptor sequence, else the reserve one.
    let (partitions, lvd) = match sequence(p, block, main) {
        Ok(found) => found,
        Err(_) => sequence(p, block, reserve)?,
    };
    *label = dstring(&lvd[84..212]);
    ensure(u64::from(u32_at(&lvd, 212)) == block, "logical block size differs from the sector size")?;
    // Partition maps: exactly one, of type 1 (physical).
    let maps_len = u32_at(&lvd, 264) as usize;
    ensure(u32_at(&lvd, 268) == 1 && maps_len >= 6 && 440 + maps_len <= lvd.len(), "not a single partition")?;
    ensure(lvd[440] == 1 && lvd[441] == 6, "virtual, sparable or metadata partition")?;
    let number = u16_at(&lvd, 444);
    let pd = partitions.into_iter().find(|d| d.number == number).ok_or_else(|| Bad("partition missing".into()))?;
    ensure(pd.start.checked_add(pd.blocks).is_some_and(|end| end <= volume_blocks), "partition outside the volume")?;
    let integrity = extent(&lvd, 432);
    kept.push((integrity.1, integrity.0.div_ceil(block)));
    // The other anchors, where they are.
    for last in [volume_blocks.saturating_sub(257), volume_blocks.saturating_sub(1)] {
        if last > 256 && read_tag(p, block, last, AVDP).is_ok() {
            kept.push((last, 1));
        }
    }
    let v = Volume { block, start: pd.start, blocks: pd.blocks };
    let free = closed(p, block, integrity, pd.access)?;
    let mut used = p.ranges();
    // Everything before the partition: recognition sequence, anchor, descriptor sequences.
    used.add(0, v.start * block);
    let end = if pd.bitmap.0 != 0 {
        bitmap(p, &v, pd.bitmap, free, &mut used)?;
        v.at(v.blocks)
    } else if pd.table.0 != 0 {
        table(p, &v, pd.table, free, &mut used)?;
        v.at(v.blocks)
    } else {
        ensure(!pd.other_space, "only freed space recorded (rewritable media)")?;
        // No free space information: everything up to the last block anything records.
        let last = kept.iter().map(|&(at, n)| at.saturating_add(n)).fold(v.start + v.blocks, u64::max);
        let last = last.saturating_mul(block).max(iso_end(p)?).min(p.size);
        used.add(0, last);
        p.size
    };
    // Descriptors past the partition are in the tail, kept as is; the others are marked here.
    for &(at, n) in &kept {
        let (from, to) = (at.saturating_mul(block), at.saturating_add(n).saturating_mul(block).min(end));
        if from < to {
            used.add(from, to - from);
        }
    }
    Ok(Usage { used, end })
}

fn read_tag(p: &mut Part, block: u64, at: u64, id: u16) -> Res<Vec<u8>> {
    let pos = at.checked_mul(block).ok_or_else(|| Bad("descriptor outside the partition".into()))?;
    let d = p.read_vec(pos, block as usize)?;
    ensure(tagged(&d, id, at), "bad descriptor")?;
    Ok(d)
}

/// The block size, and the anchor volume descriptor pointer: at block 256, else 256 blocks
/// before the last one, else in the last one.
fn find_anchor(p: &mut Part) -> Res<(u64, Vec<u8>)> {
    for block in [512u64, 1024, 2048, 4096] {
        let blocks = p.size / block;
        for at in [256, blocks.saturating_sub(257), blocks.saturating_sub(1)] {
            if at >= 256
                && at < blocks
                && let Ok(d) = read_tag(p, block, at, AVDP)
            {
                return Ok((block, d));
            }
        }
    }
    Err(Bad("no UDF anchor".into()))
}

/// The partition descriptors and the logical volume descriptor of a volume descriptor
/// sequence (the latest of each, by volume descriptor sequence number).
fn sequence(p: &mut Part, block: u64, (len, at): (u64, u64)) -> Res<(Vec<Partition>, Vec<u8>)> {
    let mut partitions: Vec<(u32, Partition)> = Vec::new();
    let mut lvd: Option<(u32, Vec<u8>)> = None;
    let (mut seen, mut hops) = (0, 0);
    let mut next = Some((len / block, at));
    'extents: while let Some((count, first)) = next.take() {
        let pos = first.checked_mul(block).ok_or_else(|| Bad("descriptors outside the partition".into()))?;
        let data = p.read_vec(pos, (count.min(MAX_SEQUENCE) * block) as usize)?;
        for (i, d) in data.chunks_exact(block as usize).enumerate() {
            seen += 1;
            ensure(seen <= MAX_SEQUENCE, "volume descriptor sequence too long")?;
            let id = u16_at(d, 0);
            ensure(tagged(d, id, first + i as u64), "bad volume descriptor")?;
            match id {
                TD => break 'extents,
                VDP => {
                    hops += 1;
                    ensure(hops <= MAX_HOPS, "too many volume descriptor pointers")?;
                    let (len, at) = extent(d, 20);
                    next = Some((len / block, at));
                    continue 'extents;
                }
                PD => {
                    let contents = &d[25..31];
                    ensure(contents == b"+NSR02" || contents == b"+NSR03", "partition holds no UDF")?;
                    let part = Partition {
                        number: u16_at(d, 22),
                        access: u32_at(d, 184),
                        start: u64::from(u32_at(d, 188)),
                        blocks: u64::from(u32_at(d, 192)),
                        // The partition header descriptor: short_ads, the top 2 bits are a type.
                        table: (u64::from(u32_at(d, 56) & 0x3FFF_FFFF), u64::from(u32_at(d, 60))),
                        bitmap: (u64::from(u32_at(d, 64) & 0x3FFF_FFFF), u64::from(u32_at(d, 68))),
                        other_space: u32_at(d, 72) != 0 || u32_at(d, 80) != 0 || u32_at(d, 88) != 0,
                    };
                    let seq = u32_at(d, 16);
                    match partitions.iter_mut().find(|(_, x)| x.number == part.number) {
                        Some(old) if old.0 >= seq => {}
                        Some(old) => *old = (seq, part),
                        None => partitions.push((seq, part)),
                    }
                }
                LVD if lvd.as_ref().is_none_or(|(seq, _)| *seq < u32_at(d, 16)) => {
                    lvd = Some((u32_at(d, 16), d.to_vec()));
                }
                _ => {}
            }
        }
        // A sequence longer than one read goes on where this one ended.
        if count > MAX_SEQUENCE {
            next = Some((count - MAX_SEQUENCE, first + MAX_SEQUENCE));
        }
    }
    let (_, lvd) = lvd.ok_or_else(|| Bad("no logical volume descriptor".into()))?;
    ensure(lvd.len() >= 512 && !partitions.is_empty(), "no partition descriptor")?;
    Ok((partitions.into_iter().map(|(_, x)| x).collect(), lvd))
}

/// Checks the logical volume integrity descriptor says the volume was closed (unless
/// estimating). Returns the free block count it records for the partition, if any.
fn closed(p: &mut Part, block: u64, (len, at): (u64, u64), access: u32) -> Res<Option<u64>> {
    let mut last: Option<Vec<u8>> = None;
    let (mut seen, mut hops) = (0, 0);
    let mut next = Some((len / block, at));
    'extents: while let Some((count, first)) = next.take() {
        for here in first..first.saturating_add(count.min(MAX_SEQUENCE)) {
            seen += 1;
            ensure(seen <= MAX_SEQUENCE, "integrity sequence too long")?;
            let Some(pos) = here.checked_mul(block).filter(|&pos| pos.saturating_add(block) <= p.size) else {
                break 'extents;
            };
            let d = p.read_vec(pos, block as usize)?;
            if !tagged(&d, LVID, here) {
                // A terminator, or unrecorded space: the sequence ends.
                break 'extents;
            }
            let (len, at) = extent(&d, 32);
            last = Some(d);
            if len != 0 {
                hops += 1;
                ensure(hops <= MAX_HOPS, "too many integrity extents")?;
                next = Some((len / block, at));
                continue 'extents;
            }
        }
    }
    let Some(lvid) = last else {
        // Read-only media may do without.
        ensure(access == 1, "no logical volume integrity descriptor")?;
        return Ok(None);
    };
    if p.opts.mode == Mode::Copy {
        ensure(u32_at(&lvid, 28) == 1, "volume still open (not cleanly unmounted)")?;
    }
    // The free space table, one count per partition map (0xFFFFFFFF: not kept).
    let free = u32_at(&lvid, 80);
    Ok((u32_at(&lvid, 72) >= 1 && free != u32::MAX).then_some(u64::from(free)))
}

/// Marks the partition's allocated blocks per its unallocated space bitmap (set bits are free).
fn bitmap(p: &mut Part, v: &Volume, (len, pos): (u64, u64), free: Option<u64>, used: &mut Ranges) -> Res<()> {
    ensure(pos < v.blocks, "space bitmap outside the partition")?;
    let head = p.read_vec(v.at(pos), v.block as usize)?;
    ensure(tagged(&head, SBD, pos), "bad space bitmap")?;
    let bits = u64::from(u32_at(&head, 16));
    let bytes = u64::from(u32_at(&head, 20));
    ensure(bits == v.blocks && bytes == bits.div_ceil(8) && 24 + bytes <= len, "space bitmap size mismatch")?;
    // The bitmap's own blocks, which must be allocated.
    let own = (pos, pos + (24 + bytes).div_ceil(v.block));
    ensure(own.1 <= v.blocks, "space bitmap outside the partition")?;
    let (base, unit) = (v.at(0), v.block);
    let mut allocated = 0u64;
    let mut own_free = false;
    let mut done = 0u64;
    while done < bytes {
        let n = (bytes - done).min(CHUNK as u64);
        let map = p.read_vec(v.at(pos) + 24 + done, n as usize)?;
        let first = done * 8;
        let span = (bits - first).min(n * 8);
        let bit = |b: u64| map[((b - first) / 8) as usize] >> ((b - first) % 8) & 1 == 1;
        own_free |= (own.0.max(first)..own.1.min(first + span)).any(bit);
        let inverted: Vec<u8> = map.iter().map(|b| !b).collect();
        bit_runs(&inverted, span, first, |b, n| {
            allocated += n;
            used.add(base + b * unit, n * unit);
        });
        done += n;
    }
    if p.opts.mode == Mode::Copy {
        ensure(!own_free, "space bitmap marks itself free")?;
        ensure(free.is_none_or(|free| free == bits - allocated), "free block count doesn't match the space bitmap")?;
    }
    Ok(())
}

/// Marks the partition's allocated blocks per its unallocated space table: short allocation
/// descriptors of the free extents, in the entry's block.
fn table(p: &mut Part, v: &Volume, (_, pos): (u64, u64), free: Option<u64>, used: &mut Ranges) -> Res<()> {
    ensure(pos < v.blocks, "space table outside the partition")?;
    let entry = p.read_vec(v.at(pos), v.block as usize)?;
    ensure(tagged(&entry, USE, pos), "bad space table")?;
    // The ICB tag's flags: short_ad allocation descriptors.
    ensure(u16_at(&entry, 16 + 18) & 7 == 0, "unsupported allocation descriptors")?;
    let len = u32_at(&entry, 36) as usize;
    let descriptors = entry.get(40..40 + len).ok_or_else(|| Bad("space table longer than a block".into()))?;
    ensure(len.is_multiple_of(8), "bad space table length")?;
    let mut extents: Vec<(u64, u64)> = Vec::with_capacity(len / 8);
    for ad in descriptors.as_chunks::<8>().0 {
        let (raw, at) = (u32_at(ad, 0), u64::from(u32_at(ad, 4)));
        ensure(raw >> 30 != 3, "continued space table")?;
        let n = u64::from(raw & 0x3FFF_FFFF).div_ceil(v.block);
        ensure(at.checked_add(n).is_some_and(|end| end <= v.blocks), "free extent outside the partition")?;
        if n > 0 {
            extents.push((at, n));
        }
    }
    extents.sort_unstable();
    let mut next = 0;
    let mut total = 0;
    for (at, n) in extents {
        ensure(at >= next, "overlapping free extents")?;
        ensure(pos < at || pos >= at + n, "space table in free space")?;
        used.add(v.at(next), (at - next) * v.block);
        (next, total) = (at + n, total + n);
    }
    used.add(v.at(next), (v.blocks - next) * v.block);
    if p.opts.mode == Mode::Copy {
        ensure(free.is_none_or(|free| free == total), "free block count doesn't match the space table")?;
    }
    Ok(())
}

/// Where an ISO 9660 volume sharing the partition ends (bridge discs), else 0.
fn iso_end(p: &mut Part) -> Res<u64> {
    if p.size < 0x8800 {
        return Ok(0);
    }
    let pvd = p.read_vec(0x8000, 2048)?;
    if pvd[0] != 1 || !at(&pvd, 1, b"CD001") {
        return Ok(0);
    }
    let (blocks, block) = (u32_at(&pvd, 80), u16_at(&pvd, 128));
    ensure(blocks == be32_at(&pvd, 84) && block == be16_at(&pvd, 130), "inconsistent ISO 9660 descriptor")?;
    Ok(u64::from(blocks) * u64::from(block))
}

/// A dstring: a compression ID (8: one byte per character, 16: UTF-16BE), the characters,
/// and in the last byte how many bytes of it are used (the ID included).
fn dstring(d: &[u8]) -> Option<String> {
    let len = usize::from(*d.last()?).min(d.len() - 1);
    let chars = d.get(1..len)?;
    let s = match d[0] {
        8 => chars.iter().map(|&c| char::from(c)).collect(),
        16 => String::from_utf16_lossy(
            &chars.as_chunks::<2>().0.iter().map(|&c| u16::from_be_bytes(c)).collect::<Vec<_>>(),
        ),
        _ => return None,
    };
    let s = s.trim_end_matches(['\0', ' ']).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_itu_check_value() {
        assert_eq!(super::crc_itu(b"123456789"), 0x31C3);
    }
}
