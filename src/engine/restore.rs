//! Restores: raw images, gzip (DD-GUI smart images included), xz, zstd and zip, all
//! decompressed on the fly by pure-Rust decoders.

use super::copy::{Feed, STOPPED, with_writer};
use super::disk::{AlignedBuf, Disk, DiskReader, is_drive};
use super::image::{self, Cursor, Map};
use super::progress::Progress;
use super::{ImageFormat, ImageInfo, SmartInfo, why};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

/// An image to restore, looked at but not read yet.
pub struct Image {
    pub path: PathBuf,
    pub info: ImageInfo,
    /// The map of a DD-GUI smart image.
    pub map: Option<Map>,
    xz: Option<XzIndex>,
}

impl Image {
    /// What `@progress` counts toward: used bytes of a smart image, else bytes of the file.
    pub fn total(&self) -> u64 {
        self.map.as_ref().map_or(self.info.size, Map::used)
    }

    /// How big a drive has to be to take the image, when that's known.
    pub fn needs(&self) -> Option<u64> {
        self.map.as_ref().map(Map::end).or(self.info.uncompressed)
    }
}

pub fn format_of(head: &[u8]) -> ImageFormat {
    match head {
        [0x1f, 0x8b, ..] => ImageFormat::Gzip,
        [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => ImageFormat::Xz,
        // A zstd frame, or a skippable frame in front of one (pzstd writes those).
        [0x28, 0xb5, 0x2f, 0xfd, ..] | [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..] => ImageFormat::Zstd,
        [b'P', b'K', 0x03, 0x04, ..] => ImageFormat::Zip,
        _ => ImageFormat::Raw,
    }
}

/// Detects the format by its magic bytes and learns what's cheap to learn: reads only
/// headers (the end of the file for xz and zip). Damaged metadata just stays unknown;
/// the restore reports those errors.
pub fn inspect(path: &Path) -> io::Result<Image> {
    if is_drive(path) {
        // A drive is a raw image of itself.
        let size = Disk::open_read(path)?.size;
        let info = ImageInfo {
            format: ImageFormat::Raw,
            size,
            uncompressed: Some(size),
            smart: None,
        };
        return Ok(Image {
            path: path.into(),
            info,
            map: None,
            xz: None,
        });
    }
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = [0u8; 6];
    let n = read_full(&mut file, &mut head)?;
    let format = format_of(&head[..n]);
    file.seek(SeekFrom::Start(0))?;
    let mut image = Image {
        path: path.into(),
        info: ImageInfo {
            format,
            size,
            uncompressed: None,
            smart: None,
        },
        map: None,
        xz: None,
    };
    match format {
        ImageFormat::Raw => image.info.uncompressed = Some(size),
        ImageFormat::Gzip => {
            let header = image::read_header(&mut BufReader::new(&mut file));
            if let Some(Ok(map)) = header
                .ok()
                .and_then(|h| h.comment)
                .as_deref()
                .and_then(Map::parse)
            {
                image.info.uncompressed = Some(map.size);
                image.info.smart = Some(SmartInfo {
                    disk_size: map.size,
                    used: map.used(),
                });
                image.map = Some(map);
            }
        }
        ImageFormat::Xz => {
            image.xz = xz_index(&mut file, size).ok();
            image.info.uncompressed = image.xz.as_ref().map(|x| x.uncompressed);
        }
        ImageFormat::Zstd => {
            image.info.uncompressed = zstd_content_size(&mut BufReader::new(&mut file))
                .ok()
                .flatten()
        }
        ImageFormat::Zip => {
            let mut archive = zip::ZipArchive::new(&mut file).ok();
            image.info.uncompressed = archive.as_mut().and_then(|a| {
                let index = first_file(a)?;
                a.by_index_raw(index).ok().map(|f| f.size())
            });
        }
        // Formats from `formats` are detected there; nothing cheap to learn here yet.
        _ => {}
    }
    Ok(image)
}

/// Writes the image to `dst`, from its start.
pub fn restore(image: &Image, dst: &Disk, chunk: usize, progress: &Progress) -> Result<(), String> {
    let file = File::open(&image.path)
        .map_err(|e| format!("couldn't open {}: {}", image.path.display(), why(&e)))?;
    let name = image
        .path
        .file_name()
        .unwrap_or(image.path.as_os_str())
        .to_string_lossy()
        .into_owned();
    let unpack_err = |e: io::Error| format!("couldn't unpack {name}: {}", why(&e));
    if let Some(map) = &image.map {
        with_writer(dst, chunk, progress, true, |feed| {
            restore_smart(
                BufReader::with_capacity(1 << 20, file),
                image.info.size,
                map,
                feed,
                chunk,
            )
        })?;
        if !dst.is_drive {
            // The file ends in free space: make it full size (sparse where the OS can).
            dst.set_len(map.size)
                .map_err(|e| format!("couldn't write to {}: {}", dst.path.display(), why(&e)))?;
        }
        return Ok(());
    }
    let format = image.info.format;
    let compressed = format != ImageFormat::Raw;
    let input = Counting {
        inner: file,
        progress: compressed.then_some(progress),
    };
    with_writer(dst, chunk, progress, !compressed, |feed| {
        let send_all = |reader: &mut dyn Read| {
            pump(reader, feed, chunk).map_err(|e| e.map_or_else(|| STOPPED.to_owned(), unpack_err))
        };
        match format {
            ImageFormat::Raw if is_drive(&image.path) => {
                let drive = Disk::open_read(&image.path).map_err(unpack_err)?;
                send_all(&mut DiskReader::new(&drive))
            }
            ImageFormat::Raw => send_all(&mut BufReader::with_capacity(1 << 20, input)),
            ImageFormat::Gzip => send_all(&mut flate2::bufread::MultiGzDecoder::new(
                BufReader::with_capacity(1 << 20, input),
            )),
            ImageFormat::Xz => match image.xz.as_ref().map_or(1, xz_workers) {
                1 => send_all(&mut lzma_rust2::XzReader::new(
                    BufReader::with_capacity(1 << 20, input),
                    true,
                )),
                workers => send_all(
                    &mut lzma_rust2::XzReaderMt::new(input, false, workers).map_err(unpack_err)?,
                ),
            },
            ImageFormat::Zstd => send_all(&mut ZstdReader::new(BufReader::with_capacity(
                1 << 20,
                input,
            ))),
            ImageFormat::Zip => {
                let zip_err = |e: zip::result::ZipError| format!("couldn't unpack {name}: {e}");
                let mut archive = zip::ZipArchive::new(input).map_err(zip_err)?;
                let index =
                    first_file(&mut archive).ok_or_else(|| format!("{name} holds no files"))?;
                send_all(&mut archive.by_index(index).map_err(zip_err)?)
            }
            other => send_all(&mut super::formats::open(&image.path, other).map_err(unpack_err)?.reader),
        }
    })?;
    if compressed {
        // Formats with an index at the end may leave some of the file unread.
        progress.done.store(image.info.size, Relaxed);
    }
    Ok(())
}

/// Feeds everything `reader` gives to the writer. Err(None) when the writer stopped.
fn pump(reader: &mut dyn Read, feed: &Feed, chunk: usize) -> Result<(), Option<io::Error>> {
    let mut pos = 0u64;
    loop {
        let mut buf = feed.buffer().ok_or(None)?;
        let n = read_full(reader, &mut buf[..chunk]).map_err(Some)?;
        if n == 0 {
            return Ok(());
        }
        if !feed.send(pos, buf, n) {
            return Err(None);
        }
        pos += n as u64;
    }
}

/// A smart image: members of free space are skipped (using their `DG` sizes) without
/// decompressing them, and only used data goes to the writer.
fn restore_smart(
    mut input: BufReader<File>,
    file_size: u64,
    map: &Map,
    feed: &Feed,
    chunk: usize,
) -> Result<(), String> {
    let damaged = |what: &str| format!("the smart image is damaged ({what})");
    let io_err = |e: io::Error| match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => damaged(&why(&e)),
        _ => format!("couldn't read the image: {}", why(&e)),
    };
    let mut cursor = Cursor::new(map);
    let mut out = Staging {
        feed,
        buf: None,
        start: 0,
        len: 0,
        chunk,
    };
    let mut scratch = vec![0u8; 256 << 10];
    let mut pos = 0u64;
    while !input.fill_buf().map_err(io_err)?.is_empty() {
        let header = image::read_header(&mut input).map_err(io_err)?;
        if let Some((csize, len)) = header.sizes
            && len > 0
            && csize >= header.len + 8
            && pos + len <= map.size
            && cursor.is_free(pos, len)
        {
            input
                .seek_relative((csize - header.len) as i64)
                .map_err(io_err)?;
            pos += len;
            continue;
        }
        let mut crc = flate2::Crc::new();
        let mut decoder = flate2::bufread::DeflateDecoder::new(&mut input);
        loop {
            let n = decoder.read(&mut scratch).map_err(io_err)?;
            if n == 0 {
                break;
            }
            crc.update(&scratch[..n]);
            let mut at = 0;
            while at < n {
                let (used, run) = cursor.run(pos, (n - at) as u64);
                let run = run as usize;
                if used {
                    out.put(pos, &scratch[at..at + run])?;
                }
                at += run;
                pos += run as u64;
            }
        }
        drop(decoder);
        let mut trailer = [0u8; 8];
        input.read_exact(&mut trailer).map_err(io_err)?;
        if trailer[..4] != crc.sum().to_le_bytes() || trailer[4..] != crc.amount().to_le_bytes() {
            return Err(damaged("checksum mismatch"));
        }
    }
    out.flush()?;
    let read_to = input.stream_position().map_err(io_err)?;
    if pos != map.size || read_to != file_size {
        return Err(damaged("it ends too early or too late"));
    }
    Ok(())
}

/// Gathers the used bytes of a smart image into buffers of contiguous data.
struct Staging<'a> {
    feed: &'a Feed,
    buf: Option<AlignedBuf>,
    /// Where `buf` goes on the target, and how full it is.
    start: u64,
    len: usize,
    chunk: usize,
}

impl Staging<'_> {
    fn put(&mut self, mut pos: u64, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            if self.buf.is_some() && (pos != self.start + self.len as u64 || self.len == self.chunk)
            {
                self.flush()?;
            }
            if self.buf.is_none() {
                self.buf = Some(self.feed.buffer().ok_or(STOPPED)?);
                (self.start, self.len) = (pos, 0);
            }
            let buf = self.buf.as_mut().expect("a buffer");
            let n = data.len().min(self.chunk - self.len);
            buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            pos += n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if let Some(buf) = self.buf.take()
            && !self.feed.send(self.start, buf, self.len)
        {
            return Err(STOPPED.into());
        }
        Ok(())
    }
}

/// Counts compressed bytes as they're read, for progress.
struct Counting<'a, R> {
    inner: R,
    progress: Option<&'a Progress>,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(progress) = self.progress {
            progress.done.fetch_add(n as u64, Relaxed);
        }
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

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

/// The first entry that is a file (not a folder).
fn first_file<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Option<usize> {
    (0..archive.len()).find(|&i| archive.by_index_raw(i).is_ok_and(|f| !f.is_dir()))
}

/// Decodes every frame of a zstd file (the ruzstd decoder does one at a time).
struct ZstdReader<R> {
    src: R,
    decoder: ruzstd::decoding::FrameDecoder,
    in_frame: bool,
}

impl<R: BufRead> ZstdReader<R> {
    fn new(src: R) -> Self {
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        // Images made with `zstd --long` use windows up to 2 GiB. The default limit is 100 MB.
        decoder.set_max_window_size(1 << 31);
        Self {
            src,
            decoder,
            in_frame: false,
        }
    }
}

impl<R: BufRead> Read for ZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        let bad = |e: FrameDecoderError| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.in_frame {
                if self.src.fill_buf()?.is_empty() {
                    return Ok(0);
                }
                match self.decoder.reset(&mut self.src) {
                    Ok(()) => self.in_frame = true,
                    Err(FrameDecoderError::ReadFrameHeaderError(
                        ReadFrameHeaderError::SkipFrame { length, .. },
                    )) => {
                        let skipped =
                            io::copy(&mut (&mut self.src).take(length as u64), &mut io::sink())?;
                        if skipped < length as u64 {
                            return Err(io::ErrorKind::UnexpectedEof.into());
                        }
                        continue;
                    }
                    Err(e) => return Err(bad(e)),
                }
            }
            let decoder = &mut self.decoder;
            while decoder.can_collect() < buf.len() && !decoder.is_finished() {
                let wanted = buf.len() - decoder.can_collect();
                decoder
                    .decode_blocks(
                        &mut self.src,
                        ruzstd::decoding::BlockDecodingStrategy::UptoBytes(wanted),
                    )
                    .map_err(bad)?;
            }
            let n = decoder.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if decoder.is_finished() {
                if let (Some(stored), Some(computed)) = (
                    decoder.get_checksum_from_data(),
                    decoder.get_calculated_checksum(),
                ) && stored != computed
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "zstd checksum mismatch",
                    ));
                }
                self.in_frame = false;
            }
        }
    }
}

/// The content size in the first zstd frame's header, if it has one. (The zstd tool
/// writes one frame per file.)
fn zstd_content_size(r: &mut impl Read) -> io::Result<Option<u64>> {
    loop {
        let mut magic = [0u8; 4];
        if read_full(r, &mut magic)? < 4 {
            return Ok(None);
        }
        match u32::from_le_bytes(magic) {
            m if m & 0xffff_fff0 == 0x184d_2a50 => {
                let mut len = [0u8; 4];
                r.read_exact(&mut len)?;
                io::copy(&mut r.take(u32::from_le_bytes(len) as u64), &mut io::sink())?;
            }
            0xfd2f_b528 => {
                let mut descriptor = [0u8; 1];
                r.read_exact(&mut descriptor)?;
                let d = descriptor[0];
                let single_segment = d & 0x20 != 0;
                let size_len = match d >> 6 {
                    0 if single_segment => 1,
                    0 => return Ok(None),
                    1 => 2,
                    2 => 4,
                    _ => 8,
                };
                let skip = usize::from(!single_segment) + [0, 1, 2, 4][(d & 3) as usize];
                let mut fields = [0u8; 13];
                r.read_exact(&mut fields[..skip + size_len])?;
                let mut size = [0u8; 8];
                size[..size_len].copy_from_slice(&fields[skip..skip + size_len]);
                let size = u64::from_le_bytes(size);
                return Ok(Some(if size_len == 2 { size + 256 } else { size }));
            }
            _ => return Ok(None),
        }
    }
}

/// What an xz file's index says.
#[derive(Debug, Default)]
pub struct XzIndex {
    pub uncompressed: u64,
    pub streams: u32,
    pub blocks: u64,
    /// The biggest block, uncompressed and compressed (for the threaded decoder's memory).
    pub max_block: u64,
    pub max_block_packed: u64,
}

/// Reads the index of every stream, from the end of the file backwards.
pub fn xz_index(f: &mut (impl Read + Seek), size: u64) -> io::Result<XzIndex> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "invalid xz index");
    let mut index = XzIndex::default();
    let mut end = size;
    while end > 0 {
        let mut footer = [0u8; 12];
        if end < 12 {
            return Err(bad());
        }
        f.seek(SeekFrom::Start(end - 12))?;
        f.read_exact(&mut footer)?;
        if footer[8..] == [0; 4] {
            end -= 4; // stream padding
            continue;
        }
        if footer[10..] != *b"YZ" {
            return Err(bad());
        }
        let index_size = (u32::from_le_bytes(footer[4..8].try_into().unwrap()) as u64 + 1) * 4;
        let index_start = end
            .checked_sub(12 + index_size)
            .filter(|_| index_size <= 1 << 30)
            .ok_or_else(bad)?;
        let mut data = vec![0u8; index_size as usize];
        f.seek(SeekFrom::Start(index_start))?;
        f.read_exact(&mut data)?;
        let mut at = 1;
        let records = vli(&data, &mut at)
            .filter(|_| data[0] == 0)
            .ok_or_else(bad)?;
        let mut packed = 0u64;
        for _ in 0..records {
            let unpadded = vli(&data, &mut at).ok_or_else(bad)?;
            let uncompressed = vli(&data, &mut at).ok_or_else(bad)?;
            let block = unpadded.div_ceil(4) * 4;
            packed = packed.checked_add(block).ok_or_else(bad)?;
            index.uncompressed = index
                .uncompressed
                .checked_add(uncompressed)
                .ok_or_else(bad)?;
            index.max_block = index.max_block.max(uncompressed);
            index.max_block_packed = index.max_block_packed.max(block);
        }
        index.blocks += records;
        index.streams += 1;
        end = index_start.checked_sub(packed + 12).ok_or_else(bad)?;
        let mut magic = [0u8; 6];
        f.seek(SeekFrom::Start(end))?;
        f.read_exact(&mut magic)?;
        if magic != [0xfd, b'7', b'z', b'X', b'Z', 0] {
            return Err(bad());
        }
    }
    Ok(index)
}

/// A variable-length integer of the xz format.
fn vli(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *data.get(*at)?;
        *at += 1;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Threads for decoding xz: files with several blocks (as `xz -T0` makes them) decode in
/// parallel, within about 512 MiB of memory.
fn xz_workers(index: &XzIndex) -> u32 {
    if index.streams != 1 || index.blocks < 2 {
        return 1;
    }
    let per_worker = (index.max_block + index.max_block_packed).max(1);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    ((512 << 20) / per_worker).min(cpus).clamp(1, 4) as u32
}
