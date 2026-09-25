//! LZMA "alone" (.lzma, from `xz --format=lzma` or the LZMA SDK): a 13-byte header
//! (properties, dictionary size, uncompressed size or "unknown"), then one LZMA stream.
//!
//! Its "magic number" is weak (usually 5D 00 00 …), so a file only counts as .lzma when
//! the header is plausible the way xz checks it (lc + lp ≤ 4, a dictionary size of 2^n
//! or 2^n + 2^(n-1), a sane size), the range coder's first byte is zero, and the first
//! 64 KiB actually decode.

use super::source::{Source, bad, le32, le64, read_full, size_text, unsupported};
use std::io::{self, Read};

/// How much a trial decode needs to produce (less if the stream ends sooner).
const TRIAL: usize = 64 << 10;
/// The biggest dictionary we allocate.
pub const MAX_DICT: u32 = 1536 << 20;

pub struct Header {
    pub props: u8,
    pub dict: u32,
    /// None when the stream ends with an end marker instead.
    pub size: Option<u64>,
}

pub fn header(head: &[u8]) -> Option<Header> {
    if head.len() < 14 || head[0] >= 9 * 5 * 5 || head[13] != 0 {
        return None;
    }
    let props = head[0];
    let (lc, lp) = (props % 9, props / 9 % 5);
    let dict = le32(head, 1);
    let size = le64(head, 5);
    // Nothing makes dictionaries under 4 KiB, and an empty image isn't worth claiming.
    let sane_size = size == u64::MAX || (1..1 << 48).contains(&size);
    if lc + lp > 4 || dict < 4096 || !plausible_dict(dict) || !sane_size {
        return None;
    }
    Some(Header {
        props,
        dict,
        size: (size != u64::MAX).then_some(size),
    })
}

/// 2^n or 2^n + 2^(n-1) (or "unknown"), as xz requires to tell .lzma files from other
/// data: rounding up to the next such size (xz's trick) must change nothing.
fn plausible_dict(d: u32) -> bool {
    if d == u32::MAX {
        return true;
    }
    let mut x = d.wrapping_sub(1);
    x |= x >> 2;
    x |= x >> 3;
    x |= x >> 4;
    x |= x >> 8;
    x |= x >> 16;
    x.wrapping_add(1) == d
}

/// Tries the first bytes: Some(what they decode to) if this really is an .lzma stream.
pub fn trial(src: &Source, head: &[u8]) -> Option<Vec<u8>> {
    let h = header(head)?;
    // Up to 64 KiB of output never reaches further back than that, so a small dictionary
    // decodes it the same (and a hostile header can't make us allocate much).
    let dict = h.dict.clamp(4096, 1 << 20);
    let want = h.size.map_or(TRIAL, |s| (s as usize).min(TRIAL));
    let stream = std::io::BufReader::new(SourceFrom { src, pos: 13 });
    let mut lz = lzma_rust2::LzmaReader::new_with_props(
        stream,
        h.size.unwrap_or(u64::MAX),
        h.props,
        dict,
        None,
    )
    .ok()?;
    let mut out = vec![0u8; want];
    let n = read_full(&mut lz, &mut out).ok()?;
    // Enough output, or a stream that ended properly before that.
    if n < want {
        let mut more = [0u8; 1];
        if !matches!(lz.read(&mut more), Ok(0)) {
            return None;
        }
        if h.size.is_some_and(|s| s != n as u64) {
            return None;
        }
    }
    out.truncate(n);
    Some(out)
}

/// A `Source` read in order, borrowed (for trial decodes).
struct SourceFrom<'a> {
    src: &'a Source,
    pos: u64,
}

impl Read for SourceFrom<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.src.size.saturating_sub(self.pos).min(buf.len() as u64) as usize;
        self.src.read_at(&mut buf[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// The decoder over the stream after the 13-byte header.
pub fn decoder<R: Read>(r: R, h: &Header) -> io::Result<lzma_rust2::LzmaReader<R>> {
    if h.dict > MAX_DICT {
        return Err(unsupported(format!(
            "this .lzma file needs a {} dictionary, more memory than DD-GUI allows",
            size_text(h.dict as u64)
        )));
    }
    lzma_rust2::LzmaReader::new_with_props(r, h.size.unwrap_or(u64::MAX), h.props, h.dict, None)
        .map_err(|e| bad(format!("the LZMA data is damaged ({e})")))
}
