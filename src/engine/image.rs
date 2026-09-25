//! DD-GUI smart images: a whole drive in one compressed file, where free space costs next
//! to nothing and a map of the used space comes first, so DD-GUI can restore just the
//! used parts. Any zstd (or, for version 1, gzip) decoder still gives the full raw image:
//! the used data in place, zeros for free space, exactly the drive's size.
//!
//! # Version 2: zstd (`.img.zst`), what DD-GUI writes
//!
//! A zstd file (RFC 8878) in zstd's seekable format [^seekable]: independent frames, then
//! a seek table, so DD-GUI jumps over free space without decompressing it (and tools that
//! know the format can read any part of the image). Integers are little-endian.
//!
//! 1. The map, in a skippable frame (which every zstd decoder skips): `u32 0x184D2A5D`,
//!    `u32 N`, then N bytes of ASCII:
//!
//!    ```text
//!    DD-GUI smart image v2
//!    size=<drive bytes> used=<bytes in extents> extents=<count> unit=<4096, or 1>
//!    map=<base64 without padding of LEB128 pairs: gap since the previous extent's end, length>
//!    ```
//!
//!    The pairs count units. An extent that ends at the end of the drive may end there
//!    inside its last unit. Keys a reader doesn't know are ignored; another first line
//!    (a later version) makes DD-GUI treat the file as plain zstd.
//!
//! 2. The drive from start to end, in zstd frames that hold either only used data (up to
//!    4 MiB of it, compressed at level 3, several frames at once on as many threads; see
//!    `compressor`) or only free space: zeros, 64 MiB per frame, then
//!    powers of two down to 4 KiB, then what's left at the very end of the drive. Each size
//!    of zeros is compressed once and repeated (64 MiB take about 2 KiB). Every frame
//!    records its size and a checksum of its content, and none holds more than 64 MiB.
//!
//! 3. The seek table, in a skippable frame: `u32 0x184D2A5E`, `u32 8 × F + 9`; for each of
//!    the F frames before it, the map's included, `u32` bytes in the file and `u32` bytes
//!    decompressed (0 for the map); then the footer: `u32 F`, `u8 0` (no checksums in the
//!    table), `u32 0x8F92EAB1`.
//!
//! [^seekable]: <https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md>
//!
//! # Version 1: gzip (`.img.gz`), still restored but no longer written
//!
//! A multi-member gzip file (RFC 1952), so plain `gunzip` gives the raw image:
//!
//! * Every member has an FEXTRA subfield `DG` of 8 bytes: the member's size in the file,
//!   then its uncompressed size, both u32. With the map, a restore skips members of free
//!   space without decompressing them.
//! * The first member is empty; its FCOMMENT holds the map as above, under the first line
//!   `DD-GUI smart image v1`.
//! * Then the drive from start to end, in members that are either all used data (up to
//!   4 MiB each, deflate level 1) or all free space (zeros, 64 MiB or a smaller power of
//!   two per member, compressed once and repeated).

use crate::smart::Extent;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};

/// The first line of a version 2 map.
pub const MAGIC_V2: &str = "DD-GUI smart image v2";
/// The first line of a version 1 map (in the first gzip member's comment).
pub const MAGIC_V1: &str = "DD-GUI smart image v1";
/// The skippable frame that holds the map.
const MAP_FRAME: u32 = 0x184D_2A5D;
/// The skippable frame that holds a seek table (the seekable format's).
const SEEK_TABLE_FRAME: u32 = 0x184D_2A5E;
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
const FOOTER_LEN: u64 = 9;
/// The seekable format's limit on frames.
const MAX_FRAMES: u64 = 0x800_0000;
/// The compression level for used data: fast, and a good deal smaller than gzip -1.
pub const LEVEL: i32 = 3;
/// The most used data in one frame.
pub const DATA_MAX: usize = 4 << 20;
/// The biggest frame of zeros (and member, in version 1). No frame holds more.
pub const ZEROS_MAX: u64 = 64 << 20;
/// A map bigger than this is damage, not data: 64 MiB of map is millions of extents.
const MAP_MAX: u64 = 64 << 20;

/// Which parts of a drive a smart image holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    pub size: u64,
    /// Sorted, not overlapping, within `size`.
    pub extents: Vec<Extent>,
}

impl Map {
    pub fn used(&self) -> u64 {
        self.extents.iter().map(|e| e.len).sum()
    }

    /// Where the data ends: a drive has to be at least this big to take it.
    pub fn end(&self) -> u64 {
        self.extents.last().map_or(0, |e| e.start + e.len)
    }

    /// The map as text, under a first line of `magic`.
    pub fn to_text(&self, magic: &str) -> String {
        // Counting in 4 KiB units makes for small numbers. The analysis aligns extents to
        // that, except where the last one stops at the end of the drive.
        let aligned = |e: &Extent| {
            e.start.is_multiple_of(4096)
                && (e.len.is_multiple_of(4096) || e.start + e.len == self.size)
        };
        let unit = if self.extents.iter().all(aligned) {
            4096
        } else {
            1
        };
        let mut bytes = Vec::with_capacity(self.extents.len() * 3);
        let mut pos = 0;
        for e in &self.extents {
            put_varint(&mut bytes, (e.start - pos) / unit);
            put_varint(&mut bytes, e.len.div_ceil(unit));
            pos = e.start + e.len;
        }
        format!(
            "{magic}\nsize={} used={} extents={} unit={unit}\nmap={}\n",
            self.size,
            self.used(),
            self.extents.len(),
            base64(&bytes)
        )
    }

    /// The map in `text`, whose first line has to be `magic`. None when it isn't.
    pub fn parse(text: &[u8], magic: &str) -> Option<Result<Map, String>> {
        let body = text.strip_prefix(magic.as_bytes())?.strip_prefix(b"\n")?;
        Some(
            parse_body(body)
                .ok_or_else(|| "the map of used space in this smart image is damaged".to_owned()),
        )
    }
}

fn parse_body(body: &[u8]) -> Option<Map> {
    let mut lines = std::str::from_utf8(body).ok()?.lines();
    let (mut size, mut used, mut count, mut unit) = (None, None, None, None);
    for word in lines.next()?.split_whitespace() {
        match word.split_once('=')? {
            ("size", v) => size = v.parse::<u64>().ok(),
            ("used", v) => used = v.parse::<u64>().ok(),
            ("extents", v) => count = v.parse::<usize>().ok(),
            ("unit", v) => unit = v.parse::<u64>().ok().filter(|&u| u > 0),
            _ => {}
        }
    }
    let (size, used, count, unit) = (size?, used?, count?, unit?);
    let bytes = unbase64(lines.next()?.strip_prefix("map=")?.as_bytes())?;
    let mut extents = Vec::with_capacity(count.min(bytes.len() / 2));
    let (mut at, mut pos) = (0, 0u64);
    while at < bytes.len() {
        let start = pos.checked_add(get_varint(&bytes, &mut at)?.checked_mul(unit)?)?;
        let end = start.checked_add(get_varint(&bytes, &mut at)?.checked_mul(unit)?)?;
        // Only the end of the drive cuts a unit short.
        let end = if end > size && end - size < unit {
            size
        } else {
            end
        };
        if end <= start || end > size {
            return None;
        }
        extents.push(Extent {
            start,
            len: end - start,
        });
        pos = end;
    }
    let map = Map { size, extents };
    (map.extents.len() == count && map.used() == used).then_some(map)
}

// ---- Version 2 ------------------------------------------------------------------------

/// The skippable frame with the map, which starts a version 2 image.
pub fn map_frame(map: &Map) -> Vec<u8> {
    let text = map.to_text(MAGIC_V2);
    let mut frame = Vec::with_capacity(8 + text.len());
    frame.extend(MAP_FRAME.to_le_bytes());
    frame.extend((text.len() as u32).to_le_bytes());
    frame.extend(text.as_bytes());
    frame
}

/// What starts a version 2 smart image.
pub struct Head {
    /// The map, or why it's unreadable.
    pub map: Result<Map, String>,
    /// Bytes the map's frame takes in the file.
    pub len: u64,
}

/// Reads the map frame at the start of `r`. None when there's none: then it isn't a
/// version 2 smart image (maybe plain zstd, or a later version).
pub fn read_head(r: &mut impl Read) -> io::Result<Option<Head>> {
    let mut header = [0u8; 8];
    if read_full(r, &mut header)? < 8 || le32(&header[..4]) != MAP_FRAME {
        return Ok(None);
    }
    let len = le32(&header[4..]) as u64;
    if len > MAP_MAX {
        return Ok(None);
    }
    let mut text = Vec::with_capacity(len as usize);
    r.take(len).read_to_end(&mut text)?;
    let map = match Map::parse(&text, MAGIC_V2) {
        None => return Ok(None),
        Some(_) if (text.len() as u64) < len => Err("the smart image is cut short".to_owned()),
        Some(map) => map,
    };
    Ok(Some(Head { map, len: 8 + len }))
}

/// A frame as the seek table lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Bytes in the file.
    pub packed: u32,
    /// Bytes once decompressed.
    pub len: u32,
}

/// The seek table (a skippable frame) that ends a version 2 image and lists `frames`.
pub fn seek_table(frames: &[Frame]) -> Vec<u8> {
    let body = 8 * frames.len() + FOOTER_LEN as usize;
    let mut table = Vec::with_capacity(8 + body);
    table.extend(SEEK_TABLE_FRAME.to_le_bytes());
    table.extend((body as u32).to_le_bytes());
    for f in frames {
        table.extend(f.packed.to_le_bytes());
        table.extend(f.len.to_le_bytes());
    }
    table.extend((frames.len() as u32).to_le_bytes());
    table.push(0);
    table.extend(SEEKABLE_MAGIC.to_le_bytes());
    table
}

/// Reads the seek table at the end of a file of `size` bytes: the frames it lists, and
/// where it starts (where the frames end). An error (InvalidData) when there's none.
pub fn read_seek_table(f: &mut (impl Read + Seek), size: u64) -> io::Result<(Vec<Frame>, u64)> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if size < 8 + FOOTER_LEN {
        return Err(bad("no seek table"));
    }
    let mut footer = [0u8; FOOTER_LEN as usize];
    f.seek(SeekFrom::Start(size - FOOTER_LEN))?;
    f.read_exact(&mut footer)?;
    if le32(&footer[5..]) != SEEKABLE_MAGIC {
        return Err(bad("no seek table"));
    }
    let descriptor = footer[4];
    if descriptor & 0x7c != 0 {
        return Err(bad("a seek table of an unknown kind"));
    }
    let entry = if descriptor & 0x80 != 0 { 12 } else { 8 };
    let count = le32(&footer[..4]) as u64;
    let body = count * entry + FOOTER_LEN;
    if count > MAX_FRAMES || 8 + body > size {
        return Err(bad("a damaged seek table"));
    }
    let start = size - 8 - body;
    let mut table = vec![0u8; (8 + body - FOOTER_LEN) as usize];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut table)?;
    if le32(&table[..4]) != SEEK_TABLE_FRAME || le32(&table[4..8]) as u64 != body {
        return Err(bad("a damaged seek table"));
    }
    let frames = table[8..]
        .chunks_exact(entry as usize)
        .map(|e| Frame {
            packed: le32(&e[..4]),
            len: le32(&e[4..8]),
        })
        .collect();
    Ok((frames, start))
}

/// A compressor for used data: level 3, with sizes and checksums in the frames.
///
/// Its two hash tables are a size smaller than level 3's own (256 + 128 KiB instead of
/// 512 + 256 KiB), so that they stay in a core's L2 cache while several threads compress
/// at once. On an 8-core laptop that made 8 threads 2.7 times as fast, for 1% more bytes.
/// Decoders don't see a difference (the window stays 2 MiB).
pub fn compressor() -> io::Result<zstd::bulk::Compressor<'static>> {
    use zstd::zstd_safe::CParameter;
    let mut c = zstd::bulk::Compressor::new(LEVEL)?;
    c.set_parameter(CParameter::ChecksumFlag(true))?;
    c.set_parameter(CParameter::ContentSizeFlag(true))?;
    c.set_parameter(CParameter::HashLog(16))?;
    c.set_parameter(CParameter::ChainLog(15))?;
    Ok(c)
}

/// Compresses `data` into one frame, in `out` (whose old content goes).
pub fn compress(c: &mut zstd::bulk::Compressor, data: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    out.reserve(zstd::zstd_safe::compress_bound(data.len()));
    c.compress_to_buffer(data, out).map(|_| ())
}

/// Frames of zeros: each size compressed once, then repeated.
#[derive(Default)]
pub struct Zeros {
    frames: HashMap<u64, Vec<u8>>,
}

impl Zeros {
    /// How `len` bytes of free space get cut into frames: 64 MiB at a time, then powers of
    /// two down to 4 KiB (gaps are 4 KiB-aligned), then whatever is left at the very end of
    /// a drive. That keeps the number of different frames small.
    pub fn pieces(mut len: u64) -> impl Iterator<Item = u64> {
        std::iter::from_fn(move || {
            let piece = match len {
                0 => return None,
                ZEROS_MAX.. => ZEROS_MAX,
                4096.. => 1 << len.ilog2(),
                _ => len,
            };
            len -= piece;
            Some(piece)
        })
    }

    /// The frame for `len` bytes of zeros (`len` being one of the `pieces`).
    pub fn frame(&mut self, len: u64) -> io::Result<&[u8]> {
        if !self.frames.contains_key(&len) {
            let frame = zeros_frame(len)?;
            self.frames.insert(len, frame);
        }
        Ok(&self.frames[&len])
    }
}

fn zeros_frame(len: u64) -> io::Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), LEVEL)?;
    encoder.include_checksum(true)?;
    encoder.set_pledged_src_size(Some(len))?;
    let zeros = vec![0u8; len.min(1 << 20) as usize];
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        encoder.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    encoder.finish()
}

// ---- Version 1 ------------------------------------------------------------------------

const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const FHCRC: u8 = 0x02;

/// A gzip member header, as far as DD-GUI cares.
#[derive(Debug, Default)]
pub struct Header {
    /// Bytes the header takes.
    pub len: u64,
    /// From the `DG` subfield: (member size in the file, uncompressed size).
    pub sizes: Option<(u64, u64)>,
    pub comment: Option<Vec<u8>>,
}

/// Reads one member header (RFC 1952 section 2.3).
pub fn read_header(r: &mut impl BufRead) -> io::Result<Header> {
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid gzip data ({what})"),
        )
    };
    let mut fixed = [0u8; 10];
    r.read_exact(&mut fixed)?;
    if fixed[..3] != [0x1f, 0x8b, 8] {
        return Err(bad("no member header where one should start"));
    }
    let flags = fixed[3];
    if flags & 0xe0 != 0 {
        return Err(bad("reserved header flags"));
    }
    let mut header = Header {
        len: 10,
        ..Header::default()
    };
    if flags & FEXTRA != 0 {
        let mut xlen = [0u8; 2];
        r.read_exact(&mut xlen)?;
        let mut extra = vec![0u8; u16::from_le_bytes(xlen) as usize];
        r.read_exact(&mut extra)?;
        header.len += 2 + extra.len() as u64;
        let mut rest = &extra[..];
        while rest.len() >= 4 {
            let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
            let Some(data) = rest.get(4..4 + len) else {
                break;
            };
            if rest[..2] == *b"DG" && len == 8 {
                header.sizes = Some((le32(&data[..4]) as u64, le32(&data[4..]) as u64));
            }
            rest = &rest[4 + len..];
        }
    }
    // Limits keep a hostile file from making probe() eat memory.
    for (flag, limit) in [(FNAME, 1 << 16), (FCOMMENT, MAP_MAX)] {
        if flags & flag != 0 {
            let mut text = Vec::new();
            r.by_ref().take(limit).read_until(0, &mut text)?;
            if text.pop() != Some(0) {
                return Err(bad("unterminated name or comment"));
            }
            header.len += text.len() as u64 + 1;
            if flag == FCOMMENT {
                header.comment = Some(text);
            }
        }
    }
    if flags & FHCRC != 0 {
        r.read_exact(&mut [0u8; 2])?;
        header.len += 2;
    }
    Ok(header)
}

/// Writes version 1 images, which DD-GUI no longer does: for testing that they restore.
#[cfg(all(test, unix))]
pub mod v1 {
    use super::*;

    /// Bytes in a member header with just the `DG` subfield.
    const HEADER_LEN: usize = 24;

    fn header(csize: u64, usize: u64, comment: Option<&[u8]>) -> Vec<u8> {
        let flags = FEXTRA | if comment.is_some() { FCOMMENT } else { 0 };
        let mut h = Vec::new();
        h.extend([0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 255]);
        h.extend(12u16.to_le_bytes());
        h.extend(*b"DG");
        h.extend(8u16.to_le_bytes());
        h.extend(u32::try_from(csize).unwrap_or(u32::MAX).to_le_bytes());
        h.extend(u32::try_from(usize).unwrap_or(u32::MAX).to_le_bytes());
        if let Some(comment) = comment {
            h.extend(comment);
            h.push(0);
        }
        h
    }

    /// One member around raw deflate data.
    fn member(out: &mut Vec<u8>, deflated: &[u8], crc: u32, len: u64) {
        let csize = (HEADER_LEN + deflated.len() + 8) as u64;
        out.extend(header(csize, len, None));
        out.extend(deflated);
        out.extend(crc.to_le_bytes());
        out.extend((len as u32).to_le_bytes());
    }

    fn deflate(data: &[u8], level: u32) -> (Vec<u8>, u32) {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        encoder.write_all(data).expect("writing to memory");
        let mut crc = flate2::Crc::new();
        crc.update(data);
        (encoder.finish().expect("writing to memory"), crc.sum())
    }

    fn data_member(out: &mut Vec<u8>, data: &[u8]) {
        let (deflated, crc) = deflate(data, 1);
        member(out, &deflated, crc, data.len() as u64);
    }

    /// Members of `len` bytes of zeros, the way version 1 cut them.
    pub fn zeros(out: &mut Vec<u8>, len: u64) {
        for piece in Zeros::pieces(len) {
            let (deflated, crc) = deflate(&vec![0u8; piece as usize], 6);
            member(out, &deflated, crc, piece);
        }
    }

    /// A version 1 smart image of `drive` (its bytes), holding the extents of `map`.
    pub fn image(drive: &[u8], map: &Map) -> Vec<u8> {
        let comment = map.to_text(MAGIC_V1);
        let mut out = Vec::new();
        let empty = [0x03, 0x00]; // a final fixed-Huffman block with only end-of-block
        let csize = (HEADER_LEN + comment.len() + 1 + empty.len() + 8) as u64;
        out.extend(header(csize, 0, Some(comment.as_bytes())));
        out.extend(empty);
        out.extend([0u8; 8]);
        let mut pos = 0;
        for e in &map.extents {
            zeros(&mut out, e.start - pos);
            let data = &drive[e.start as usize..(e.start + e.len) as usize];
            for piece in data.chunks(DATA_MAX) {
                data_member(&mut out, piece);
            }
            pos = e.start + e.len;
        }
        zeros(&mut out, map.size - pos);
        out
    }
}

// ---- Shared ---------------------------------------------------------------------------

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().expect("4 bytes"))
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *data.get(*at)?;
        *at += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn unbase64(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in text {
        let v = BASE64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6 | v) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Walks the output of a restore through the map.
pub struct Cursor<'a> {
    extents: &'a [Extent],
    next: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(map: &'a Map) -> Self {
        Self {
            extents: &map.extents,
            next: 0,
        }
    }

    /// The run starting at `pos`, at most `len` long: (holds data, length).
    pub fn run(&mut self, pos: u64, len: u64) -> (bool, u64) {
        while self
            .extents
            .get(self.next)
            .is_some_and(|e| e.start + e.len <= pos)
        {
            self.next += 1;
        }
        match self.extents.get(self.next) {
            Some(e) if e.start <= pos => (true, len.min(e.start + e.len - pos)),
            Some(e) => (false, len.min(e.start - pos)),
            None => (false, len),
        }
    }

    /// Is all of `pos..pos + len` free space?
    pub fn is_free(&mut self, pos: u64, len: u64) -> bool {
        self.run(pos, len) == (false, len)
    }
}
