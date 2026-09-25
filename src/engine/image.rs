//! DD-GUI smart images: a gzip file of a whole drive, where free space is stored as
//! (very cheap) zeros and the map of used space sits in the first member's header.
//!
//! Version 1 is a multi-member gzip file (RFC 1952), so plain `gunzip` gives the raw
//! image. DD-GUI reads more out of it:
//!
//! * Every member has an FEXTRA subfield `DG` of 8 bytes: the member's size in the
//!   file, then its uncompressed size, both u32 little-endian. With the map, a restore
//!   skips members of free space without decompressing them.
//! * The first member is empty. Its FCOMMENT holds the map, in ASCII:
//!
//!   ```text
//!   DD-GUI smart image v1
//!   size=<drive bytes> used=<bytes in extents> extents=<count> unit=<4096, or 1>
//!   map=<base64 without padding of LEB128 pairs: gap since the previous extent's end, length>
//!   ```
//!
//!   The pairs count units. An extent that ends at the end of the drive may end there
//!   inside its last unit. Keys a reader doesn't know are ignored; another first line
//!   (a later version) makes DD-GUI treat the file as plain gzip.
//!
//! * Then the drive from start to end, in members that are either all used data (up to
//!   4 MiB each, deflate level 1) or all free space (zeros, 64 MiB or a smaller power
//!   of two per member, compressed once and repeated).

use crate::smart::Extent;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

pub const MAGIC: &str = "DD-GUI smart image v1";
/// The biggest member of zeros.
pub const ZEROS_MAX: u64 = 64 << 20;
/// Bytes in a member header with just the `DG` subfield.
const HEADER_LEN: usize = 24;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const FHCRC: u8 = 0x02;

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

    pub fn to_comment(&self) -> String {
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
            "{MAGIC}\nsize={} used={} extents={} unit={unit}\nmap={}\n",
            self.size,
            self.used(),
            self.extents.len(),
            base64(&bytes)
        )
    }

    /// The map in a first member's comment. None when it isn't a (version 1) smart image.
    pub fn parse(comment: &[u8]) -> Option<Result<Map, String>> {
        let body = comment
            .strip_prefix(MAGIC.as_bytes())?
            .strip_prefix(b"\n")?;
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
                let csize = u32::from_le_bytes(data[..4].try_into().unwrap());
                let usize = u32::from_le_bytes(data[4..].try_into().unwrap());
                header.sizes = Some((csize as u64, usize as u64));
            }
            rest = &rest[4 + len..];
        }
    }
    // Limits keep a hostile file from making probe() eat memory. 64 MiB of map is
    // millions of extents.
    for (flag, limit) in [(FNAME, 1 << 16), (FCOMMENT, 64 << 20)] {
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

fn header(csize: u64, usize: u64, mtime: u32, comment: Option<&[u8]>) -> Vec<u8> {
    let flags = FEXTRA | if comment.is_some() { FCOMMENT } else { 0 };
    let mut h = Vec::with_capacity(HEADER_LEN + comment.map_or(0, |c| c.len() + 1));
    h.extend([0x1f, 0x8b, 8, flags]);
    h.extend(mtime.to_le_bytes());
    h.extend([0, 255]); // XFL, OS: unknown
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

/// Writes one member around raw deflate data; returns its size.
pub fn write_member(out: &mut impl Write, deflated: &[u8], crc: u32, len: u64) -> io::Result<u64> {
    let csize = (HEADER_LEN + deflated.len() + 8) as u64;
    out.write_all(&header(csize, len, 0, None))?;
    out.write_all(deflated)?;
    out.write_all(&crc.to_le_bytes())?;
    out.write_all(&(len as u32).to_le_bytes())?;
    Ok(csize)
}

/// The first member: no data, the map in its comment.
pub fn map_member(map: &Map, mtime: u32) -> Vec<u8> {
    let comment = map.to_comment();
    // An empty deflate stream: one final fixed-Huffman block holding only end-of-block.
    let empty = [0x03, 0x00];
    let csize = (HEADER_LEN + comment.len() + 1 + empty.len() + 8) as u64;
    let mut member = header(csize, 0, mtime, Some(comment.as_bytes()));
    member.extend(empty);
    member.extend([0u8; 8]); // CRC and length of nothing
    member
}

/// A data member: deflate level 1, which also stores incompressible blocks as they are.
pub fn data_member(data: &[u8]) -> (Vec<u8>, u32) {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    (miniz_oxide::deflate::compress_to_vec(data, 1), crc.sum())
}

/// Members of zeros, compressed once and then repeated.
#[derive(Default)]
pub struct Zeros {
    members: HashMap<u64, Vec<u8>>,
}

impl Zeros {
    /// Writes `len` bytes of zeros; returns how many bytes that took.
    pub fn write(&mut self, out: &mut impl Write, mut len: u64) -> io::Result<u64> {
        let mut written = 0;
        while len > 0 {
            // 64 MiB at a time, then powers of two down to 4 KiB (gaps are 4 KiB-aligned),
            // then whatever is left at the very end of a drive.
            let piece = match len {
                ZEROS_MAX.. => ZEROS_MAX,
                4096.. => 1 << len.ilog2(),
                _ => len,
            };
            let member = self
                .members
                .entry(piece)
                .or_insert_with(|| zeros_member(piece));
            out.write_all(member)?;
            written += member.len() as u64;
            len -= piece;
        }
        Ok(written)
    }
}

fn zeros_member(len: u64) -> Vec<u8> {
    // zlib-rs handles long runs of zeros many times faster than miniz_oxide.
    let zeros = vec![0u8; len.min(1 << 20) as usize];
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    let mut crc = flate2::Crc::new();
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        encoder.write_all(&zeros[..n]).expect("writing to memory");
        crc.update(&zeros[..n]);
        left -= n as u64;
    }
    let deflated = encoder.finish().expect("writing to memory");
    let mut member = Vec::with_capacity(HEADER_LEN + deflated.len() + 8);
    write_member(&mut member, &deflated, crc.sum(), len).expect("writing to memory");
    member
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
