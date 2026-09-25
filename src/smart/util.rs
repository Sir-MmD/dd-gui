//! Plumbing shared by the partition table and file system readers: reading the drive,
//! little-endian fields, checksums, bitmaps and extent lists.

use super::Extent;
use std::io::{self, Read, Seek, SeekFrom};

pub(crate) const KIB: u64 = 1 << 10;
pub(crate) const MIB: u64 = 1 << 20;
/// Neighbouring extents closer than this get merged: fewer, larger reads for a little more data.
pub(crate) const GAP: u64 = 64 * KIB;
/// `Layout::extents` are aligned to this.
pub(crate) const ALIGN: u64 = 4 * KIB;
/// Past this many extents, larger gaps get merged too (keeps the JSON and the copy plan small).
pub(crate) const MAX_EXTENTS: usize = 65_536;
/// Bitmaps, FATs and descriptor tables are read in pieces of at most this size.
pub(crate) const CHUNK: usize = 4 << 20;
/// No single read buffer gets bigger than this, whatever a corrupt size field says.
const MAX_READ: usize = 64 << 20;
/// Ranges held while collecting, before merging more aggressively.
const MAX_RANGES: usize = 1 << 20;

/// Why a file system can't be trusted. It gets copied in full instead.
#[derive(Debug)]
pub(crate) struct Bad(pub String);

impl From<io::Error> for Bad {
    fn from(err: io::Error) -> Self {
        Bad(format!("read error: {err}"))
    }
}

pub(crate) type Res<T> = Result<T, Bad>;

/// Fails with `why` unless `ok`.
pub(crate) fn ensure(ok: bool, why: &str) -> Res<()> {
    if ok { Ok(()) } else { Err(Bad(why.to_owned())) }
}

/// How far to trust file systems that weren't cleanly unmounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// For copying: a dirty file system, or one with a pending journal, is copied in full.
    Copy,
    /// For a preview of a drive that's still mounted: trust the allocation maps anyway.
    Estimate,
}

/// Knobs for one analysis. Tests turn merging and alignment off to compare exact numbers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opts {
    pub mode: Mode,
    pub gap: u64,
    pub align: u64,
}

impl Opts {
    pub fn new(mode: Mode) -> Self {
        Opts { mode, gap: GAP, align: ALIGN }
    }
}

pub(crate) trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

fn past_end() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of the drive")
}

/// The drive being analyzed. Never reads past `size`.
pub(crate) struct Disk<'a> {
    dev: &'a mut dyn ReadSeek,
    size: u64,
    /// Where the device's cursor is, when known (saves a seek per sequential read).
    cursor: Option<u64>,
    cache: Vec<u8>,
    cache_at: Option<u64>,
}

const CACHE: u64 = 64 * KIB;

impl<'a> Disk<'a> {
    pub fn new(dev: &'a mut dyn ReadSeek, size: u64) -> Self {
        Disk { dev, size, cursor: None, cache: Vec::new(), cache_at: None }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Fills `buf` from byte `pos`.
    pub fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = pos.checked_add(buf.len() as u64).filter(|&end| end <= self.size).ok_or_else(past_end)?;
        if self.cursor != Some(pos) {
            self.cursor = None;
            self.dev.seek(SeekFrom::Start(pos))?;
        }
        self.cursor = None;
        self.dev.read_exact(buf)?;
        self.cursor = Some(end);
        Ok(())
    }

    pub fn read_vec(&mut self, pos: u64, len: usize) -> io::Result<Vec<u8>> {
        if len > MAX_READ {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "read too large"));
        }
        let mut buf = vec![0; len];
        self.read_at(pos, &mut buf)?;
        Ok(buf)
    }

    /// Like `read_at`, through a small cache: for many tiny reads close together (FAT entries).
    pub fn read_cached(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<()> {
        let base = pos - pos % CACHE;
        let end = pos.checked_add(buf.len() as u64).ok_or_else(past_end)?;
        if end > base.saturating_add(CACHE) {
            return self.read_at(pos, buf);
        }
        if self.cache_at != Some(base) {
            self.cache_at = None;
            let mut cache = std::mem::take(&mut self.cache);
            cache.resize(self.size.saturating_sub(base).min(CACHE) as usize, 0);
            let read = self.read_at(base, &mut cache);
            self.cache = cache;
            read?;
            self.cache_at = Some(base);
        }
        let from = (pos - base) as usize;
        let src = self.cache.get(from..from + buf.len()).ok_or_else(past_end)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A partition: offsets are relative to its start, and reads stay inside it.
pub(crate) struct Part<'p, 'a> {
    disk: &'p mut Disk<'a>,
    pub start: u64,
    pub size: u64,
    pub opts: Opts,
}

impl<'p, 'a> Part<'p, 'a> {
    pub fn new(disk: &'p mut Disk<'a>, start: u64, size: u64, opts: Opts) -> Self {
        Part { disk, start, size, opts }
    }

    fn abs(&self, pos: u64, len: usize) -> io::Result<u64> {
        pos.checked_add(len as u64).filter(|&end| end <= self.size).ok_or_else(past_end)?;
        self.start.checked_add(pos).ok_or_else(past_end)
    }

    pub fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<()> {
        let abs = self.abs(pos, buf.len())?;
        self.disk.read_at(abs, buf)
    }

    pub fn read_vec(&mut self, pos: u64, len: usize) -> io::Result<Vec<u8>> {
        let abs = self.abs(pos, len)?;
        self.disk.read_vec(abs, len)
    }

    pub fn read_cached(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<()> {
        let abs = self.abs(pos, buf.len())?;
        self.disk.read_cached(abs, buf)
    }

    /// An empty range list that merges like this analysis wants.
    pub fn ranges(&self) -> Ranges {
        Ranges::new(self.opts.gap)
    }
}

/// What a file system uses.
pub(crate) struct Usage {
    /// Byte ranges, relative to the start of the partition.
    pub used: Ranges,
    /// Where the part of the partition that the file system's allocation map covers ends.
    /// Everything from here to the end of the partition is copied as is.
    pub end: u64,
}

// Little-endian fields. Out of bounds reads give zeros; callers validate what they read.

fn array<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    at.checked_add(N).and_then(|end| b.get(at..end)).and_then(|s| s.try_into().ok()).unwrap_or([0; N])
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(array(b, at))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(array(b, at))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(array(b, at))
}

pub(crate) fn be16_at(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(array(b, at))
}

pub(crate) fn be32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(array(b, at))
}

/// True when `b` holds `magic` at `at`.
pub(crate) fn at(b: &[u8], at: usize, magic: &[u8]) -> bool {
    at.checked_add(magic.len()).and_then(|end| b.get(at..end)) == Some(magic)
}

/// A label stored as UTF-16LE, cut at the first NUL.
pub(crate) fn utf16_label(b: &[u8]) -> Option<String> {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).take_while(|&u| u != 0).collect();
    tidy_label(String::from_utf16_lossy(&units))
}

/// A label in a single-byte code page (FAT). Non-ASCII bytes are read as Latin-1.
pub(crate) fn byte_label(b: &[u8]) -> Option<String> {
    tidy_label(b.iter().take_while(|&&c| c != 0).map(|&c| char::from(c)).collect())
}

fn tidy_label(s: String) -> Option<String> {
    let s = s.trim_end_matches([' ', '\0']).trim_start();
    (!s.is_empty()).then(|| s.to_owned())
}

// Checksums.

const fn table32(poly: u32) -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const fn table16(poly: u16) -> [u16; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32: [u32; 256] = table32(0xEDB8_8320);
static CRC32C: [u32; 256] = table32(0x82F6_3B78);
static CRC16: [u16; 256] = table16(0xA001);

/// CRC-32 (IEEE), as GPT uses it.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| (c >> 8) ^ CRC32[((c ^ u32::from(b)) & 0xFF) as usize])
}

/// Raw CRC-32C update (no inversions), the way ext4 chains it.
pub(crate) fn crc32c(crc: u32, data: &[u8]) -> u32 {
    data.iter().fold(crc, |c, &b| (c >> 8) ^ CRC32C[((c ^ u32::from(b)) & 0xFF) as usize])
}

/// Raw CRC-16 (ARC polynomial) update, as ext4's uninit_bg group descriptors use it.
pub(crate) fn crc16(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |c, &b| (c >> 8) ^ CRC16[((c ^ u16::from(b)) & 0xFF) as usize])
}

/// Calls `f(first, count)` for each run of set bits among the first `bits` bits of `map`
/// (bit 0 is the lowest bit of byte 0), numbering bits from `base`.
pub(crate) fn bit_runs(map: &[u8], bits: u64, base: u64, mut f: impl FnMut(u64, u64)) {
    let bits = bits.min(map.len() as u64 * 8);
    let mut run: Option<u64> = None;
    let mut word_at = 0u64;
    for chunk in map.chunks(8) {
        if word_at >= bits {
            break;
        }
        let mut bytes = [0u8; 8];
        bytes[..chunk.len()].copy_from_slice(chunk);
        let mut word = u64::from_le_bytes(bytes);
        let valid = bits - word_at;
        if valid < 64 {
            word &= (1u64 << valid) - 1;
        }
        match word {
            0 => {
                if let Some(start) = run.take() {
                    f(base + start, word_at - start);
                }
            }
            u64::MAX => {
                run.get_or_insert(word_at);
            }
            _ => {
                let mut pos = 0u32;
                while pos < 64 {
                    match run {
                        None => {
                            let rest = word >> pos;
                            if rest == 0 {
                                break;
                            }
                            pos += rest.trailing_zeros();
                            run = Some(word_at + u64::from(pos));
                        }
                        Some(start) => {
                            let rest = !word >> pos;
                            if rest == 0 {
                                break;
                            }
                            pos += rest.trailing_zeros();
                            f(base + start, word_at + u64::from(pos) - start);
                            run = None;
                        }
                    }
                }
            }
        }
        word_at += 64;
    }
    if let Some(start) = run {
        f(base + start, bits - start);
    }
}

/// Byte ranges being collected. Neighbours closer than the gap merge as they come in
/// (in order, as from a bitmap), so fragmented file systems stay cheap to describe.
pub(crate) struct Ranges {
    v: Vec<Extent>,
    gap: u64,
}

impl Ranges {
    pub fn new(gap: u64) -> Self {
        Ranges { v: Vec::new(), gap }
    }

    pub fn add(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        if let Some(last) = self.v.last_mut() {
            let last_end = last.start + last.len;
            if start >= last.start && (start <= last_end || start - last_end < self.gap) {
                last.len = end.max(last_end) - last.start;
                return;
            }
        }
        self.v.push(Extent { start, len: end - start });
        if self.v.len() >= MAX_RANGES {
            // A pathologically fragmented file system: trade a little precision for memory.
            while self.v.len() >= MAX_RANGES / 2 {
                self.gap = self.gap.max(ALIGN).saturating_mul(2);
                self.v = merge(std::mem::take(&mut self.v), self.gap);
            }
        }
    }

    pub fn into_vec(self) -> Vec<Extent> {
        self.v
    }
}

/// Sorts and merges extents that overlap, touch, or are less than `gap` apart.
/// Every extent must satisfy `start + len <= u64::MAX`.
fn merge(mut v: Vec<Extent>, gap: u64) -> Vec<Extent> {
    v.sort_unstable_by_key(|e| e.start);
    let mut out: Vec<Extent> = Vec::with_capacity(v.len());
    for e in v.into_iter().filter(|e| e.len > 0) {
        if let Some(last) = out.last_mut() {
            let last_end = last.start + last.len;
            if e.start <= last_end || e.start - last_end < gap {
                last.len = (e.start + e.len).max(last_end) - last.start;
                continue;
            }
        }
        out.push(e);
    }
    out
}

/// Aligns (start down, end up), clamps to `size`, sorts and merges: what `Layout::extents`
/// promises. Past `MAX_EXTENTS`, the merge gap doubles until they fit.
pub(crate) fn normalize(v: Vec<Extent>, size: u64, gap: u64, align: u64) -> Vec<Extent> {
    let align = align.max(1);
    let aligned = v
        .into_iter()
        .filter_map(|e| {
            let start = e.start - e.start % align;
            let end = e.start.saturating_add(e.len).min(size);
            let end = end.checked_next_multiple_of(align).unwrap_or(u64::MAX).min(size);
            (start < end).then(|| Extent { start, len: end - start })
        })
        .collect();
    let mut gap = gap;
    let mut out = merge(aligned, gap);
    while out.len() > MAX_EXTENTS {
        gap = gap.max(ALIGN).saturating_mul(2);
        out = merge(out, gap);
    }
    out
}

/// Bytes of the (sorted, non-overlapping) `extents` that fall inside `start..start + len`.
pub(crate) fn overlap(extents: &[Extent], start: u64, len: u64) -> u64 {
    let end = start.saturating_add(len);
    let first = extents.partition_point(|e| e.start + e.len <= start);
    extents[first..].iter().take_while(|e| e.start < end).map(|e| (e.start + e.len).min(end) - e.start.max(start)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(map: &[u8], bits: u64) -> Vec<(u64, u64)> {
        let mut v = Vec::new();
        bit_runs(map, bits, 0, |a, n| v.push((a, n)));
        v
    }

    fn naive(map: &[u8], bits: u64) -> Vec<(u64, u64)> {
        let mut v: Vec<(u64, u64)> = Vec::new();
        for i in 0..bits.min(map.len() as u64 * 8) {
            if map[(i / 8) as usize] >> (i % 8) & 1 == 1 {
                match v.last_mut() {
                    Some((a, n)) if *a + *n == i => *n += 1,
                    _ => v.push((i, 1)),
                }
            }
        }
        v
    }

    #[test]
    fn bit_runs_match_a_naive_scan() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..400 {
            let len = (round % 37) as usize;
            let map: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    // Mix all-zero, all-one and random bytes so every branch runs.
                    match x % 4 {
                        0 => 0,
                        1 => 0xFF,
                        _ => x as u8,
                    }
                })
                .collect();
            for bits in [0, 1, 7, 8, 63, 64, 65, 100, (len * 8) as u64, u64::MAX] {
                assert_eq!(runs(&map, bits), naive(&map, bits), "map {map:02x?} bits {bits}");
            }
        }
        assert_eq!(runs(&[0xFF; 16], 128), vec![(0, 128)]);
        let mut v = Vec::new();
        bit_runs(&[0b0110], 8, 1000, |a, n| v.push((a, n)));
        assert_eq!(v, vec![(1001, 2)]);
    }

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    #[test]
    fn normalize_aligns_merges_and_clamps() {
        let e = |start, len| Extent { start, len };
        let v = vec![e(10_000, 100), e(0, 1), e(5000, 10), e(200_000, 1), e(u64::MAX - 5, 5)];
        let out = normalize(v, 300_000, GAP, ALIGN);
        assert_eq!(out, vec![e(0, 12_288), e(196_608, 4096)]);
        // The gap is strict: exactly 64 KiB apart stays apart, less gets merged.
        let out = normalize(vec![e(0, 4096), e(4096 + GAP, 4096)], 1 << 30, GAP, ALIGN);
        assert_eq!(out.len(), 2);
        let out = normalize(vec![e(0, 4096), e(GAP, 4096)], 1 << 30, GAP, ALIGN);
        assert_eq!(out, vec![e(0, GAP + 4096)]);
        // An unaligned end of the drive is kept as is.
        assert_eq!(normalize(vec![e(0, 10_000)], 5000, GAP, ALIGN), vec![e(0, 5000)]);
        assert_eq!(overlap(&[e(0, 100), e(200, 100)], 50, 200), 100);
    }

    #[test]
    fn too_many_extents_get_coarser() {
        let v: Vec<Extent> = (0..200_000u64).map(|i| Extent { start: i * 2 * GAP, len: 4096 }).collect();
        let out = normalize(v, u64::MAX, GAP, ALIGN);
        assert!(out.len() <= MAX_EXTENTS);
        assert_eq!(out.first().map(|e| e.start), Some(0));
        let last = out.last().unwrap();
        assert_eq!(last.start + last.len, 199_999 * 2 * GAP + 4096);
    }

    #[test]
    fn ranges_merge_on_the_fly() {
        let mut r = Ranges::new(GAP);
        r.add(0, 10);
        r.add(100, 10);
        r.add(GAP + 200, 10);
        r.add(5, 1);
        let v = r.into_vec();
        assert_eq!(
            v,
            vec![Extent { start: 0, len: 110 }, Extent { start: GAP + 200, len: 10 }, Extent { start: 5, len: 1 }]
        );
        let mut r = Ranges::new(0);
        r.add(0, 10);
        r.add(10, 10);
        r.add(21, 1);
        assert_eq!(r.into_vec(), vec![Extent { start: 0, len: 20 }, Extent { start: 21, len: 1 }]);
    }
}
