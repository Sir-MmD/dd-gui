//! FAT12, FAT16 and FAT32: a cluster is used when its FAT entry isn't zero.

use super::util::{Bad, CHUNK, Part, Res, Usage, at, byte_label, ensure, u16_at, u32_at};

/// FAT geometry, from the BIOS parameter block.
struct Geometry {
    /// Bits per FAT entry: 12, 16 or 32.
    bits: u64,
    fats: u64,
    /// First FAT, in bytes from the start of the partition.
    fat: u64,
    /// Bytes per FAT.
    fat_len: u64,
    /// The data area (cluster 2), in bytes.
    data: u64,
    cluster: u64,
    clusters: u64,
}

impl Geometry {
    fn cluster_at(&self, n: u64) -> u64 {
        self.data + (n - 2) * self.cluster
    }
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let b = head.get(..512).ok_or_else(|| Bad("partition too small for FAT".into()))?;
    let g = geometry(b, p.size)?;
    *label = volume_label(p, b, &g);

    let mut used = p.ranges();
    // Boot sector, reserved sectors, the FATs and (FAT12/16) the root directory.
    used.add(0, g.data);
    let bad = match g.bits {
        12 => 0xFF7,
        16 => 0xFFF7,
        _ => 0x0FFF_FFF7,
    };
    // Entries 0 and 1 are reserved; cluster n has entry n. A cluster counts as used when
    // any copy of the FAT says so (they should agree, but may not after a crash).
    let entries = g.clusters + 2;
    let per_chunk = (CHUNK as u64 * 8 / g.bits).max(2);
    let mut first = 0;
    while first < entries {
        let n = per_chunk.min(entries - first);
        let mut in_use = vec![false; n as usize];
        for f in 0..g.fats {
            let fat = g.fat + f * g.fat_len;
            if g.bits == 12 {
                // Small enough to be read in one go (per_chunk covers every entry).
                let table = p.read_vec(fat, (entries * 3).div_ceil(2) as usize)?;
                for (i, slot) in in_use.iter_mut().enumerate() {
                    let v = u16_at(&table, i + i / 2);
                    let v = if i % 2 == 1 { v >> 4 } else { v & 0xFFF };
                    *slot |= v != 0 && u32::from(v) != bad;
                }
            } else {
                let width = g.bits / 8;
                let table = p.read_vec(fat + first * width, (n * width) as usize)?;
                for (i, slot) in in_use.iter_mut().enumerate() {
                    let v =
                        if width == 2 { u32::from(u16_at(&table, 2 * i)) } else { u32_at(&table, 4 * i) & 0x0FFF_FFFF };
                    *slot |= v != 0 && v != bad;
                }
            }
        }
        let mut i = if first == 0 { 2 } else { 0 };
        while i < in_use.len() {
            if !in_use[i] {
                i += 1;
                continue;
            }
            let run = in_use[i..].iter().take_while(|&&u| u).count();
            used.add(g.cluster_at(first + i as u64), run as u64 * g.cluster);
            i += run;
        }
        first += n;
    }
    Ok(Usage { used, end: g.cluster_at(g.clusters + 2) })
}

fn geometry(b: &[u8], part_size: u64) -> Res<Geometry> {
    let bps = u64::from(u16_at(b, 11));
    ensure(bps.is_power_of_two() && (512..=4096).contains(&bps), "bad bytes per sector")?;
    let spc = u64::from(b[13]);
    ensure(spc.is_power_of_two(), "bad sectors per cluster")?;
    let reserved = u64::from(u16_at(b, 14));
    let fats = u64::from(b[16]);
    ensure(reserved > 0 && fats > 0, "no reserved sectors or no FAT")?;
    let root_entries = u64::from(u16_at(b, 17));
    let total = match u16_at(b, 19) {
        0 => u64::from(u32_at(b, 32)),
        n => u64::from(n),
    };
    let fat16_len = u64::from(u16_at(b, 22));
    let fat_len = if fat16_len != 0 { fat16_len } else { u64::from(u32_at(b, 36)) };
    ensure(total > 0 && fat_len > 0, "empty geometry")?;
    let root_sectors = (root_entries * 32).div_ceil(bps);
    let meta = reserved + fats * fat_len + root_sectors;
    ensure(total > meta, "no data area")?;
    let clusters = (total - meta) / spc;
    // The cluster count decides between FAT12 and FAT16, like every OS does.
    let bits = if fat16_len == 0 {
        32
    } else if clusters < 4085 {
        12
    } else {
        16
    };
    let max = match bits {
        12 => 4084,
        16 => 65_524,
        _ => 0x0FFF_FFF5,
    };
    ensure((1..=max).contains(&clusters), "cluster count out of range")?;
    if bits == 32 {
        ensure(root_entries == 0 && u16_at(b, 42) == 0, "unknown FAT32 variant")?;
    } else {
        // A type string contradicting the cluster count means a volume made to be read
        // unusually (mount -o fat=16): don't guess.
        let named = |s: &[u8]| at(b, 0x36, s);
        ensure(!(bits == 12 && named(b"FAT16   ") || bits == 16 && named(b"FAT12   ")), "FAT type is ambiguous")?;
    }
    ensure(fat_len * bps * 8 / bits >= clusters + 2, "FAT too small for the cluster count")?;
    ensure(total * bps <= part_size, "file system larger than its partition")?;
    Ok(Geometry {
        bits,
        fats,
        fat: reserved * bps,
        fat_len: fat_len * bps,
        data: meta * bps,
        cluster: spc * bps,
        clusters,
    })
}

/// The label from the root directory (what Windows shows), else from the boot sector.
fn volume_label(p: &mut Part, b: &[u8], g: &Geometry) -> Option<String> {
    let root = if g.bits == 32 {
        let first = u64::from(u32_at(b, 44) & 0x0FFF_FFFF);
        (2..g.clusters + 2).contains(&first).then(|| (g.cluster_at(first), g.cluster))
    } else {
        let at = g.fat + g.fats * g.fat_len;
        Some((at, g.data - at))
    };
    let from_root = root.and_then(|(at, len)| p.read_vec(at, len.min(16 * 1024) as usize).ok()).and_then(|dir| {
        for e in dir.as_chunks::<32>().0 {
            match (e[0], e[11]) {
                (0, _) => break,
                (0xE5, _) | (_, 0x0F) => continue,
                (_, attr) if attr & 0x18 == 0x08 => return byte_label(&e[..11]),
                _ => continue,
            }
        }
        None
    });
    let boot_sig = if g.bits == 32 { 0x42 } else { 0x26 };
    let from_boot = (b[boot_sig] == 0x29).then(|| byte_label(&b[boot_sig + 5..boot_sig + 16])).flatten();
    from_root.or(from_boot).filter(|l| l != "NO NAME")
}
