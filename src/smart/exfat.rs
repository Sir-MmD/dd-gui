//! exFAT: the allocation bitmap says which clusters are used. The directory tree is walked
//! as well, and every cluster a file or directory claims counts too: when a volume wasn't
//! cleanly unmounted, its bitmap may lag behind its directories.

use super::util::{Bad, CHUNK, Part, Ranges, Res, Usage, at, bit_runs, ensure, u16_at, u32_at, u64_at, utf16_label};
use std::collections::HashSet;

/// Directories are at most 256 MiB.
const MAX_DIR: u64 = 256 << 20;
/// Directory entries read at most, all directories together.
const MAX_ENTRIES: u64 = 1 << 26;
const MAX_DEPTH: u32 = 4096;

/// Where things are, in bytes from the start of the partition.
struct Vol {
    fat: u64,
    heap: u64,
    cluster: u64,
    clusters: u32,
}

impl Vol {
    fn valid(&self, c: u32) -> bool {
        c >= 2 && c - 2 < self.clusters
    }

    fn at(&self, c: u32) -> u64 {
        self.heap + u64::from(c - 2) * self.cluster
    }
}

/// Contiguous clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    first: u32,
    count: u32,
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let b = head.get(..512).ok_or_else(|| Bad("partition too small for exFAT".into()))?;
    ensure(at(b, 3, b"EXFAT   ") && b[11..64].iter().all(|&x| x == 0), "not exFAT")?;
    let bps_shift = u32::from(b[108]);
    let spc_shift = u32::from(b[109]);
    ensure((9..=12).contains(&bps_shift) && bps_shift + spc_shift <= 25, "bad sector or cluster size")?;
    let bps = 1u64 << bps_shift;
    let cluster = 1u64 << (bps_shift + spc_shift);
    let volume = u64_at(b, 72);
    let fat_offset = u64::from(u32_at(b, 80));
    let fat_len = u64::from(u32_at(b, 84));
    let heap_offset = u64::from(u32_at(b, 88));
    let clusters = u32_at(b, 92);
    let root = u32_at(b, 96);
    let flags = u16_at(b, 106);
    let fats = u64::from(b[110]);
    ensure(fats == 1 || fats == 2, "bad FAT count")?;
    ensure(fat_offset >= 24 && heap_offset >= fat_offset + fat_len * fats, "overlapping regions")?;
    ensure((1..=0xFFFF_FFF5).contains(&clusters), "bad cluster count")?;
    ensure(fat_len * bps >= (u64::from(clusters) + 2) * 4, "FAT too small")?;
    let volume = volume.checked_mul(bps).filter(|&v| v <= p.size);
    let volume = volume.ok_or_else(|| Bad("file system larger than its partition".into()))?;
    let end = heap_offset * bps + u64::from(clusters) * cluster;
    ensure(end <= volume, "cluster heap past the end of the volume")?;

    // The main boot region ends with a checksum of its first 11 sectors.
    let region = p.read_vec(0, (12 * bps) as usize)?;
    let (sectors, sums) = region.split_at((11 * bps) as usize);
    let sum = boot_checksum(sectors);
    ensure(sums.as_chunks::<4>().0.iter().all(|c| u32::from_le_bytes(*c) == sum), "boot checksum mismatch")?;

    let active = if fats == 2 && flags & 1 != 0 { fat_len } else { 0 };
    let v = Vol { fat: (fat_offset + active) * bps, heap: heap_offset * bps, cluster, clusters };
    ensure(v.valid(root), "bad root directory cluster")?;
    let mut w = Walk {
        used: p.ranges(),
        v,
        entries: 0,
        chained: 0,
        seen: HashSet::from([root]),
        bitmaps: Vec::new(),
        upcase: None,
        label: None,
        todo: Vec::new(),
    };
    // Boot regions and FATs.
    w.used.add(0, w.v.heap);

    let runs = w.follow(p, root, None, false, true)?;
    w.dir(p, &runs, true, 0)?;
    *label = w.label.take();

    ensure((1..=2).contains(&w.bitmaps.len()), "no allocation bitmap")?;
    for (first, len) in std::mem::take(&mut w.bitmaps) {
        let need = u64::from(clusters).div_ceil(8);
        ensure(len >= need, "allocation bitmap too small")?;
        let run = w.system_file(p, first, len)?;
        let start = w.v.at(run.first);
        let mut done = 0;
        while done < need {
            let n = (CHUNK as u64).min(need - done);
            let map = p.read_vec(start + done, n as usize)?;
            let bits = (u64::from(clusters) - done * 8).min(n * 8);
            let (heap, size) = (w.v.heap, w.v.cluster);
            bit_runs(&map, bits, done * 8, |first, count| w.used.add(heap + first * size, count * size));
            done += n;
        }
    }
    if let Some((first, len)) = w.upcase {
        w.system_file(p, first, len)?;
    }
    while let Some((runs, depth)) = w.todo.pop() {
        w.dir(p, &runs, false, depth)?;
    }
    Ok(Usage { used: w.used, end })
}

/// The boot region checksum: every byte of the first 11 sectors but VolumeFlags and PercentInUse.
fn boot_checksum(sectors: &[u8]) -> u32 {
    sectors.iter().enumerate().fold(0u32, |sum, (i, &b)| {
        if matches!(i, 106 | 107 | 112) { sum } else { sum.rotate_right(1).wrapping_add(u32::from(b)) }
    })
}

struct Walk {
    v: Vol,
    used: Ranges,
    entries: u64,
    /// Clusters visited following FAT chains.
    chained: u64,
    /// First clusters of the directories found, against loops.
    seen: HashSet<u32>,
    /// (first cluster, bytes) of the allocation bitmaps and the up-case table.
    bitmaps: Vec<(u32, u64)>,
    upcase: Option<(u32, u64)>,
    label: Option<String>,
    /// Directories still to read, with their depth.
    todo: Vec<(Vec<Run>, u32)>,
}

impl Walk {
    fn mark(&mut self, run: Run) {
        self.used.add(self.v.at(run.first), u64::from(run.count) * self.v.cluster);
    }

    fn fat_entry(&self, p: &mut Part, c: u32) -> Res<u32> {
        let mut buf = [0; 4];
        p.read_cached(self.v.fat + u64::from(c) * 4, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Marks the clusters of an allocation starting at `first` as used: `count` of them,
    /// contiguous or chained in the FAT (without a count, the chain is followed to its end).
    /// Returns them when `keep` is set (directories, to be read next).
    fn follow(&mut self, p: &mut Part, first: u32, count: Option<u64>, contiguous: bool, keep: bool) -> Res<Vec<Run>> {
        ensure(self.v.valid(first), "cluster out of range")?;
        let clusters = u64::from(self.v.clusters);
        if contiguous {
            let n = count.unwrap_or(1);
            ensure(n >= 1 && u64::from(first - 2) + n <= clusters, "allocation past the end of the volume")?;
            let run = Run { first, count: n as u32 };
            self.mark(run);
            return Ok(if keep { vec![run] } else { Vec::new() });
        }
        let max = match count {
            Some(n) => n,
            None => (MAX_DIR / self.v.cluster).clamp(1, clusters),
        };
        ensure(max >= 1 && max <= clusters, "allocation larger than the volume")?;
        let mut runs = Vec::new();
        let mut run = Run { first, count: 0 };
        let (mut c, mut n) = (first, 0);
        loop {
            ensure(self.v.valid(c), "broken cluster chain")?;
            // In a sound volume no cluster is in two chains: this bounds the work on a broken one.
            self.chained += 1;
            ensure(self.chained <= 2 * clusters + 1024, "cross-linked or looping cluster chains")?;
            if run.first + run.count == c {
                run.count += 1;
            } else {
                self.mark(run);
                if keep {
                    runs.push(run);
                }
                run = Run { first: c, count: 1 };
            }
            n += 1;
            if count == Some(n) {
                break;
            }
            let next = self.fat_entry(p, c)?;
            if next >= 0xFFFF_FFF8 {
                ensure(count.is_none(), "cluster chain too short")?;
                break;
            }
            ensure(n < max, "directory too large, or a loop in its chain")?;
            c = next;
        }
        self.mark(run);
        if keep {
            runs.push(run);
        }
        Ok(runs)
    }

    /// The allocation bitmap or the up-case table: contiguous, as Linux reads them. A FAT
    /// chain saying otherwise makes the volume ambiguous.
    fn system_file(&mut self, p: &mut Part, first: u32, len: u64) -> Res<Run> {
        let count = len.div_ceil(self.v.cluster).max(1);
        let run = self.follow(p, first, Some(count), true, true)?[0];
        if let Ok(chain) = self.follow(p, first, Some(count), false, true) {
            ensure(chain == [run], "fragmented system file")?;
        }
        Ok(run)
    }

    /// Reads a directory up to its end marker, noting everything its entries allocate.
    fn dir(&mut self, p: &mut Part, runs: &[Run], root: bool, depth: u32) -> Res<()> {
        // Attributes of a File entry, until its Stream Extension shows up.
        let mut file_is_dir = None;
        for r in runs {
            let start = self.v.at(r.first);
            let len = u64::from(r.count) * self.v.cluster;
            let (mut done, mut piece) = (0, 4096);
            while done < len {
                let n = piece.min(len - done);
                let buf = p.read_vec(start + done, n as usize)?;
                for e in buf.as_chunks::<32>().0 {
                    self.entries += 1;
                    ensure(self.entries <= MAX_ENTRIES, "too many directory entries")?;
                    if !self.entry(p, e, root, depth, &mut file_is_dir)? {
                        return Ok(());
                    }
                }
                done += n;
                piece = (piece * 2).min(1 << 20);
            }
        }
        Ok(())
    }

    /// Handles one directory entry; false at the end of the directory.
    fn entry(&mut self, p: &mut Part, e: &[u8], root: bool, depth: u32, file_is_dir: &mut Option<bool>) -> Res<bool> {
        let kind = e[0];
        if kind & 0x40 == 0 {
            // A primary entry starts a new entry set.
            *file_is_dir = None;
        }
        match kind {
            0x00 => return Ok(false),
            // Unused or deleted.
            0x01..=0x7F => {}
            0x81 if root => self.bitmaps.push((u32_at(e, 20), u64_at(e, 24))),
            0x82 if root => self.upcase = Some((u32_at(e, 20), u64_at(e, 24))),
            0x83 if root => self.label = utf16_label(&e[2..2 + 2 * usize::from(e[1]).min(11)]),
            0x85 => *file_is_dir = Some(u16_at(e, 4) & 0x10 != 0),
            // Stream Extension: the data of a file or directory.
            0xC0 => {
                let dir = file_is_dir.take() == Some(true);
                self.allocation(p, e, e[1], dir, depth)?;
            }
            // File Name
            0xC1 => {}
            // Benign primary (GUID, padding, access control) and secondary (vendor) entries.
            0xA0..=0xBF => self.allocation(p, e, u16_at(e, 2) as u8, false, depth)?,
            0xE0..=0xFF => self.allocation(p, e, e[1], false, depth)?,
            _ => return Err(Bad(format!("unknown critical directory entry {kind:#04x}"))),
        }
        Ok(true)
    }

    /// Marks what an entry allocates (flags: bit 0 allocation possible, bit 1 no FAT chain).
    fn allocation(&mut self, p: &mut Part, e: &[u8], flags: u8, dir: bool, depth: u32) -> Res<()> {
        let (first, len) = (u32_at(e, 20), u64_at(e, 24));
        if flags & 1 == 0 || len == 0 {
            return Ok(());
        }
        let count = len.div_ceil(self.v.cluster);
        let runs = self.follow(p, first, Some(count), flags & 2 != 0, dir)?;
        if dir {
            ensure(len <= MAX_DIR && depth < MAX_DEPTH, "directory too large or too deep")?;
            ensure(self.seen.insert(first), "directory reachable twice")?;
            self.todo.push((runs, depth + 1));
        }
        Ok(())
    }
}
