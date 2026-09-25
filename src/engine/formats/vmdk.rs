//! VMDK (VMware), in its single-file variants: monolithicSparse (a sparse extent with a
//! grain directory and grain tables) and streamOptimized (the same with deflated grains
//! behind markers, the directory at the end). Descriptor-only disks (flat, split, ESXi
//! VMFS extents), snapshots (delta disks) and ESXi's COWD/SEsparse formats aren't
//! supported; the errors say which file to use or how to convert.

use super::codec::Codec;
use super::source::{Source, bad, le16, le32, le64, size_text, unsupported};
use super::vdisk::{Layout, Packed, Piece, Run, Table, VDisk};
use super::{Detected, ImageFormat, Opened};
use std::io;
use std::sync::Arc;

pub const MAGIC: &[u8] = b"KDMV";
const COWD: &[u8] = b"COWD";
/// SEsparse: 0x00000000cafebabe, little-endian.
const SESPARSE: &[u8] = &[0xbe, 0xba, 0xfe, 0xca, 0, 0, 0, 0];
const DESCRIPTOR: &[u8] = b"# Disk DescriptorFile";
const GD_AT_END: u64 = u64::MAX;
const FLAG_ZERO_GRAIN: u32 = 1 << 2;
const FLAG_MARKERS: u32 = 1 << 17;

struct Header {
    version: u32,
    flags: u32,
    capacity: u64,
    grain: u64,
    desc_offset: u64,
    desc_size: u64,
    gtes_per_gt: u32,
    gd_offset: u64,
    compress: u16,
}

fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < 79 || !b.starts_with(MAGIC) {
        return None;
    }
    Some(Header {
        version: le32(b, 4),
        flags: le32(b, 8),
        capacity: le64(b, 12),
        grain: le64(b, 20),
        desc_offset: le64(b, 28),
        desc_size: le64(b, 36),
        gtes_per_gt: le32(b, 44),
        gd_offset: le64(b, 56),
        compress: le16(b, 77),
    })
}

pub fn detect(src: &Source, head: &[u8]) -> Option<Detected> {
    let text = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    if head.starts_with(MAGIC) {
        return Some(Detected {
            format: ImageFormat::Vmdk,
            raw_size: sparse(src).ok().map(|(h, _)| h.capacity * 512),
        });
    }
    (head.starts_with(COWD) || head.starts_with(SESPARSE) || text.starts_with(DESCRIPTOR))
        .then_some(Detected {
            format: ImageFormat::Vmdk,
            raw_size: None,
        })
}

pub fn open(src: Source) -> io::Result<Opened> {
    let head = src.read_upto(0, 512)?;
    if head.starts_with(COWD) || head.starts_with(SESPARSE) {
        return Err(unsupported(
            "this is an ESXi sparse VMDK (usually a snapshot's delta disk), which DD-GUI doesn't support. Convert it to a whole disk first, e.g. `vmkfstools -i disk.vmdk -d thin whole.vmdk` on the ESXi host, or `qemu-img convert -O raw disk.vmdk disk.img`.",
        ));
    }
    if !head.starts_with(MAGIC) {
        // A text descriptor: the data is elsewhere.
        let text = src.read_upto(0, 64 << 10)?;
        return Err(descriptor_only(&String::from_utf8_lossy(&text)));
    }
    let (h, _) = sparse(&src)?;
    let grain = h.grain * 512;
    let span = h.gtes_per_gt as u64 * h.grain;
    // As QEMU: the algorithm field says (the "compressed" flag goes with it).
    let compressed = match h.compress {
        0 => false,
        1 => true,
        other => {
            return Err(unsupported(format!(
                "this VMDK uses a compression DD-GUI doesn't know ({other})"
            )));
        }
    };
    let layout = Grains {
        gd: Table::new(
            h.gd_offset.saturating_mul(512),
            h.capacity.div_ceil(span),
            4,
        ),
        gt: Vec::new(),
        gt_loaded: None,
        gtes_per_gt: h.gtes_per_gt as u64,
        grain,
        next: 0,
        grains: h.capacity.div_ceil(h.grain),
        compressed,
        markers: h.flags & FLAG_MARKERS != 0,
        zero_grain: h.flags & FLAG_ZERO_GRAIN != 0,
    };
    let size = h.capacity * 512;
    Ok(Opened {
        reader: Box::new(VDisk::new(Arc::new(src), size, Box::new(layout))),
        size: Some(size),
    })
}

/// The sparse extent's header (the footer's copy for streamOptimized files, whose
/// header doesn't know where the directory ends up), after checking that the file is
/// a whole disk on its own.
fn sparse(src: &Source) -> io::Result<(Header, String)> {
    let damaged = || bad("the VMDK header is damaged");
    let mut h = parse_header(&src.read_upto(0, 512)?).ok_or_else(damaged)?;
    if h.capacity == 0 && h.desc_offset != 0 {
        let text = read_descriptor(src, &h)?;
        return Err(descriptor_only(&text));
    }
    let descriptor = read_descriptor(src, &h)?;
    if h.gd_offset == GD_AT_END {
        // The footer: a marker sector, a copy of the header, the end-of-stream marker.
        if src.size < 1536 {
            return Err(damaged());
        }
        let tail = src.read_vec(src.size - 1536, 1536)?;
        let footer = parse_header(&tail[512..1024])
            .ok_or_else(|| bad("the VMDK is damaged or incomplete (its footer is missing)"))?;
        if le32(&tail, 12) != 3 || le32(&tail, 1024 + 12) != 0 {
            return Err(bad(
                "the VMDK is damaged or incomplete (its footer is missing)",
            ));
        }
        h = footer;
    }
    if h.version > 3 {
        return Err(unsupported(format!(
            "this VMDK has version {}, which DD-GUI doesn't know",
            h.version
        )));
    }
    if h.grain == 0 || h.grain > 1 << 17 || h.gtes_per_gt == 0 || h.gtes_per_gt > 512 {
        return Err(damaged());
    }
    if h.capacity > 1 << 44 || h.capacity.div_ceil(h.gtes_per_gt as u64 * h.grain) > 32 << 20 {
        return Err(bad(format!(
            "the VMDK header is damaged (a disk of {})",
            size_text(h.capacity.saturating_mul(512))
        )));
    }
    if descriptor.is_empty() {
        return Err(unsupported(
            "this file is one part of a split VMDK (the disk is spread over several files, with a small .vmdk descriptor that lists them), which DD-GUI doesn't support. Convert the whole disk to one file first: `qemu-img convert -O vmdk -o subformat=streamOptimized disk.vmdk whole.vmdk` (using the descriptor).",
        ));
    }
    let create_type = value(&descriptor, "createType").unwrap_or_default();
    if let Some(parent) = value(&descriptor, "parentCID")
        && !parent.eq_ignore_ascii_case("ffffffff")
    {
        let hint = value(&descriptor, "parentFileNameHint")
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        return Err(unsupported(format!(
            "this VMDK is a snapshot: it only holds the changes made on top of another disk{hint}, which DD-GUI doesn't support. Consolidate the snapshots in VMware first, or convert it: `qemu-img convert -O raw disk.vmdk disk.img` (next to its parent)."
        )));
    }
    let extents = extents(&descriptor);
    if !matches!(
        create_type.to_ascii_lowercase().as_str(),
        "monolithicsparse" | "streamoptimized" | ""
    ) || extents.len() > 1
    {
        return Err(unsupported(format!(
            "this VMDK ({}) spreads the disk over several files, which DD-GUI doesn't support. Convert it to one file first: `qemu-img convert -O raw disk.vmdk disk.img`.",
            if create_type.is_empty() {
                "several extents"
            } else {
                &create_type
            }
        )));
    }
    Ok((h, descriptor))
}

/// The embedded descriptor ("" if there's none).
fn read_descriptor(src: &Source, h: &Header) -> io::Result<String> {
    if h.desc_offset == 0 || h.desc_size == 0 {
        return Ok(String::new());
    }
    let len = h.desc_size.saturating_mul(512).min(1 << 20) as usize;
    let text = src.read_upto(h.desc_offset.saturating_mul(512), len)?;
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    Ok(String::from_utf8_lossy(&text[..end]).into_owned())
}

/// A `key = "value"` line of a descriptor.
fn value(descriptor: &str, key: &str) -> Option<String> {
    descriptor.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim().eq_ignore_ascii_case(key)).then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// Extent lines: `RW 16777216 SPARSE "disk-flat.vmdk"` → (type, file name).
fn extents(descriptor: &str) -> Vec<(String, String)> {
    descriptor
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let mut words = line.split_whitespace();
            let access = words.next()?;
            if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
                return None;
            }
            words.next()?.parse::<u64>().ok()?;
            let kind = words.next()?.to_owned();
            let name = line
                .split_once('"')
                .and_then(|(_, rest)| rest.split_once('"'))
                .map_or(String::new(), |(n, _)| n.to_owned());
            Some((kind, name))
        })
        .collect()
}

/// The error for a descriptor whose disk lives in other files.
fn descriptor_only(text: &str) -> io::Error {
    let extents = extents(text);
    let names: Vec<&str> = extents.iter().map(|(_, n)| n.as_str()).collect();
    let create_type = value(text, "createType").unwrap_or_default();
    if let [(kind, name)] = extents.as_slice()
        && (kind.eq_ignore_ascii_case("FLAT") || kind.eq_ignore_ascii_case("VMFS"))
    {
        return unsupported(format!(
            "this .vmdk file is only a descriptor: the disk itself is in \"{name}\", next to it. That file is a plain raw image: choose it instead."
        ));
    }
    unsupported(format!(
        "this .vmdk file is only a descriptor{}: the disk itself is in other files{}, which DD-GUI can't read together. Convert it to one file first: `qemu-img convert -O raw disk.vmdk disk.img`.",
        if create_type.is_empty() {
            String::new()
        } else {
            format!(" ({create_type})")
        },
        if names.is_empty() {
            String::new()
        } else {
            format!(" ({})", names.join(", "))
        }
    ))
}

struct Grains {
    /// The grain directory: sector offsets of grain tables.
    gd: Table,
    gt: Vec<u8>,
    gt_loaded: Option<u64>,
    gtes_per_gt: u64,
    /// Bytes per grain.
    grain: u64,
    next: u64,
    grains: u64,
    compressed: bool,
    markers: bool,
    zero_grain: bool,
}

impl Layout for Grains {
    fn next(&mut self, src: &Source) -> io::Result<Option<Piece>> {
        if self.next >= self.grains {
            return Ok(None);
        }
        let table = self.next / self.gtes_per_gt;
        let index = self.next % self.gtes_per_gt;
        let gt_sector = self.gd.get(src, table)?.map_or(0, |e| le32(e, 0) as u64);
        if gt_sector == 0 {
            let n = (self.gtes_per_gt - index).min(self.grains - self.next);
            self.next += n;
            return Ok(Some(Piece::zeros(n * self.grain)));
        }
        if self.gt_loaded != Some(table) {
            self.gt = src.read_vec(gt_sector * 512, self.gtes_per_gt as usize * 4)?;
            self.gt_loaded = Some(table);
        }
        let entry = le32(&self.gt, index as usize * 4) as u64;
        self.next += 1;
        if entry == 0 || (entry == 1 && self.zero_grain) {
            return Ok(Some(Piece::zeros(self.grain)));
        }
        let offset = entry * 512;
        if !self.compressed {
            return Ok(Some(Piece::stored(offset, self.grain)));
        }
        // A compressed grain: with markers, 12 bytes (its sector, the compressed length)
        // come first. Without, the zlib stream starts right away; deflate can grow data
        // a little, so read a bit more than a grain.
        let (data, len) = if self.markers {
            let marker = src.read_vec(offset, 12)?;
            let len = le32(&marker, 8) as u64;
            if len == 0 || len > 2 * self.grain {
                return Err(bad(format!(
                    "the VMDK is damaged (the grain at byte {offset} claims {})",
                    size_text(len)
                )));
            }
            (offset + 12, len)
        } else {
            (offset, self.grain + self.grain / 512 + 1024)
        };
        Ok(Some(Piece {
            len: self.grain,
            run: Run::Packed(Packed {
                offset: data,
                len: len as u32,
                codec: Codec::Zlib,
                size: self.grain as u32,
                // The last grain of a disk may hold less than a whole grain.
                short_ok: true,
            }),
        }))
    }
}
