//! Reading the image file: positioned reads that several threads can share, and the
//! helpers every format uses (errors, byte order, checksums).

use std::fmt::Display;
use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

/// The image file, read at any offset.
pub struct Source {
    file: File,
    /// Size of the file.
    pub size: u64,
    /// Changes to apply on top of the file (a VHDX log replayed in memory), in order.
    patches: Vec<Patch>,
    /// Reads up to here (past the end of the file) give zeros (a replayed log may
    /// have grown the file).
    virtual_end: u64,
}

/// A change to the file's contents that only lives in memory.
pub struct Patch {
    pub offset: u64,
    pub len: u64,
    /// None: zeros.
    pub data: Option<Box<[u8]>>,
}

impl Source {
    pub fn new(file: File) -> io::Result<Source> {
        let size = file.metadata()?.len();
        Ok(Source {
            file,
            size,
            patches: Vec::new(),
            virtual_end: size,
        })
    }

    /// A second handle on the same file, for looking at it without disturbing `file`.
    pub fn borrow(file: &File, size: u64) -> io::Result<Source> {
        Ok(Source {
            file: file.try_clone()?,
            size,
            patches: Vec::new(),
            virtual_end: size,
        })
    }

    pub fn set_patches(&mut self, patches: Vec<Patch>, virtual_end: u64) {
        self.patches = patches;
        self.virtual_end = virtual_end.max(self.size);
    }

    /// Fills `buf` from `offset`. Reading past the end of the file is an error (the image
    /// is cut short).
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| bad("an offset is out of range"))?;
        if end > self.virtual_end {
            return Err(truncated(self.size, end));
        }
        let in_file = self.size.saturating_sub(offset).min(buf.len() as u64) as usize;
        read_exact_at(&self.file, &mut buf[..in_file], offset).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                truncated(self.size, end)
            } else {
                e
            }
        })?;
        buf[in_file..].fill(0);
        for patch in &self.patches {
            let (start, stop) = (
                patch.offset.max(offset),
                (patch.offset + patch.len).min(end),
            );
            if start < stop {
                let out = &mut buf[(start - offset) as usize..(stop - offset) as usize];
                match &patch.data {
                    Some(data) => out.copy_from_slice(
                        &data[(start - patch.offset) as usize..(stop - patch.offset) as usize],
                    ),
                    None => out.fill(0),
                }
            }
        }
        Ok(())
    }

    /// `len` bytes from `offset`.
    pub fn read_vec(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| bad("an offset is out of range"))?;
        if end > self.virtual_end {
            return Err(truncated(self.size, end));
        }
        let mut buf = vec![0u8; len];
        self.read_at(&mut buf, offset)?;
        Ok(buf)
    }

    /// Up to `len` bytes from `offset`: fewer where the file ends.
    pub fn read_upto(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let len = self.virtual_end.saturating_sub(offset).min(len as u64) as usize;
        self.read_vec(offset, len)
    }

    /// Reads the file in order, from `offset`.
    pub fn reader(self: Arc<Self>, offset: u64) -> Reader {
        Reader {
            src: self,
            pos: offset,
        }
    }
}

/// A `Source` read in order, from its own position. (Every read says where it reads, so
/// readers never get in each other's way, not even on Windows, where a positioned read
/// moves the file pointer that cloned handles share.)
pub struct Reader {
    src: Arc<Source>,
    pos: u64,
}

impl io::Seek for Reader {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        let pos = match to {
            io::SeekFrom::Start(p) => Some(p),
            io::SeekFrom::End(d) => self.src.virtual_end.checked_add_signed(d),
            io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek to a negative offset")
        })?;
        Ok(self.pos)
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self
            .src
            .virtual_end
            .saturating_sub(self.pos)
            .min(buf.len() as u64) as usize;
        self.src.read_at(&mut buf[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Damaged or unexpected data.
pub fn bad(what: impl Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// A variant of a format that DD-GUI doesn't handle.
pub fn unsupported(what: impl Display) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

fn truncated(size: u64, needed: u64) -> io::Error {
    bad(format!(
        "the image is cut short: the file ends at byte {size}, but data up to byte {needed} is needed"
    ))
}

/// Fills `buf` as far as the reader goes; returns how much it got.
pub fn read_full(r: &mut (impl Read + ?Sized), buf: &mut [u8]) -> io::Result<usize> {
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

// Fixed-size fields. The callers check lengths first; out-of-range reads give zero
// rather than a panic.
pub fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(field(b, at))
}
pub fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(field(b, at))
}
pub fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(field(b, at))
}
pub fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(field(b, at))
}
pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(field(b, at))
}
pub fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(field(b, at))
}

fn field<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b.get(at..at.saturating_add(N))
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; N])
}

/// CRC-32C (Castagnoli), as VHDX uses it.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for (i, entry) in t[0].iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0x82f6_3b78
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
            }
        }
        t
    });
    let mut c = !crc;
    let (blocks, rest) = data.as_chunks::<8>();
    for b in blocks {
        let lo = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ c;
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// A number of bytes for messages: "4 GiB", "512 bytes".
pub fn size_text(n: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut unit = 0;
    let mut v = n as f64;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} bytes")
    } else if v.fract() == 0.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_known_values() {
        assert_eq!(super::crc32c(0, b"123456789"), 0xe306_9283);
        assert_eq!(super::crc32c(0, &[0u8; 32]), 0x8a91_36aa);
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let whole = super::crc32c(0, &data);
        let split = super::crc32c(super::crc32c(0, &data[..333]), &data[333..]);
        assert_eq!(whole, split);
    }
}
