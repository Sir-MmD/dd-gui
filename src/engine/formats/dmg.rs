//! Apple disk images (UDIF, .dmg). A 512-byte "koly" trailer at the end of the file
//! points to a property list (XML) whose "resource-fork" → "blkx" entries each describe
//! a range of the disk ("mish" blocks). Their chunks are zeros, raw data, or compressed:
//! ADC (UDCO), zlib (UDZO), bzip2 (UDBZ), LZFSE (ULFO) or LZMA (ULMO, an xz stream per
//! chunk). The disk is assembled by sector; each blkx's CRC-32 is checked as it goes by.
//! Old images without the XML keep the same blocks in a resource fork, which works too.
//!
//! Encrypted images, sparse images and sparse bundles (.sparseimage, .sparsebundle) and
//! segmented images (.dmgpart) aren't supported.

use super::codec::Codec;
use super::source::{Source, bad, be16, be32, be64, size_text, unsupported};
use super::vdisk::{Layout, MAX_BLOCK, Packed, Piece, Run, VDisk};
use super::{Detected, ImageFormat, Opened};
use std::io;
use std::sync::Arc;

const KOLY: &[u8] = b"koly";
const ENCRYPTED: &[u8] = b"encrcdsa";
const ENCRYPTED_V1: &[u8] = b"cdsaencr";
const SPARSE_IMAGE: &[u8] = b"sprs";
const SPARSE_BUNDLE: &[u8] = b"com.apple.diskimage.sparsebundle";

const ZERO: u32 = 0;
const RAW: u32 = 1;
const IGNORE: u32 = 2;
const ADC: u32 = 0x8000_0004;
const ZLIB: u32 = 0x8000_0005;
const BZIP2: u32 = 0x8000_0006;
const LZFSE: u32 = 0x8000_0007;
const LZMA: u32 = 0x8000_0008;
const COMMENT: u32 = 0x7fff_fffe;
const END: u32 = 0xffff_ffff;

/// The biggest XML property list we read.
const MAX_XML: u64 = 128 << 20;

struct Koly {
    data_fork_offset: u64,
    rsrc_offset: u64,
    rsrc_len: u64,
    segment_count: u32,
    xml_offset: u64,
    xml_len: u64,
    sectors: u64,
}

fn parse_koly(b: &[u8]) -> Option<Koly> {
    (b.len() >= 512 && b.starts_with(KOLY) && be32(b, 4) == 4 && be32(b, 8) == 512).then(|| Koly {
        data_fork_offset: be64(b, 24),
        rsrc_offset: be64(b, 40),
        rsrc_len: be64(b, 48),
        segment_count: be32(b, 60),
        xml_offset: be64(b, 216),
        xml_len: be64(b, 224),
        sectors: be64(b, 492),
    })
}

/// The trailer, at the end (or, in some old images, at the start).
fn koly(src: &Source) -> io::Result<Option<Koly>> {
    if src.size >= 512
        && let Some(k) = parse_koly(&src.read_vec(src.size - 512, 512)?)
    {
        return Ok(Some(k));
    }
    Ok(parse_koly(&src.read_upto(0, 512)?).filter(|k| k.data_fork_offset == 512))
}

/// What kind of Apple image this is, when it's one we can only refuse.
fn refused(src: &Source, head: &[u8]) -> io::Result<Option<&'static str>> {
    if head.starts_with(ENCRYPTED) {
        return Ok(Some("encrypted"));
    }
    if head.starts_with(SPARSE_IMAGE) && (1..=15).contains(&be32(head, 4)) {
        return Ok(Some("sparse image"));
    }
    if head.starts_with(b"<?xml")
        && head
            .windows(SPARSE_BUNDLE.len())
            .any(|w| w == SPARSE_BUNDLE)
    {
        return Ok(Some("sparse bundle"));
    }
    let tail = src.read_vec(src.size.saturating_sub(1024), src.size.min(1024) as usize)?;
    if tail.windows(ENCRYPTED_V1.len()).any(|w| w == ENCRYPTED_V1) {
        return Ok(Some("encrypted"));
    }
    Ok(None)
}

pub fn detect(src: &Source, head: &[u8]) -> io::Result<Option<Detected>> {
    if let Some(k) = koly(src)? {
        return Ok(Some(Detected {
            format: ImageFormat::Dmg,
            raw_size: (k.segment_count <= 1).then(|| k.sectors.saturating_mul(512)),
        }));
    }
    Ok(refused(src, head)?.map(|_| Detected {
        format: ImageFormat::Dmg,
        raw_size: None,
    }))
}

pub fn open(src: Source) -> io::Result<Opened> {
    let head = src.read_upto(0, 4096)?;
    let koly = match koly(&src)? {
        Some(k) => k,
        None => {
            return Err(match refused(&src, &head)? {
                Some("encrypted") => unsupported(
                    "this disk image is encrypted, which DD-GUI doesn't support. Open it in Finder (with its password) and make an unencrypted copy: `hdiutil convert in.dmg -format UDZO -o out.dmg`.",
                ),
                Some(kind) => unsupported(format!(
                    "this is a {kind}, which DD-GUI doesn't support. Convert it to a regular disk image first: `hdiutil convert in -format UDZO -o out.dmg`."
                )),
                None => bad("the DMG is damaged (its trailer is missing)"),
            });
        }
    };
    if koly.segment_count > 1 {
        return Err(unsupported(
            "this DMG is one segment of an image split into several files (.dmgpart), which DD-GUI doesn't support. Join them first: `hdiutil convert first.dmg -format UDZO -o whole.dmg`.",
        ));
    }
    let blocks = if koly.xml_len > 0 {
        blocks_from_xml(&src, &koly)?
    } else if koly.rsrc_len > 0 {
        blocks_from_rsrc(&src, &koly)?
    } else {
        return Err(bad("the DMG is damaged (it doesn't say where its data is)"));
    };
    let size = koly.sectors.saturating_mul(512);
    let layout = assemble(&koly, blocks)?;
    Ok(Opened {
        reader: Box::new(VDisk::new(Arc::new(src), size, Box::new(layout))),
        size: Some(size),
    })
}

/// A blkx entry: a name and its "mish" block.
struct Block {
    name: String,
    mish: Vec<u8>,
}

fn blocks_from_xml(src: &Source, koly: &Koly) -> io::Result<Vec<Block>> {
    if koly.xml_len > MAX_XML {
        return Err(bad(format!(
            "the DMG's table of contents is too big ({})",
            size_text(koly.xml_len)
        )));
    }
    let xml = src.read_vec(koly.xml_offset, koly.xml_len as usize)?;
    let plist = plist::Value::from_reader_xml(&xml[..])
        .map_err(|e| bad(format!("the DMG's table of contents is damaged ({e})")))?;
    let blkx = plist
        .as_dictionary()
        .and_then(|d| d.get("resource-fork"))
        .and_then(|r| r.as_dictionary())
        .and_then(|r| r.get("blkx"))
        .and_then(|b| b.as_array())
        .ok_or_else(|| bad("the DMG's table of contents has no blkx list"))?;
    let mut blocks = Vec::with_capacity(blkx.len());
    for entry in blkx {
        let Some(entry) = entry.as_dictionary() else {
            continue;
        };
        let mish = entry
            .get("Data")
            .and_then(|d| d.as_data())
            .ok_or_else(|| bad("the DMG's table of contents is damaged (a blkx without data)"))?;
        let name = ["Name", "CFName"]
            .iter()
            .find_map(|k| entry.get(k).and_then(|n| n.as_string()))
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("a partition")
            .trim()
            .to_owned();
        blocks.push(Block {
            name,
            mish: mish.to_vec(),
        });
    }
    Ok(blocks)
}

/// The blkx resources of a classic resource fork (as 7-Zip reads them).
fn blocks_from_rsrc(src: &Source, koly: &Koly) -> io::Result<Vec<Block>> {
    let damaged = || bad("the DMG's resource fork is damaged");
    if !(0x100..=16 << 20).contains(&koly.rsrc_len) {
        return Err(damaged());
    }
    let r = src.read_vec(koly.rsrc_offset, koly.rsrc_len as usize)?;
    let (data_offset, map_offset, data_len, map_len) = (
        be32(&r, 0) as usize,
        be32(&r, 4) as usize,
        be32(&r, 8) as usize,
        be32(&r, 12) as usize,
    );
    if data_offset != 0x100
        || map_offset != data_offset + data_len
        || map_offset + map_len > r.len()
        || map_len < 0x1e
    {
        return Err(damaged());
    }
    let map = &r[map_offset..map_offset + map_len];
    let types_offset = be16(map, 0x18) as usize;
    let names_offset = be16(map, 0x1a) as usize;
    let types = be16(map, 0x1c) as usize + 1;
    if types_offset != 0x1c || names_offset > map_len || 0x1e + types * 8 > names_offset {
        return Err(damaged());
    }
    let mut blocks = Vec::new();
    for t in 0..types {
        let entry = &map[0x1e + t * 8..0x1e + t * 8 + 8];
        if &entry[..4] != b"blkx" {
            continue;
        }
        let count = be16(entry, 4) as usize + 1;
        let refs = be16(entry, 6) as usize;
        if 0x1c + refs + 12 * count > names_offset {
            return Err(damaged());
        }
        for k in 0..count {
            let reference = &map[0x1c + refs + 12 * k..0x1c + refs + 12 * k + 12];
            let name_at = be16(reference, 2) as usize;
            let offset = (be32(reference, 4) & 0x00ff_ffff) as usize;
            let len_at = data_offset + offset;
            let len = be32(&r, len_at) as usize;
            let mish = r
                .get(len_at + 4..len_at + 4 + len)
                .filter(|_| offset + 4 + len <= data_len)
                .ok_or_else(damaged)?;
            let name = if name_at == 0xffff {
                "a partition".to_owned()
            } else {
                map.get(names_offset + name_at..)
                    .and_then(|n| n.get(1..1 + *n.first()? as usize))
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default()
            };
            blocks.push(Block {
                name,
                mish: mish.to_vec(),
            });
        }
    }
    Ok(blocks)
}

/// A run of the disk from one chunk.
#[derive(Clone, Copy)]
struct Chunk {
    start: u64,
    len: u64,
    kind: u32,
    offset: u64,
    packed_len: u64,
}

/// A blkx's range of the disk, and the CRC-32 of its data when it has one.
struct Checked {
    name: String,
    start: u64,
    end: u64,
    crc: Option<u32>,
}

fn assemble(koly: &Koly, blocks: Vec<Block>) -> io::Result<Chunks> {
    let damaged = |what: &str| bad(format!("the DMG is damaged ({what})"));
    let mut chunks = Vec::new();
    let mut checked = Vec::new();
    for block in blocks {
        let m = &block.mish;
        if m.len() < 204 || !m.starts_with(b"mish") || be32(m, 4) != 1 {
            return Err(damaged("a blkx entry isn't a mish block"));
        }
        let first = be64(m, 8);
        let sectors = be64(m, 16);
        let data_offset = be64(m, 24);
        let count = be32(m, 200) as usize;
        if 204 + count as u64 * 40 > m.len() as u64 {
            return Err(damaged("a mish block is cut short"));
        }
        let start = first
            .checked_mul(512)
            .ok_or_else(|| damaged("sector numbers"))?;
        let end = first
            .checked_add(sectors)
            .and_then(|e| e.checked_mul(512))
            .ok_or_else(|| damaged("sector numbers"))?;
        let crc = (be32(m, 64) == 2 && be32(m, 68) == 32).then(|| be32(m, 72));
        if end > start {
            checked.push(Checked {
                name: block.name.clone(),
                start,
                end,
                crc,
            });
        }
        for c in m[204..204 + count * 40].as_chunks::<40>().0 {
            let kind = be32(c, 0);
            match kind {
                COMMENT => continue,
                END => break,
                ZERO | RAW | IGNORE | ADC | ZLIB | BZIP2 | LZFSE | LZMA => {}
                other => {
                    return Err(unsupported(format!(
                        "this DMG uses a kind of compression DD-GUI doesn't know ({other:#010x})"
                    )));
                }
            }
            let (sector, n) = (be64(c, 8), be64(c, 16));
            let chunk_start = first
                .checked_add(sector)
                .and_then(|s| s.checked_mul(512))
                .ok_or_else(|| damaged("sector numbers"))?;
            let len = n
                .checked_mul(512)
                .ok_or_else(|| damaged("sector numbers"))?;
            if len == 0 {
                continue;
            }
            if chunk_start < start || chunk_start.checked_add(len).is_none_or(|e| e > end) {
                return Err(damaged("a chunk lies outside its partition"));
            }
            let offset = koly
                .data_fork_offset
                .checked_add(data_offset)
                .and_then(|o| o.checked_add(be64(c, 24)))
                .ok_or_else(|| damaged("chunk offsets"))?;
            chunks.push(Chunk {
                start: chunk_start,
                len,
                kind,
                offset,
                packed_len: be64(c, 32),
            });
        }
    }
    chunks.sort_by_key(|c| c.start);
    checked.sort_by_key(|c| c.start);
    for pair in chunks.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(damaged("chunks overlap"));
        }
    }
    for pair in checked.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(damaged("partitions overlap"));
        }
    }
    let ignored = chunks
        .iter()
        .filter(|c| c.kind == IGNORE)
        .map(|c| (c.start, c.start + c.len))
        .collect();
    Ok(Chunks {
        chunks,
        next: 0,
        pos: 0,
        crc: Crcs {
            blocks: checked,
            ignored,
            pos: 0,
            block: 0,
            ignore: 0,
            all: flate2::Crc::new(),
            data: flate2::Crc::new(),
        },
    })
}

struct Chunks {
    chunks: Vec<Chunk>,
    next: usize,
    /// Where the pieces handed out so far end.
    pos: u64,
    crc: Crcs,
}

impl Layout for Chunks {
    fn next(&mut self, _: &Source) -> io::Result<Option<Piece>> {
        let Some(c) = self.chunks.get(self.next).copied() else {
            return Ok(None);
        };
        if c.start > self.pos {
            // Between partitions (or chunks): zeros.
            let gap = c.start - self.pos;
            self.pos = c.start;
            return Ok(Some(Piece::zeros(gap)));
        }
        self.next += 1;
        self.pos = c.start + c.len;
        let codec = match c.kind {
            ZERO | IGNORE => return Ok(Some(Piece::zeros(c.len))),
            RAW => {
                if c.packed_len < c.len {
                    return Err(bad("the DMG is damaged (a raw chunk is too short)"));
                }
                return Ok(Some(Piece::stored(c.offset, c.len)));
            }
            ADC => Codec::Adc,
            ZLIB => Codec::Zlib,
            BZIP2 => Codec::Bzip2,
            LZFSE => Codec::Lzfse,
            _ => Codec::Xz,
        };
        if c.len > MAX_BLOCK as u64 || c.packed_len > MAX_BLOCK as u64 {
            return Err(bad(format!(
                "the DMG has a compressed chunk of {}, more than DD-GUI handles",
                size_text(c.len.max(c.packed_len))
            )));
        }
        Ok(Some(Piece {
            len: c.len,
            run: Run::Packed(Packed {
                offset: c.offset,
                len: c.packed_len as u32,
                codec,
                size: c.len as u32,
                short_ok: false,
            }),
        }))
    }

    fn check(&mut self, data: &[u8]) -> io::Result<()> {
        self.crc.update(data)
    }
}

/// Checks each blkx's CRC-32 as the disk goes by. Like 7-Zip, chunks of type 2
/// ("ignore") are left out of it; the CRC over everything is accepted too.
struct Crcs {
    blocks: Vec<Checked>,
    ignored: Vec<(u64, u64)>,
    pos: u64,
    block: usize,
    ignore: usize,
    all: flate2::Crc,
    data: flate2::Crc,
}

impl Crcs {
    fn update(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let Some(b) = self.blocks.get(self.block) else {
                return Ok(());
            };
            if self.pos < b.start {
                let skip = (b.start - self.pos).min(data.len() as u64) as usize;
                data = &data[skip..];
                self.pos += skip as u64;
                continue;
            }
            while self
                .ignored
                .get(self.ignore)
                .is_some_and(|&(_, end)| end <= self.pos)
            {
                self.ignore += 1;
            }
            let (in_ignored, boundary) = match self.ignored.get(self.ignore) {
                Some(&(start, end)) if start <= self.pos => (true, end),
                Some(&(start, _)) => (false, start),
                None => (false, u64::MAX),
            };
            let n = (b.end.min(boundary) - self.pos).min(data.len() as u64) as usize;
            if b.crc.is_some() {
                self.all.update(&data[..n]);
                if !in_ignored {
                    self.data.update(&data[..n]);
                }
            }
            data = &data[n..];
            self.pos += n as u64;
            if self.pos == b.end {
                if let Some(want) = b.crc
                    && self.data.sum() != want
                    && self.all.sum() != want
                {
                    return Err(bad(format!(
                        "the DMG is damaged: the data of {} doesn't match its checksum",
                        b.name
                    )));
                }
                self.all.reset();
                self.data.reset();
                self.block += 1;
            }
        }
        Ok(())
    }
}
