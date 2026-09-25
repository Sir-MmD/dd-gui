//! QCOW2 (QEMU), versions 2 and 3: an L1 table of L2 tables of cluster entries. Clusters
//! are stored as is, compressed (deflate, or zstd when the header says so), marked as
//! zeros, or not allocated (zeros). Extended L2 entries (32 subclusters per cluster) are
//! supported. The active L1 table is read; internal snapshots are ignored. Backing files,
//! encryption and external data files aren't supported.

use super::codec::Codec;
use super::source::{Source, bad, be32, be64, size_text, unsupported};
use super::vdisk::{Layout, Packed, Piece, Run, Table, VDisk};
use super::{Detected, ImageFormat, Opened};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

pub const MAGIC: &[u8] = b"QFI\xfb";

const DIRTY: u64 = 1;
const CORRUPT: u64 = 2;
const EXTERNAL_DATA: u64 = 4;
const COMPRESSION_TYPE: u64 = 8;
const EXTENDED_L2: u64 = 16;
/// Bits 9–55 of an entry: a host offset.
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const COMPRESSED: u64 = 1 << 62;

struct Header {
    version: u32,
    size: u64,
    cluster_bits: u32,
    l1_offset: u64,
    l1_size: u64,
    extended_l2: bool,
    zstd: bool,
}

pub fn detect(src: &Source, head: &[u8]) -> Option<Detected> {
    head.starts_with(MAGIC).then(|| Detected {
        format: ImageFormat::Qcow2,
        raw_size: header(src).ok().map(|h| h.size),
    })
}

pub fn open(src: Source) -> io::Result<Opened> {
    let h = header(&src)?;
    let cluster = 1u64 << h.cluster_bits;
    let entry_size = if h.extended_l2 { 16 } else { 8 };
    let l2_entries = cluster / entry_size as u64;
    // For compressed clusters: x = 62 - (cluster_bits - 8) bits of offset, then the
    // number of extra 512-byte sectors.
    let csize_shift = 62 - (h.cluster_bits - 8);
    let layout = Clusters {
        l1: Table::new(h.l1_offset, h.l1_size, 8),
        cluster,
        l2_entries,
        entry_size,
        l2: Vec::new(),
        l2_loaded: None,
        next: 0,
        clusters: h.size.div_ceil(cluster),
        csize_shift,
        csize_mask: (1u64 << (h.cluster_bits - 8)) - 1,
        offset_mask: (1u64 << csize_shift) - 1,
        codec: if h.zstd { Codec::Zstd } else { Codec::Deflate },
        zero_flag: h.version >= 3 && !h.extended_l2,
        extended: h.extended_l2,
        pending: VecDeque::new(),
    };
    let size = h.size;
    Ok(Opened {
        reader: Box::new(VDisk::new(Arc::new(src), size, Box::new(layout))),
        size: Some(size),
    })
}

fn header(src: &Source) -> io::Result<Header> {
    let b = src.read_upto(0, 4096)?;
    if b.len() < 72 || !b.starts_with(MAGIC) {
        return Err(bad("the QCOW2 header is damaged"));
    }
    let version = be32(&b, 4);
    if version == 1 {
        return Err(unsupported(
            "this is a QCOW version 1 image, which DD-GUI doesn't support. Convert it first: `qemu-img convert -O qcow2 old.qcow new.qcow2`.",
        ));
    }
    if version != 2 && version != 3 {
        return Err(unsupported(format!(
            "this QCOW2 image has version {version}, which DD-GUI doesn't know"
        )));
    }
    let cluster_bits = be32(&b, 20);
    let size = be64(&b, 24);
    let (backing_offset, backing_len) = (be64(&b, 8), be32(&b, 16));
    if backing_offset != 0 {
        let name = src
            .read_upto(backing_offset, backing_len.min(1023) as usize)
            .map(|n| String::from_utf8_lossy(&n).into_owned())
            .unwrap_or_default();
        return Err(unsupported(format!(
            "this QCOW2 image is an overlay: it only holds the changes made on top of another image{}, which DD-GUI doesn't support. Make a whole image first: `qemu-img convert -O qcow2 overlay.qcow2 whole.qcow2`.",
            if name.is_empty() {
                String::new()
            } else {
                format!(" ({name})")
            }
        )));
    }
    if be32(&b, 32) != 0 {
        return Err(unsupported(
            "this QCOW2 image is encrypted, which DD-GUI doesn't support. Decrypt it first with `qemu-img convert`.",
        ));
    }
    let (mut incompatible, mut compression_type, mut header_len) = (0u64, 0u8, 72usize);
    if version == 3 {
        if b.len() < 104 {
            return Err(bad("the QCOW2 header is damaged"));
        }
        incompatible = be64(&b, 72);
        header_len = be32(&b, 100) as usize;
        if header_len > 104 {
            compression_type = b.get(104).copied().unwrap_or(0);
        }
    }
    if incompatible & EXTERNAL_DATA != 0 {
        let name = extension(&b, header_len, 0x4441_5441)
            .map(|n| format!(" ({})", String::from_utf8_lossy(&n)))
            .unwrap_or_default();
        return Err(unsupported(format!(
            "this QCOW2 image keeps its data in a separate file{name}, which DD-GUI doesn't support. Convert it first: `qemu-img convert -O qcow2 in.qcow2 out.qcow2`."
        )));
    }
    let unknown = incompatible & !(DIRTY | CORRUPT | COMPRESSION_TYPE | EXTENDED_L2);
    if unknown != 0 {
        return Err(unsupported(format!(
            "this QCOW2 image uses features DD-GUI doesn't know (incompatible feature bits {unknown:#x})"
        )));
    }
    let zstd = match (incompatible & COMPRESSION_TYPE != 0, compression_type) {
        (false, _) | (true, 0) => false,
        (true, 1) => true,
        (true, other) => {
            return Err(unsupported(format!(
                "this QCOW2 image uses compression type {other}, which DD-GUI doesn't know"
            )));
        }
    };
    let extended_l2 = incompatible & EXTENDED_L2 != 0;
    if !(9..=21).contains(&cluster_bits) || (extended_l2 && cluster_bits < 14) {
        return Err(bad(format!(
            "the QCOW2 header is damaged (cluster size 2^{cluster_bits})"
        )));
    }
    if size > 1 << 56 {
        return Err(bad(format!(
            "the QCOW2 header is damaged (a disk of {})",
            size_text(size)
        )));
    }
    let l1_size = be32(&b, 36) as u64;
    let l1_offset = be64(&b, 40);
    let per_l2 = (1u64 << cluster_bits) / if extended_l2 { 16 } else { 8 };
    let needed = size.div_ceil((1u64 << cluster_bits) * per_l2);
    if l1_size < needed || l1_size > (32 << 20) / 8 {
        return Err(bad(
            "the QCOW2 image is damaged (its L1 table doesn't fit the disk)",
        ));
    }
    if !l1_offset.is_multiple_of(1 << cluster_bits) {
        return Err(bad(
            "the QCOW2 image is damaged (its L1 table is misplaced)",
        ));
    }
    Ok(Header {
        version,
        size,
        cluster_bits,
        l1_offset,
        l1_size,
        extended_l2,
        zstd,
    })
}

/// A header extension's data, from the first cluster.
fn extension(b: &[u8], mut at: usize, wanted: u32) -> Option<Vec<u8>> {
    while at + 8 <= b.len() {
        let (kind, len) = (be32(b, at), be32(b, at + 4) as usize);
        if kind == 0 {
            return None;
        }
        let data = b.get(at + 8..at + 8 + len)?;
        if kind == wanted {
            return Some(data.to_vec());
        }
        at += 8 + len.div_ceil(8) * 8;
    }
    None
}

struct Clusters {
    l1: Table,
    cluster: u64,
    l2_entries: u64,
    entry_size: usize,
    l2: Vec<u8>,
    /// Which L1 entry `l2` belongs to.
    l2_loaded: Option<u64>,
    /// The next guest cluster.
    next: u64,
    clusters: u64,
    csize_shift: u32,
    csize_mask: u64,
    offset_mask: u64,
    codec: Codec,
    /// Version 3 without extended entries: bit 0 marks a cluster of zeros.
    zero_flag: bool,
    extended: bool,
    /// Subcluster pieces not handed out yet.
    pending: VecDeque<Piece>,
}

impl Layout for Clusters {
    fn next(&mut self, src: &Source) -> io::Result<Option<Piece>> {
        if let Some(p) = self.pending.pop_front() {
            return Ok(Some(p));
        }
        if self.next >= self.clusters {
            return Ok(None);
        }
        let l1_index = self.next / self.l2_entries;
        let l2_index = self.next % self.l2_entries;
        let l2_offset = self
            .l1
            .get(src, l1_index)?
            .map_or(0, |e| be64(e, 0) & OFFSET_MASK);
        if l2_offset == 0 {
            // No L2 table: all of its clusters are unallocated.
            let n = (self.l2_entries - l2_index).min(self.clusters - self.next);
            self.next += n;
            return Ok(Some(Piece::zeros(n * self.cluster)));
        }
        if self.l2_loaded != Some(l1_index) {
            if l2_offset % self.cluster != 0 {
                return Err(bad(format!(
                    "the QCOW2 image is damaged (L2 table {l1_index} is misplaced)"
                )));
            }
            self.l2 = src.read_vec(l2_offset, self.cluster as usize)?;
            self.l2_loaded = Some(l1_index);
        }
        let at = l2_index as usize * self.entry_size;
        let entry = be64(&self.l2, at);
        let guest = self.next;
        self.next += 1;
        let cluster = self.cluster;
        if entry & COMPRESSED != 0 {
            let offset = entry & self.offset_mask;
            let sectors = ((entry >> self.csize_shift) & self.csize_mask) + 1;
            let len = sectors * 512 - (offset & 511);
            return Ok(Some(Piece {
                len: cluster,
                run: Run::Packed(Packed {
                    offset,
                    len: len as u32,
                    codec: self.codec,
                    size: cluster as u32,
                    short_ok: false,
                }),
            }));
        }
        let host = entry & OFFSET_MASK;
        if !host.is_multiple_of(cluster) {
            return Err(bad(format!(
                "the QCOW2 image is damaged (cluster {guest} is misplaced)"
            )));
        }
        if self.extended {
            let bitmap = be64(&self.l2, at + 8);
            if host == 0 {
                return Ok(Some(Piece::zeros(cluster)));
            }
            // Allocated subclusters are stored; the others read as zeros (there's no
            // backing file to read them from).
            let sub = cluster / 32;
            for i in 0..32 {
                self.pending.push_back(if bitmap & (1 << i) != 0 {
                    Piece::stored(host + i * sub, sub)
                } else {
                    Piece::zeros(sub)
                });
            }
            return Ok(self.pending.pop_front());
        }
        if (self.zero_flag && entry & 1 != 0) || host == 0 {
            return Ok(Some(Piece::zeros(cluster)));
        }
        Ok(Some(Piece::stored(host, cluster)))
    }
}
