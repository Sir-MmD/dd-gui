//! LZ4 (.lz4): frames one after another (linked or independent blocks, with their
//! checksums verified), skippable frames, and the legacy format (`lz4 -l`, the Linux
//! kernel's). Blocks are decoded by lz4_flex.

use super::source::{Source, bad, le32, le64, unsupported};
use std::io::{self, BufRead, Read};

const MAGIC: u32 = 0x184d_2204;
const LEGACY: u32 = 0x184c_2102;
const LEGACY_BLOCK: usize = 8 << 20;
/// The most a legacy block may take compressed (LZ4_compressBound of 8 MiB).
const LEGACY_PACKED: usize = LEGACY_BLOCK + LEGACY_BLOCK / 255 + 16;
const WINDOW: usize = 64 << 10;

fn skippable(magic: u32) -> bool {
    magic & 0xffff_fff0 == 0x184d_2a50
}

/// An LZ4 frame (after any skippable frames), or a legacy one.
pub fn detect(src: &Source, head: &[u8]) -> bool {
    let first = le32(head, 0);
    if head.len() < 8 || !(first == MAGIC || first == LEGACY || skippable(first)) {
        return false;
    }
    let mut at = 0u64;
    for _ in 0..1000 {
        let Ok(b) = src.read_upto(at, 32) else {
            return false;
        };
        if b.len() < 8 {
            return false;
        }
        let magic = le32(&b, 0);
        if skippable(magic) {
            at = at.saturating_add(8 + le32(&b, 4) as u64);
            continue;
        }
        return match magic {
            MAGIC => descriptor(&b[4..]).is_ok(),
            LEGACY => {
                let first = le32(&b, 4) as usize;
                first > 0 && first <= LEGACY_PACKED
            }
            _ => false,
        };
    }
    false
}

struct Frame {
    block_max: usize,
    linked: bool,
    block_checksum: bool,
    content_checksum: bool,
    content_size: Option<u64>,
    /// Bytes of the frame descriptor (including its checksum byte).
    len: usize,
}

/// Parses a frame descriptor (FLG, BD, optional content size and dictionary ID, header
/// checksum) from the bytes after the magic number.
fn descriptor(b: &[u8]) -> io::Result<Frame> {
    let damaged = || bad("the LZ4 frame header is damaged");
    if b.len() < 3 {
        return Err(damaged());
    }
    let (flg, bd) = (b[0], b[1]);
    if flg >> 6 != 1 || flg & 2 != 0 || bd & 0x8f != 0 || bd >> 4 < 4 {
        return Err(damaged());
    }
    let has_size = flg & 8 != 0;
    let has_dict = flg & 1 != 0;
    let len = 2 + if has_size { 8 } else { 0 } + if has_dict { 4 } else { 0 };
    if b.len() < len + 1 {
        return Err(damaged());
    }
    if (xxh32(&b[..len]) >> 8) as u8 != b[len] {
        return Err(damaged());
    }
    if has_dict {
        return Err(unsupported(
            "this LZ4 file needs a separate dictionary, which DD-GUI doesn't support",
        ));
    }
    Ok(Frame {
        block_max: 1 << (8 + 2 * (bd >> 4)),
        linked: flg & 0x20 == 0,
        block_checksum: flg & 0x10 != 0,
        content_checksum: flg & 4 != 0,
        content_size: has_size.then(|| le64(b, 2)),
        len: len + 1,
    })
}

/// The decoded size of the whole file, when every frame records its content size. None
/// otherwise (or when there are too many blocks to walk quickly).
pub fn total_size(src: &Source) -> Option<u64> {
    let (mut at, mut total, mut steps) = (0u64, 0u64, 0u32);
    let mut step = || {
        steps += 1;
        (steps <= 1_000_000).then_some(())
    };
    while at < src.size {
        step()?;
        let head = src.read_upto(at, 19).ok()?;
        let magic = le32(&head, 0);
        if skippable(magic) {
            at = at.checked_add(8 + le32(&head, 4) as u64)?;
            continue;
        }
        if magic != MAGIC {
            return None;
        }
        let frame = descriptor(head.get(4..)?).ok()?;
        total = total.checked_add(frame.content_size?)?;
        at += 4 + frame.len as u64;
        loop {
            step()?;
            let b = src.read_upto(at, 4).ok()?;
            if b.len() < 4 {
                return None;
            }
            let v = le32(&b, 0);
            at += 4;
            if v == 0 {
                at += if frame.content_checksum { 4 } else { 0 };
                break;
            }
            at += (v & 0x7fff_ffff) as u64 + if frame.block_checksum { 4 } else { 0 };
        }
    }
    Some(total)
}

enum State {
    /// Before a frame (or at the end).
    Between,
    Frame {
        frame: Frame,
        hash: Xxh32,
        produced: u64,
    },
    Legacy,
}

pub struct Lz4<R> {
    src: R,
    state: State,
    frames: u64,
    /// A magic number the legacy reader already took.
    next_magic: Option<u32>,
    packed: Vec<u8>,
    out: Vec<u8>,
    at: usize,
    /// The last 64 KiB of output, for linked blocks.
    window: Vec<u8>,
}

impl<R: BufRead> Lz4<R> {
    pub fn new(src: R) -> Self {
        Lz4 {
            src,
            state: State::Between,
            frames: 0,
            next_magic: None,
            packed: Vec::new(),
            out: Vec::new(),
            at: 0,
            window: Vec::new(),
        }
    }

    /// A little-endian u32; None at a clean end of the input.
    fn u32_or_end(&mut self) -> io::Result<Option<u32>> {
        let mut b = [0u8; 4];
        let n = super::source::read_full(&mut self.src, &mut b)?;
        match n {
            0 => Ok(None),
            4 => Ok(Some(u32::from_le_bytes(b))),
            _ => Err(bad("the LZ4 data is cut short")),
        }
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.u32_or_end()?
            .ok_or_else(|| bad("the LZ4 data is cut short"))
    }

    fn fill_packed(&mut self, len: usize) -> io::Result<()> {
        self.packed.resize(len, 0);
        self.src
            .read_exact(&mut self.packed)
            .map_err(|_| bad("the LZ4 data is cut short"))
    }

    /// Decodes the next block into `out`. False at the end of the data.
    fn next_block(&mut self) -> io::Result<bool> {
        loop {
            match &mut self.state {
                State::Between => {
                    let magic = match self.next_magic.take() {
                        Some(m) => m,
                        None => match self.u32_or_end()? {
                            Some(m) => m,
                            None if self.frames > 0 => return Ok(false),
                            None => return Err(bad("the LZ4 file is empty")),
                        },
                    };
                    match magic {
                        MAGIC => {
                            // The descriptor is 3 to 15 bytes long; its first byte says.
                            let mut d = [0u8; 15];
                            self.src
                                .read_exact(&mut d[..3])
                                .map_err(|_| bad("the LZ4 data is cut short"))?;
                            let extra = if d[0] & 8 != 0 { 8 } else { 0 }
                                + if d[0] & 1 != 0 { 4 } else { 0 };
                            self.src
                                .read_exact(&mut d[3..3 + extra])
                                .map_err(|_| bad("the LZ4 data is cut short"))?;
                            let frame = descriptor(&d[..3 + extra])?;
                            self.window.clear();
                            self.frames += 1;
                            self.state = State::Frame {
                                frame,
                                hash: Xxh32::new(),
                                produced: 0,
                            };
                        }
                        LEGACY => {
                            self.frames += 1;
                            self.state = State::Legacy;
                        }
                        m if skippable(m) => {
                            let len = self.u32()? as u64;
                            let skipped =
                                io::copy(&mut (&mut self.src).take(len), &mut io::sink())?;
                            if skipped < len {
                                return Err(bad("the LZ4 data is cut short"));
                            }
                        }
                        _ if self.frames == 0 => return Err(bad("this isn't LZ4 data")),
                        _ => return Err(bad("the LZ4 file has unexpected data after a frame")),
                    }
                }
                State::Frame { .. } => {
                    let size = self.u32()?;
                    let State::Frame {
                        frame,
                        hash,
                        produced,
                    } = &mut self.state
                    else {
                        unreachable!()
                    };
                    if size == 0 {
                        // End mark, then the content checksum.
                        let (want_hash, want_size, got_size) =
                            (frame.content_checksum, frame.content_size, *produced);
                        let digest = hash.digest();
                        self.state = State::Between;
                        if want_hash && self.u32()? != digest {
                            return Err(bad("the LZ4 data is damaged (checksum mismatch)"));
                        }
                        if want_size.is_some_and(|s| s != got_size) {
                            return Err(bad("the LZ4 data is damaged (wrong length)"));
                        }
                        continue;
                    }
                    let raw = size & 0x8000_0000 != 0;
                    let len = (size & 0x7fff_ffff) as usize;
                    let (block_max, linked, block_checksum) =
                        (frame.block_max, frame.linked, frame.block_checksum);
                    if len > block_max {
                        return Err(bad("the LZ4 data is damaged (a block is too big)"));
                    }
                    self.fill_packed(len)?;
                    if block_checksum && self.u32()? != xxh32(&self.packed) {
                        return Err(bad("the LZ4 data is damaged (block checksum mismatch)"));
                    }
                    if raw {
                        std::mem::swap(&mut self.out, &mut self.packed);
                    } else {
                        self.out.resize(block_max, 0);
                        let n = if linked && !self.window.is_empty() {
                            lz4_flex::block::decompress_into_with_dict(
                                &self.packed,
                                &mut self.out,
                                &self.window,
                            )
                        } else {
                            lz4_flex::block::decompress_into(&self.packed, &mut self.out)
                        }
                        .map_err(|e| bad(format!("the LZ4 data is damaged ({e})")))?;
                        self.out.truncate(n);
                    }
                    if linked {
                        remember(&mut self.window, &self.out);
                    }
                    let State::Frame {
                        frame,
                        hash,
                        produced,
                    } = &mut self.state
                    else {
                        unreachable!()
                    };
                    if frame.content_checksum {
                        hash.update(&self.out);
                    }
                    *produced += self.out.len() as u64;
                    self.at = 0;
                    return Ok(true);
                }
                State::Legacy => {
                    let Some(len) = self.u32_or_end()? else {
                        return Ok(false);
                    };
                    if len == MAGIC || len == LEGACY || skippable(len) {
                        // Another frame follows.
                        self.next_magic = Some(len);
                        self.state = State::Between;
                        continue;
                    }
                    if len as usize > LEGACY_PACKED || len == 0 {
                        return Err(bad("the LZ4 data is damaged (a block is too big)"));
                    }
                    self.fill_packed(len as usize)?;
                    self.out.resize(LEGACY_BLOCK, 0);
                    let n = lz4_flex::block::decompress_into(&self.packed, &mut self.out)
                        .map_err(|e| bad(format!("the LZ4 data is damaged ({e})")))?;
                    self.out.truncate(n);
                    self.at = 0;
                    return Ok(true);
                }
            }
        }
    }
}

/// Keeps the last 64 KiB of output.
fn remember(window: &mut Vec<u8>, out: &[u8]) {
    if out.len() >= WINDOW {
        window.clear();
        window.extend_from_slice(&out[out.len() - WINDOW..]);
    } else {
        let keep = (WINDOW - out.len()).min(window.len());
        window.drain(..window.len() - keep);
        window.extend_from_slice(out);
    }
}

impl<R: BufRead> Read for Lz4<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.out.len() {
            if buf.is_empty() || !self.next_block()? {
                return Ok(0);
            }
        }
        let n = (self.out.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

const P1: u32 = 0x9e37_79b1;
const P2: u32 = 0x85eb_ca77;
const P3: u32 = 0xc2b2_ae3d;
const P4: u32 = 0x27d4_eb2f;
const P5: u32 = 0x1656_67b1;

/// xxHash32 with seed 0, as LZ4 frames use it.
struct Xxh32 {
    v: [u32; 4],
    buf: [u8; 16],
    buffered: usize,
    total: u64,
}

impl Xxh32 {
    fn new() -> Self {
        Xxh32 {
            v: [P1.wrapping_add(P2), P2, 0, 0u32.wrapping_sub(P1)],
            buf: [0; 16],
            buffered: 0,
            total: 0,
        }
    }

    fn round(acc: u32, lane: u32) -> u32 {
        acc.wrapping_add(lane.wrapping_mul(P2))
            .rotate_left(13)
            .wrapping_mul(P1)
    }

    fn stripe(&mut self, s: &[u8; 16]) {
        for (v, lane) in self.v.iter_mut().zip(s.as_chunks::<4>().0) {
            *v = Self::round(*v, u32::from_le_bytes(*lane));
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buffered > 0 {
            let n = (16 - self.buffered).min(data.len());
            self.buf[self.buffered..self.buffered + n].copy_from_slice(&data[..n]);
            self.buffered += n;
            data = &data[n..];
            if self.buffered < 16 {
                return;
            }
            let buf = self.buf;
            self.stripe(&buf);
            self.buffered = 0;
        }
        let (stripes, rest) = data.as_chunks::<16>();
        for s in stripes {
            self.stripe(s);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    fn digest(&self) -> u32 {
        let mut h = if self.total >= 16 {
            self.v[0]
                .rotate_left(1)
                .wrapping_add(self.v[1].rotate_left(7))
                .wrapping_add(self.v[2].rotate_left(12))
                .wrapping_add(self.v[3].rotate_left(18))
        } else {
            P5
        };
        h = h.wrapping_add(self.total as u32);
        let (words, bytes) = self.buf[..self.buffered].as_chunks::<4>();
        for w in words {
            h = h
                .wrapping_add(u32::from_le_bytes(*w).wrapping_mul(P3))
                .rotate_left(17)
                .wrapping_mul(P4);
        }
        for &b in bytes {
            h = h
                .wrapping_add((b as u32).wrapping_mul(P5))
                .rotate_left(11)
                .wrapping_mul(P1);
        }
        h ^= h >> 15;
        h = h.wrapping_mul(P2);
        h ^= h >> 13;
        h = h.wrapping_mul(P3);
        h ^ (h >> 16)
    }
}

fn xxh32(data: &[u8]) -> u32 {
    let mut h = Xxh32::new();
    h.update(data);
    h.digest()
}

#[cfg(test)]
mod tests {
    #[test]
    fn xxh32_known_values() {
        assert_eq!(super::xxh32(b""), 0x02cc_5d05);
        assert_eq!(super::xxh32(b"a"), 0x550d_7456);
        assert_eq!(super::xxh32(b"abc"), 0x32d1_53ff);
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        let mut h = super::Xxh32::new();
        for piece in data.chunks(7) {
            h.update(piece);
        }
        assert_eq!(h.digest(), super::xxh32(&data));
    }
}
