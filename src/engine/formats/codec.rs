//! Decoders for the compressed blocks inside disk images: whole blocks in memory, with
//! the output limited to what the block may hold (so a damaged or hostile block can't
//! make us allocate more).

use super::source::bad;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// zlib (RFC 1950): DMG (UDZO).
    Zlib,
    /// Raw deflate (RFC 1951): QCOW2.
    Deflate,
    /// zstd frames: QCOW2.
    Zstd,
    /// A bzip2 stream: DMG (UDBZ).
    Bzip2,
    /// An LZFSE stream: DMG (ULFO).
    Lzfse,
    /// An xz stream: DMG (ULMO).
    Xz,
    /// Apple Data Compression: DMG (UDCO).
    Adc,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Zlib => "zlib",
            Codec::Deflate => "deflate",
            Codec::Zstd => "zstd",
            Codec::Bzip2 => "bzip2",
            Codec::Lzfse => "LZFSE",
            Codec::Xz => "xz",
            Codec::Adc => "ADC",
        }
    }
}

/// Decodes `input` into `out` (cleared first), producing at most `max` bytes. Stops early
/// where the data ends; the caller checks that enough came out.
pub fn decode(codec: Codec, input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    out.clear();
    out.reserve_exact(max);
    let result = match codec {
        Codec::Zlib => inflate(input, out, max, true),
        Codec::Deflate => inflate(input, out, max, false),
        Codec::Zstd => unzstd(input, out, max),
        Codec::Bzip2 => bunzip2(input, out, max),
        Codec::Lzfse => unlzfse(input, out, max),
        Codec::Xz => unxz(input, out, max),
        Codec::Adc => unadc(input, out, max),
    };
    result.map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            e
        } else {
            bad(format!("{} data doesn't decode: {e}", codec.name()))
        }
    })
}

fn inflate(input: &[u8], out: &mut Vec<u8>, max: usize, zlib: bool) -> io::Result<()> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut z = Decompress::new(zlib);
    loop {
        let before = (z.total_in(), out.len());
        let status = z
            .decompress_vec(
                &input[z.total_in() as usize..],
                out,
                FlushDecompress::Finish,
            )
            .map_err(|e| bad(format!("invalid deflate data ({e})")))?;
        if status == Status::StreamEnd || out.len() >= max {
            return Ok(());
        }
        if (z.total_in(), out.len()) == before {
            // Out of input before the end of the stream.
            return Ok(());
        }
    }
}

fn unzstd(input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};
    let mut decoder = Decoder::new()?;
    let mut src = InBuffer::around(input);
    // One frame after another (as QEMU writes them), until the cluster is full.
    out.resize(max, 0);
    let mut dst = OutBuffer::around(&mut out[..]);
    let mut result = Ok(());
    while dst.pos() < max && src.pos < input.len() {
        let before = (src.pos, dst.pos());
        if let Err(e) = decoder.run(&mut src, &mut dst) {
            result = Err(bad(format!("invalid zstd data ({e})")));
            break;
        }
        if (src.pos, dst.pos()) == before {
            break;
        }
    }
    let n = dst.pos();
    out.truncate(n);
    result
}

fn bunzip2(input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    use bzip2::{Decompress, Status};
    let mut bz = Decompress::new(false);
    loop {
        let before = (bz.total_in(), out.len());
        let status = bz
            .decompress_vec(&input[bz.total_in() as usize..], out)
            .map_err(|e| bad(format!("invalid bzip2 data ({e})")))?;
        if status == Status::StreamEnd || out.len() >= max {
            return Ok(());
        }
        if (bz.total_in(), out.len()) == before {
            return Ok(());
        }
    }
}

fn unxz(input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    let mut xz = lzma_rust2::XzReader::new(input, false);
    out.resize(max, 0);
    let n = super::source::read_full(&mut xz, &mut out[..]);
    let n = match n {
        Ok(n) => n,
        Err(e) => {
            out.clear();
            return Err(bad(format!("invalid xz data ({e})")));
        }
    };
    out.truncate(n);
    Ok(())
}

/// LZFSE through the ring-buffer decoder, which never holds more than its rings: the
/// output goes to a writer that refuses anything past `max`.
fn unlzfse(input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    struct Limited<'a> {
        out: &'a mut Vec<u8>,
        max: usize,
    }
    impl io::Write for Limited<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.out.len() + buf.len() > self.max {
                return Err(bad("LZFSE data decodes to more than its block holds"));
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut decoder = lzfse_rust::LzfseRingDecoder::default();
    let mut src = input;
    decoder
        .decode(&mut src, &mut Limited { out, max })
        .map(|_| ())
        .map_err(|e| match e {
            lzfse_rust::Error::Io(e) if e.kind() == io::ErrorKind::InvalidData => e,
            e => bad(format!("invalid LZFSE data ({e})")),
        })
}

/// Apple Data Compression (the "ADC" of old UDCO disk images): byte-oriented LZ77.
fn unadc(input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<()> {
    let short = || bad("invalid ADC data (it ends in the middle of a code)");
    let mut at = 0;
    while at < input.len() && out.len() < max {
        let b = input[at];
        at += 1;
        if b & 0x80 != 0 {
            // A run of literal bytes.
            let n = (b & 0x7f) as usize + 1;
            let lit = input.get(at..at + n).ok_or_else(short)?;
            let n = n.min(max - out.len());
            out.extend_from_slice(&lit[..n]);
            at += (b & 0x7f) as usize + 1;
            continue;
        }
        let (len, dist) = if b & 0x40 != 0 {
            let d = input.get(at..at + 2).ok_or_else(short)?;
            at += 2;
            (
                (b & 0x3f) as usize + 4,
                ((d[0] as usize) << 8 | d[1] as usize) + 1,
            )
        } else {
            let d = *input.get(at).ok_or_else(short)?;
            at += 1;
            (
                ((b >> 2) & 0x0f) as usize + 3,
                (((b & 3) as usize) << 8 | d as usize) + 1,
            )
        };
        if dist > out.len() {
            return Err(bad(
                "invalid ADC data (a match reaches back before the start)",
            ));
        }
        let len = len.min(max - out.len());
        let from = out.len() - dist;
        for i in 0..len {
            let byte = out[from + i];
            out.push(byte);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_literals_and_matches() {
        // "abc" as literals, then a short match of 6 at distance 3, then a long match of
        // 4 at distance 1 (repeating the last byte).
        let input = [0x82, b'a', b'b', b'c', (6 - 3) << 2, 2, 0x40, 0, 0];
        let mut out = Vec::new();
        decode(Codec::Adc, &input, &mut out, 100).unwrap();
        assert_eq!(out, b"abcabcabccccc");
        // Output is capped.
        decode(Codec::Adc, &input, &mut out, 5).unwrap();
        assert_eq!(out, b"abcab");
        // A match before the start is an error, not a panic.
        assert!(decode(Codec::Adc, &[0x40, 0, 9], &mut out, 100).is_err());
        assert!(decode(Codec::Adc, &[0x85, 1], &mut out, 100).is_err());
    }

    #[test]
    fn capped_output() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 7) as u8).collect();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        io::Write::write_all(&mut z, &data).unwrap();
        let packed = z.finish().unwrap();
        let mut out = Vec::new();
        decode(Codec::Zlib, &packed, &mut out, 1000).unwrap();
        assert_eq!(out, &data[..1000]);
        decode(Codec::Zlib, &packed, &mut out, 1 << 20).unwrap();
        assert_eq!(out, data);
        assert!(decode(Codec::Zlib, &packed[..10], &mut out, 1 << 20).is_ok());
        assert!(out.len() < data.len());
        assert!(decode(Codec::Zlib, b"garbage!", &mut out, 1 << 20).is_err());
    }
}
