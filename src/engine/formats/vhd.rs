//! VHD (Virtual PC, Hyper-V's first format): fixed disks are the raw disk followed by a
//! 512-byte footer; dynamic disks have a copy of the footer at the start, a header
//! ("cxsparse"), a block allocation table, and blocks that each start with a bitmap of
//! the sectors written in them. Differencing disks (changes on top of a parent disk)
//! aren't supported.

use super::source::{Source, bad, be16, be32, be64, size_text, unsupported};
use super::vdisk::{Layout, Piece, Table, VDisk};
use super::{Detected, ImageFormat, Opened};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

const COOKIE: &[u8] = b"conectix";
const FIXED: u32 = 2;
const DYNAMIC: u32 = 3;
const DIFFERENCING: u32 = 4;
const UNALLOCATED: u32 = 0xffff_ffff;

struct Footer {
    data_offset: u64,
    size: u64,
    disk_type: u32,
    checksum_ok: bool,
}

fn parse_footer(b: &[u8]) -> Option<Footer> {
    if b.len() < 511 || !b.starts_with(COOKIE) {
        return None;
    }
    // One's complement of the byte sum, without the checksum field itself.
    let sum = b
        .iter()
        .take(512)
        .enumerate()
        .filter(|(i, _)| !(64..68).contains(i))
        .fold(0u32, |s, (_, &x)| s.wrapping_add(x as u32));
    let current_size = be64(b, 48);
    let (cylinders, heads, sectors) = (be16(b, 56) as u64, b[58] as u64, b[59] as u64);
    let chs = cylinders * heads * sectors * 512;
    // Like QEMU: Virtual PC and old QEMU versions size the disk by its geometry (the
    // footer's size can be larger); everyone else by the size field. The largest
    // geometry means the size didn't fit in it.
    let creator = &b[28..32];
    let size =
        if (creator == b"vpc " || creator == b"qemu") && chs != 65535 * 16 * 255 * 512 && chs > 0 {
            chs
        } else {
            current_size
        };
    Some(Footer {
        data_offset: be64(b, 16),
        size,
        disk_type: be32(b, 60),
        checksum_ok: !sum == be32(b, 64),
    })
}

/// A VHD footer at the end of the file (fixed disks have only that one). Some old
/// Virtual PC versions wrote 511-byte footers.
fn tail_footer(src: &Source) -> io::Result<Option<(Footer, u64)>> {
    for len in [512u64, 511] {
        if src.size < len {
            continue;
        }
        let mut b = src.read_vec(src.size - len, len as usize)?;
        b.resize(512, 0);
        if let Some(f) = parse_footer(&b) {
            return Ok(Some((f, len)));
        }
    }
    Ok(None)
}

/// A dynamic or differencing VHD: the footer's copy is at the start.
pub fn detect_head(head: &[u8]) -> Option<Detected> {
    let footer = parse_footer(head.get(..512)?)?;
    matches!(footer.disk_type, DYNAMIC | DIFFERENCING).then(|| Detected {
        format: ImageFormat::Vhd,
        raw_size: (footer.disk_type == DYNAMIC && footer.checksum_ok).then_some(footer.size),
    })
}

/// A fixed VHD (or a dynamic one whose first footer copy is damaged): the footer at the end.
pub fn detect_tail(src: &Source) -> io::Result<Option<Detected>> {
    Ok(tail_footer(src)?
        .filter(|(f, _)| matches!(f.disk_type, FIXED | DYNAMIC | DIFFERENCING))
        .map(|(f, _)| Detected {
            format: ImageFormat::Vhd,
            raw_size: (f.checksum_ok && f.disk_type != DIFFERENCING).then_some(f.size),
        }))
}

pub fn open(src: Source) -> io::Result<Opened> {
    let src = Arc::new(src);
    let head = src.read_upto(0, 512)?;
    let head_footer = parse_footer(&head).filter(|f| f.checksum_ok);
    let tail = tail_footer(&src)?;
    let footer_len = tail.as_ref().map_or(0, |(_, len)| *len);
    let tail_footer = tail.map(|(f, _)| f).filter(|f| f.checksum_ok);
    let footer = match (head_footer, tail_footer) {
        (Some(f), _) if f.disk_type != FIXED => f,
        (_, Some(f)) => f,
        _ => {
            return Err(bad(
                "the VHD footer is damaged (its checksum doesn't match)",
            ));
        }
    };
    match footer.disk_type {
        FIXED => {
            let data = src.size - footer_len;
            if footer.size > data {
                return Err(bad(format!(
                    "the VHD is cut short: it should hold {} of disk data, but has {}",
                    size_text(footer.size),
                    size_text(data)
                )));
            }
            let size = footer.size;
            Ok(Opened {
                reader: Box::new(VDisk::new(src, size, Box::new(Whole(Some(size))))),
                size: Some(size),
            })
        }
        DYNAMIC | DIFFERENCING => {
            let header = src
                .read_vec(footer.data_offset, 1024)
                .map_err(|_| bad("the VHD is damaged (its dynamic disk header is missing)"))?;
            if !header.starts_with(b"cxsparse") {
                return Err(bad(
                    "the VHD is damaged (its dynamic disk header is missing)",
                ));
            }
            if footer.disk_type == DIFFERENCING {
                return Err(unsupported(format!(
                    "this is a differencing VHD: it only holds the changes made on top of another disk{}, which DD-GUI doesn't support. Merge it into a whole disk first, e.g. with `qemu-img convert -O vpc child.vhd whole.vhd` (next to its parent), or with Hyper-V's Edit Disk.",
                    parent_name(&header).map_or(String::new(), |n| format!(" ({n})"))
                )));
            }
            let table = be64(&header, 16);
            let entries = be32(&header, 28) as u64;
            let block_size = be32(&header, 32) as u64;
            if !block_size.is_power_of_two() || block_size < 512 {
                return Err(bad(format!("the VHD is damaged (block size {block_size})")));
            }
            // The bitmap: one bit per sector, padded to whole sectors.
            let bitmap_size = (block_size / 512).div_ceil(8).div_ceil(512) * 512;
            let size = footer.size;
            let blocks = size.div_ceil(block_size);
            let layout = Dynamic {
                bat: Table::new(table, entries.min(blocks), 4),
                block_size,
                bitmap_size,
                next: 0,
                blocks,
                pending: VecDeque::new(),
            };
            Ok(Opened {
                reader: Box::new(VDisk::new(src, size, Box::new(layout))),
                size: Some(size),
            })
        }
        other => Err(bad(format!(
            "the VHD is damaged (unknown disk type {other})"
        ))),
    }
}

fn parent_name(header: &[u8]) -> Option<String> {
    let units: Vec<u16> = header
        .get(64..576)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_be_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    let name = String::from_utf16_lossy(&units);
    (!name.trim().is_empty()).then_some(name)
}

/// The whole disk stored as is from the start of the file.
struct Whole(Option<u64>);

impl Layout for Whole {
    fn next(&mut self, _: &Source) -> io::Result<Option<Piece>> {
        Ok(self.0.take().map(|len| Piece::stored(0, len)))
    }
}

struct Dynamic {
    bat: Table,
    block_size: u64,
    bitmap_size: u64,
    next: u64,
    blocks: u64,
    pending: VecDeque<Piece>,
}

impl Layout for Dynamic {
    fn next(&mut self, src: &Source) -> io::Result<Option<Piece>> {
        if let Some(p) = self.pending.pop_front() {
            return Ok(Some(p));
        }
        if self.next >= self.blocks {
            return Ok(None);
        }
        let i = self.next;
        self.next += 1;
        let entry = self.bat.get(src, i)?.map_or(UNALLOCATED, |b| be32(b, 0));
        if entry == UNALLOCATED {
            return Ok(Some(Piece::zeros(self.block_size)));
        }
        let start = entry as u64 * 512;
        let data = start + self.bitmap_size;
        let bitmap = src.read_vec(start, self.bitmap_size as usize)?;
        // Sectors whose bit is clear were never written: zeros. (Bits go from the most
        // significant one of each byte.)
        let sectors = (self.block_size / 512) as usize;
        let full = sectors / 8;
        if bitmap[..full].iter().all(|&b| b == 0xff) && sectors.is_multiple_of(8) {
            return Ok(Some(Piece::stored(data, self.block_size)));
        }
        let bit = |s: usize| bitmap[s / 8] & (0x80 >> (s % 8)) != 0;
        let mut s = 0;
        while s < sectors {
            let set = bit(s);
            let run = (s..sectors).take_while(|&t| bit(t) == set).count();
            let len = run as u64 * 512;
            self.pending.push_back(if set {
                Piece::stored(data + s as u64 * 512, len)
            } else {
                Piece::zeros(len)
            });
            s += run;
        }
        Ok(self.pending.pop_front())
    }
}
