//! Disk image containers (DMG, VHD, VHDX, VMDK, QCOW2) and more compression and archive
//! formats (bzip2, lz4, lzma, 7z, tar), decoded to the raw disk they hold.
//!
//! | format | reads | refuses (with an explanation) |
//! |---|---|---|
//! | DMG (UDIF) | UDRW/UDRO (raw), UDCO (ADC), UDZO (zlib), UDBZ (bzip2), ULFO (LZFSE), ULMO (LZMA/xz); XML or resource-fork tables; CRC-32 checked | encrypted, sparse images and bundles, segmented |
//! | VHD | fixed, dynamic (sector bitmaps honoured) | differencing |
//! | VHDX | fixed, dynamic, 512/4096-byte sectors; a pending log is replayed in memory | differencing |
//! | VMDK | monolithicSparse, streamOptimized | descriptor-only (flat, split, VMFS), snapshots, COWD/SEsparse |
//! | QCOW2 | v2, v3, compressed clusters (deflate, zstd), zero clusters, extended L2 entries | backing files, encryption, external data files |
//! | bzip2 | multi-stream | |
//! | lz4 | frames (linked or not), skippable frames, legacy format | dictionaries |
//! | lzma | "LZMA alone", known or unknown size | |
//! | 7z | the first file: LZMA, LZMA2, PPMd, BZip2, Deflate, Zstd, LZ4, Copy, BCJ/BCJ2/ARM…/Delta | encryption |
//! | tar | the first regular file: ustar, GNU, pax, sparse files | |
//!
//! A tar inside bzip2, lz4, lzma or 7z gives its first file too (`untar` does the same for
//! the engine's gzip, xz, zstd and zip). Unallocated space reads as zeros everywhere.
//! Damaged files give errors, never panics or huge allocations: tables are read a window
//! at a time and every size from the file is checked before it's used.

mod bz2;
mod codec;
mod dmg;
mod lz4;
mod lzma;
mod qcow2;
mod sevenz;
mod source;
mod tar;
mod vdisk;
mod vhd;
mod vhdx;
mod vmdk;

#[cfg(test)]
mod tests;

use super::ImageFormat;
use source::{Source, read_full};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::Arc;

/// A format this module handles, as `detect` found it.
pub struct Detected {
    pub format: ImageFormat,
    /// The raw disk's size, when it's cheap to learn (virtual disks record it).
    pub raw_size: Option<u64>,
}

/// Recognises the formats this module handles. `head` is the start of the file (up to
/// 4 KiB); `file` may be read further (a trailer at the end, say) and needn't be rewound.
///
/// Returns None for everything else, gzip, xz, zstd and zip included. Call it before
/// looking for those: a DMG's data can start like xz (ULMO) or bzip2 (UDBZ).
pub fn detect(file: &mut File, size: u64, head: &[u8]) -> io::Result<Option<Detected>> {
    let src = Arc::new(Source::borrow(file, size)?);
    let found = |format, raw_size| Ok(Some(Detected { format, raw_size }));
    // Magic numbers at the start that nothing else has.
    if let Some(d) = vhdx::detect(&src, head) {
        return Ok(Some(d));
    }
    if let Some(d) = qcow2::detect(&src, head) {
        return Ok(Some(d));
    }
    if let Some(d) = vmdk::detect(&src, head) {
        return Ok(Some(d));
    }
    if let Some(d) = vhd::detect_head(head) {
        return Ok(Some(d));
    }
    // Trailers at the end: DMG's "koly", a fixed VHD's footer.
    if let Some(d) = dmg::detect(&src, head)? {
        return Ok(Some(d));
    }
    if let Some(d) = vhd::detect_tail(&src)? {
        return Ok(Some(d));
    }
    // Archives and compressed streams. Their raw size is the size of what they hold, or,
    // when that's a tar, of the tar's first file.
    if head.starts_with(sevenz::SIGNATURE) {
        let raw_size = sevenz::peek(&src)
            .ok()
            .and_then(|(size, first)| inner_size(&first, Some(size)));
        return found(ImageFormat::SevenZ, raw_size);
    }
    if tar::is_header(head) {
        let start = src.read_upto(0, 64 << 10)?;
        return found(ImageFormat::Tar, tar::probe(&start).flatten());
    }
    if bz2::detect(head) {
        let start = decoded_start(&src, ImageFormat::Bzip2);
        return found(ImageFormat::Bzip2, start.and_then(|s| inner_size(&s, None)));
    }
    if lz4::detect(&src, head) {
        let whole = lz4_size(&src);
        let start = decoded_start(&src, ImageFormat::Lz4);
        return found(ImageFormat::Lz4, start.and_then(|s| inner_size(&s, whole)));
    }
    // Last, as its magic number is weak: only a header that makes sense and a first
    // stretch that decodes.
    if let Some(start) = lzma::trial(&src, head) {
        let size = lzma::header(head).and_then(|h| h.size);
        return found(ImageFormat::Lzma, inner_size(&start, size));
    }
    Ok(None)
}

/// The raw size of a stream that starts with `start` and holds `whole` bytes: the first
/// file's size if it's a tar (None if its headers go past `start`), else `whole`.
fn inner_size(start: &[u8], whole: Option<u64>) -> Option<u64> {
    match tar::probe(start) {
        Some(size) => size,
        None => whole,
    }
}

/// The first 64 KiB (or less) that a compressed stream decodes to.
fn decoded_start(src: &Arc<Source>, format: ImageFormat) -> Option<Vec<u8>> {
    let mut stream = decoder(src, format).ok()?;
    let mut start = vec![0u8; 64 << 10];
    let n = read_full(&mut stream, &mut start).ok()?;
    start.truncate(n);
    Some(start)
}

/// A decoder for the stream formats (the whole file, from its start).
fn decoder(src: &Arc<Source>, format: ImageFormat) -> io::Result<Box<dyn Read + Send>> {
    let input = BufReader::with_capacity(1 << 20, src.clone().reader(0));
    Ok(match format {
        ImageFormat::Bzip2 => Box::new(bz2::Bz2::new(input)),
        ImageFormat::Lz4 => Box::new(lz4::Lz4::new(input)),
        other => return Err(io::Error::other(format!("{other:?} isn't a stream format"))),
    })
}

/// An LZ4 file's size once decoded, when every frame records its own.
fn lz4_size(src: &Source) -> Option<u64> {
    lz4::total_size(src)
}

/// The raw disk inside an image this module handles.
pub struct Opened {
    pub reader: Box<dyn Read + Send>,
    /// The raw disk's size, when the format says (virtual disks always do).
    pub size: Option<u64>,
}

/// Opens `path`, already detected as `format`, as a stream of the raw disk it holds.
pub fn open(path: &Path, format: ImageFormat) -> io::Result<Opened> {
    let src = Source::new(File::open(path)?)?;
    match format {
        ImageFormat::Dmg => dmg::open(src),
        ImageFormat::Vhd => vhd::open(src),
        ImageFormat::Vhdx => vhdx::open(src),
        ImageFormat::Vmdk => vmdk::open(src),
        ImageFormat::Qcow2 => qcow2::open(src),
        _ => open_stream(Arc::new(src), format),
    }
}

/// Archives and compressed streams.
fn open_stream(src: Arc<Source>, format: ImageFormat) -> io::Result<Opened> {
    match format {
        ImageFormat::Tar => {
            let input = BufReader::with_capacity(1 << 20, src.reader(0));
            let (reader, size) = untar_sized(Box::new(input))?;
            if size.is_none() {
                return Err(source::bad("this isn't a tar archive"));
            }
            Ok(Opened { reader, size })
        }
        ImageFormat::Bzip2 => match bz2::ParBz2::new(src.clone()) {
            Some(parallel) => inside(Box::new(parallel), None),
            None => inside(decoder(&src, format)?, None),
        },
        ImageFormat::Lz4 => {
            let size = lz4_size(&src);
            inside(decoder(&src, format)?, size)
        }
        ImageFormat::Lzma => {
            let head = src.read_upto(0, 14)?;
            let header =
                lzma::header(&head).ok_or_else(|| source::bad("the .lzma header is damaged"))?;
            let input = BufReader::with_capacity(1 << 20, src.reader(13));
            inside(Box::new(lzma::decoder(input, &header)?), header.size)
        }
        ImageFormat::SevenZ => {
            let (reader, size) = sevenz::open(&src)?;
            inside(reader, Some(size))
        }
        other => Err(io::Error::other(format!(
            "{other:?} images aren't decoded by this module"
        ))),
    }
}

/// What a decoded stream holds: the first file of a tar inside it, or itself.
fn inside(stream: Box<dyn Read + Send>, size: Option<u64>) -> io::Result<Opened> {
    let (reader, tar_size) = untar_sized(stream)?;
    Ok(Opened {
        reader,
        size: tar_size.or(size),
    })
}

/// If `stream` (e.g. a decompressed .tar.gz) holds a tar archive: the first regular file
/// in it. Otherwise `stream` itself, with nothing lost.
pub fn untar(stream: Box<dyn Read + Send>) -> io::Result<Box<dyn Read + Send>> {
    untar_sized(stream).map(|(reader, _)| reader)
}

/// `untar`, and the size of the file when it's a tar.
fn untar_sized(
    mut stream: Box<dyn Read + Send>,
) -> io::Result<(Box<dyn Read + Send>, Option<u64>)> {
    let mut head = vec![0u8; 512];
    let n = read_full(&mut stream, &mut head)?;
    head.truncate(n);
    if tar::is_header(&head) {
        let (reader, size) = tar::first_file(head, stream)?;
        return Ok((reader, Some(size)));
    }
    Ok((Box::new(io::Cursor::new(head).chain(stream)), None))
}
