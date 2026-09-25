//! Disk image containers (DMG, VHD, VHDX, VMDK, QCOW2) and more compression and archive
//! formats (bzip2, lz4, lzma, 7z, tar), decoded to the raw disk they hold.

use super::ImageFormat;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// A format this module handles, as `detect` found it.
pub struct Detected {
    pub format: ImageFormat,
    /// The raw disk's size, when it's cheap to learn (virtual disks record it).
    pub raw_size: Option<u64>,
}

/// Recognises the formats this module handles. `head` is the start of the file (up to
/// 4 KiB); `file` may be read further (a trailer at the end, say) and needn't be rewound.
pub fn detect(file: &mut File, size: u64, head: &[u8]) -> io::Result<Option<Detected>> {
    let _ = (file, size, head);
    Ok(None)
}

/// The raw disk inside an image this module handles.
pub struct Opened {
    pub reader: Box<dyn Read + Send>,
    /// The raw disk's size, when the format says (virtual disks always do).
    pub size: Option<u64>,
}

/// Opens `path`, already detected as `format`, as a stream of the raw disk it holds.
pub fn open(path: &Path, format: ImageFormat) -> io::Result<Opened> {
    let _ = path;
    Err(io::Error::other(format!("{format:?} images aren't supported yet")))
}

/// If `stream` (e.g. a decompressed .tar.gz) holds a tar archive: the first regular file
/// in it. Otherwise `stream` itself, with nothing lost.
pub fn untar(stream: Box<dyn Read + Send>) -> io::Result<Box<dyn Read + Send>> {
    Ok(stream)
}
