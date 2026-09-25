//! tar archives (ustar, GNU, pax): the first regular file in them. Long names, pax
//! headers (including sizes past 8 GiB) and GNU base-256 sizes are understood, and so
//! are sparse files (old GNU sparse headers, and pax sparse formats 0.0, 0.1 and 1.0),
//! whose holes come out as zeros.

use super::source::{bad, read_full};
use std::io::{self, Read};

const BLOCK: usize = 512;
/// The most we read of a pax header or a GNU long name.
const MAX_META: u64 = 1 << 20;
/// The most pieces a sparse map may have.
const MAX_SPARSE: usize = 1 << 20;

/// Is this 512-byte block a ustar/GNU/pax header?
pub fn is_header(b: &[u8]) -> bool {
    b.len() >= BLOCK && &b[257..262] == b"ustar" && checksum_ok(b)
}

fn checksum_ok(b: &[u8]) -> bool {
    let Some(stored) = number(&b[148..156]) else {
        return false;
    };
    // The sum of all bytes with the checksum field as spaces; some old tars summed
    // signed bytes.
    let field = 148..156;
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &x) in b[..BLOCK].iter().enumerate() {
        let x = if field.contains(&i) { b' ' } else { x };
        unsigned += x as u64;
        signed += x as i8 as i64;
    }
    stored == unsigned || stored as i64 == signed
}

/// A numeric field: octal text, or GNU's base-256 (top bit of the first byte set).
fn number(field: &[u8]) -> Option<u64> {
    if let Some(&first) = field.first()
        && first & 0x80 != 0
    {
        if first == 0xff {
            return None; // negative
        }
        let mut v = (first & 0x7f) as u64;
        for &b in &field[1..] {
            v = v.checked_mul(256)?.checked_add(b as u64)?;
        }
        return Some(v);
    }
    let text = field
        .iter()
        .skip_while(|&&b| b == b' ' || b == 0)
        .take_while(|&&b| (b'0'..=b'7').contains(&b))
        .try_fold(0u64, |v, &b| {
            v.checked_mul(8)?.checked_add((b - b'0') as u64)
        });
    // An all-blank field is no number.
    if field.iter().all(|&b| b == b' ' || b == 0) {
        return None;
    }
    text
}

/// The first regular file of the archive in `r`, whose first header `first` has been
/// read already: a reader for its contents, and its size.
pub fn first_file(
    first: Vec<u8>,
    mut r: Box<dyn Read + Send>,
) -> io::Result<(Box<dyn Read + Send>, u64)> {
    let cut = || bad("the tar archive is cut short");
    let mut header = first;
    // What a pax header said about the next entry.
    let mut pax = Pax::default();
    loop {
        if header.iter().all(|&b| b == 0) {
            return Err(bad("the tar archive holds no files"));
        }
        if !checksum_ok(&header) {
            return Err(bad(
                "the tar archive is damaged (a header's checksum doesn't match)",
            ));
        }
        let kind = header[156];
        let stored = match pax.size {
            Some(size) => size,
            None => number(&header[124..136]).ok_or_else(|| bad("the tar archive is damaged"))?,
        };
        match kind {
            b'x' | b'g' => {
                let data = read_meta(&mut r, stored)?;
                if kind == b'x' {
                    pax = Pax::parse(&data)?;
                }
                // A global header ('g') holds defaults for every file; none of what we
                // use (sizes, sparse maps) belongs there.
            }
            b'0' | 0 | b'7' if pax.sparse.is_some() || pax.sparse_v1 => {
                return sparse_pax(r, stored, pax);
            }
            b'0' | 0 | b'7' if stored > 0 => {
                return Ok((
                    Box::new(Exact {
                        inner: r,
                        left: stored,
                    }),
                    stored,
                ));
            }
            b'S' => return sparse_gnu(r, &header, stored),
            _ => {
                // Directories, links, long names, empty files…: skip their data.
                skip(&mut r, padded(stored))?;
                pax = Pax::default();
            }
        }
        if kind == b'x' || kind == b'g' {
            // The pax header applies to the header that comes next.
        } else {
            pax = Pax::default();
        }
        header = vec![0u8; BLOCK];
        if read_full(&mut r, &mut header)? < BLOCK {
            return Err(cut());
        }
    }
}

fn padded(n: u64) -> u64 {
    n.div_ceil(BLOCK as u64).saturating_mul(BLOCK as u64)
}

fn skip(r: &mut impl Read, n: u64) -> io::Result<()> {
    if io::copy(&mut r.take(n), &mut io::sink())? < n {
        return Err(bad("the tar archive is cut short"));
    }
    Ok(())
}

/// A pax header's or GNU long name's data (and its padding).
fn read_meta(r: &mut impl Read, len: u64) -> io::Result<Vec<u8>> {
    if len > MAX_META {
        return Err(bad("the tar archive is damaged (a huge extended header)"));
    }
    let mut data = vec![0u8; len as usize];
    if read_full(r, &mut data)? < data.len() {
        return Err(bad("the tar archive is cut short"));
    }
    skip(r, padded(len) - len)?;
    Ok(data)
}

/// What a pax extended header says.
#[derive(Default)]
struct Pax {
    size: Option<u64>,
    /// GNU sparse 0.0/0.1: the map and the real size.
    sparse: Option<(Vec<(u64, u64)>, u64)>,
    /// GNU sparse 1.0: the map is at the start of the data.
    sparse_v1: bool,
    real_size: Option<u64>,
}

impl Pax {
    fn parse(data: &[u8]) -> io::Result<Pax> {
        let damaged = || bad("the tar archive is damaged (pax header)");
        let mut pax = Pax::default();
        let (mut offsets, mut map) = (Vec::new(), Vec::new());
        let (mut major, mut minor) = (None, None);
        let mut at = 0;
        while at < data.len() {
            // "LEN key=value\n", LEN counting the whole record.
            let space = data[at..]
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(damaged)?;
            let len: usize = std::str::from_utf8(&data[at..at + space])
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&l| {
                    l > space + 1 && at.checked_add(l).is_some_and(|end| end <= data.len())
                })
                .ok_or_else(damaged)?;
            let record = &data[at + space + 1..at + len - 1];
            at += len;
            let Some(eq) = record.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (key, value) = (&record[..eq], &record[eq + 1..]);
            let num = || -> io::Result<u64> {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(damaged)
            };
            match key {
                b"size" => pax.size = Some(num()?),
                b"GNU.sparse.size" | b"GNU.sparse.realsize" => pax.real_size = Some(num()?),
                b"GNU.sparse.major" => major = Some(num()?),
                b"GNU.sparse.minor" => minor = Some(num()?),
                // 0.0: repeated offset/numbytes records.
                b"GNU.sparse.offset" => offsets.push(num()?),
                b"GNU.sparse.numbytes" => {
                    let off = offsets.pop().ok_or_else(damaged)?;
                    map.push((off, num()?));
                }
                // 0.1: one list.
                b"GNU.sparse.map" => {
                    let numbers: Vec<u64> = std::str::from_utf8(value)
                        .map_err(|_| damaged())?
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.trim().parse().map_err(|_| damaged()))
                        .collect::<io::Result<_>>()?;
                    if !numbers.len().is_multiple_of(2) || numbers.len() / 2 > MAX_SPARSE {
                        return Err(damaged());
                    }
                    map.extend(
                        numbers
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|&[offset, len]| (offset, len)),
                    );
                }
                _ => {}
            }
            if map.len() > MAX_SPARSE {
                return Err(damaged());
            }
        }
        if major == Some(1) && minor == Some(0) {
            pax.sparse_v1 = true;
        } else if (!map.is_empty() || major.is_some()) && pax.real_size.is_some() {
            pax.sparse = Some((map, pax.real_size.unwrap_or(0)));
        } else if major.is_some() || minor.is_some() {
            return Err(bad(format!(
                "the tar archive uses a sparse format DD-GUI doesn't know ({}.{})",
                major.unwrap_or(0),
                minor.unwrap_or(0)
            )));
        }
        Ok(pax)
    }
}

/// A pax sparse file: the map from the pax header (0.0, 0.1) or from the start of the
/// data (1.0).
fn sparse_pax(
    mut r: Box<dyn Read + Send>,
    stored: u64,
    pax: Pax,
) -> io::Result<(Box<dyn Read + Send>, u64)> {
    let damaged = || bad("the tar archive is damaged (sparse map)");
    if let Some((map, real)) = pax.sparse {
        return sparse(r, map, real, stored);
    }
    // 1.0: decimal numbers, one per line: the count, then offset/size pairs, padded to a
    // whole block.
    let real = pax.real_size.ok_or_else(damaged)?;
    let mut used = 0u64;
    let mut line = || -> io::Result<u64> {
        let mut digits = Vec::new();
        loop {
            let mut b = [0u8; 1];
            if read_full(&mut r, &mut b)? == 0 {
                return Err(bad("the tar archive is cut short"));
            }
            used += 1;
            if b[0] == b'\n' {
                break;
            }
            if !b[0].is_ascii_digit() || digits.len() > 20 {
                return Err(damaged());
            }
            digits.push(b[0]);
        }
        std::str::from_utf8(&digits)
            .ok()
            .and_then(|d| d.parse().ok())
            .ok_or_else(damaged)
    };
    let count = line()? as usize;
    if count > MAX_SPARSE {
        return Err(damaged());
    }
    let mut map = Vec::with_capacity(count);
    for _ in 0..count {
        let offset = line()?;
        map.push((offset, line()?));
    }
    let map_len = padded(used);
    if map_len > stored {
        return Err(damaged());
    }
    skip(&mut r, map_len - used)?;
    sparse(r, map, real, stored - map_len)
}

/// An old GNU sparse file ('S'): four map entries in the header, more in extension
/// blocks after it.
fn sparse_gnu(
    mut r: Box<dyn Read + Send>,
    header: &[u8],
    stored: u64,
) -> io::Result<(Box<dyn Read + Send>, u64)> {
    let damaged = || bad("the tar archive is damaged (sparse map)");
    let mut map = Vec::new();
    let mut add = |entries: &[u8]| -> io::Result<()> {
        for e in entries.as_chunks::<24>().0 {
            if e.iter().all(|&b| b == 0) {
                break;
            }
            let offset = number(&e[..12]).ok_or_else(damaged)?;
            let len = number(&e[12..]).ok_or_else(damaged)?;
            map.push((offset, len));
        }
        if map.len() > MAX_SPARSE {
            return Err(damaged());
        }
        Ok(())
    };
    add(&header[386..482])?;
    let real = number(&header[483..495]).ok_or_else(damaged)?;
    let mut extended = header[482] != 0;
    while extended {
        let mut block = [0u8; BLOCK];
        if read_full(&mut r, &mut block)? < BLOCK {
            return Err(bad("the tar archive is cut short"));
        }
        add(&block[..504])?;
        extended = block[504] != 0;
    }
    sparse(r, map, real, stored)
}

/// Checks a sparse map and makes the reader that fills its holes with zeros.
fn sparse(
    r: Box<dyn Read + Send>,
    map: Vec<(u64, u64)>,
    real: u64,
    stored: u64,
) -> io::Result<(Box<dyn Read + Send>, u64)> {
    let damaged = || bad("the tar archive is damaged (sparse map)");
    let mut end = 0u64;
    let mut total = 0u64;
    for &(offset, len) in &map {
        let stop = offset.checked_add(len).ok_or_else(damaged)?;
        if offset < end || stop > real {
            return Err(damaged());
        }
        end = stop;
        total += len;
    }
    if total != stored {
        return Err(damaged());
    }
    Ok((
        Box::new(Sparse {
            inner: r,
            map: map.into_iter().filter(|&(_, len)| len > 0).collect(),
            next: 0,
            pos: 0,
            real,
        }),
        real,
    ))
}

/// Exactly `left` bytes of `inner`; ending sooner is an error.
struct Exact {
    inner: Box<dyn Read + Send>,
    left: u64,
}

impl Read for Exact {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.left) as usize;
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(bad("the tar archive is cut short"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

struct Sparse {
    inner: Box<dyn Read + Send>,
    /// (offset, length) of the stored pieces, in order.
    map: Vec<(u64, u64)>,
    next: usize,
    pos: u64,
    real: u64,
}

impl Read for Sparse {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.real || buf.is_empty() {
            return Ok(0);
        }
        let room = buf.len() as u64;
        match self.map.get(self.next).copied() {
            Some((offset, _)) if self.pos < offset => {
                let n = (offset - self.pos).min(room) as usize;
                buf[..n].fill(0);
                self.pos += n as u64;
                Ok(n)
            }
            Some((offset, len)) => {
                let want = (offset + len - self.pos).min(room) as usize;
                let n = self.inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(bad("the tar archive is cut short"));
                }
                self.pos += n as u64;
                if self.pos == offset + len {
                    self.next += 1;
                }
                Ok(n)
            }
            None => {
                let n = (self.real - self.pos).min(room) as usize;
                buf[..n].fill(0);
                self.pos += n as u64;
                Ok(n)
            }
        }
    }
}

/// For `detect`: if `head` starts a tar archive, the size of its first regular file
/// when the headers in `head` tell it (Some(None) when they don't).
pub fn probe(head: &[u8]) -> Option<Option<u64>> {
    if !is_header(head) {
        return None;
    }
    let reader: Box<dyn Read + Send> = Box::new(io::Cursor::new(head[BLOCK..].to_vec()));
    Some(
        first_file(head[..BLOCK].to_vec(), reader)
            .ok()
            .map(|(_, size)| size),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number(b"0000644\0"), Some(0o644));
        assert_eq!(number(b"   17 \0"), Some(0o17));
        assert_eq!(number(b"\0\0\0\0"), None);
        let mut big = [0u8; 12];
        big[0] = 0x80;
        big[4..].copy_from_slice(&(20u64 << 30).to_be_bytes());
        assert_eq!(number(&big), Some(20 << 30));
        assert_eq!(number(&[0xff; 12]), None);
    }

    #[test]
    fn pax_records() {
        let pax = Pax::parse(b"20 size=12345678901\n28 GNU.sparse.realsize=4096\n").unwrap();
        assert_eq!(pax.size, Some(12_345_678_901));
        assert_eq!(pax.real_size, Some(4096));
        assert!(Pax::parse(b"99 size=1\n").is_err());
        assert!(Pax::parse(b"x").is_err());
    }
}
