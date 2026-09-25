//! Restores: raw images, DD-GUI smart images (zstd; gzip from version 1), plain gzip, xz,
//! zstd and zip (with a tar in any of them, too), and whatever `formats` reads: virtual
//! disks and more archive and compression formats. All decompressed on the fly.

use super::copy::{Feed, STOPPED, with_writer};
use super::disk::{AlignedBuf, Disk, DiskReader, is_drive};
use super::formats;
use super::image::{self, Cursor, Map};
use super::progress::Progress;
use super::{ImageFormat, ImageInfo, SmartInfo, why};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

/// An image to restore, looked at but not read yet.
pub struct Image {
    pub path: PathBuf,
    pub info: ImageInfo,
    /// The map of a DD-GUI smart image.
    pub map: Option<Map>,
    /// Bytes the map takes at the start of a version 2 smart image.
    head: u64,
    xz: Option<XzIndex>,
    /// A format from `formats`, once `open`ed.
    opened: Option<formats::Opened>,
}

impl Image {
    /// Decoded by `formats` rather than here?
    fn from_formats(&self) -> bool {
        !matches!(
            self.info.format,
            ImageFormat::Raw
                | ImageFormat::Gzip
                | ImageFormat::Xz
                | ImageFormat::Zstd
                | ImageFormat::Zip
        )
    }

    /// What `@progress` counts toward, when that's known: the used bytes of a smart image;
    /// the file's bytes for raw images, and for gzip, xz, zstd and zip, where progress
    /// counts the compressed bytes read; the raw disk's size for formats from `formats`,
    /// where progress counts the bytes they produce (None when they can't tell upfront).
    pub fn total(&self) -> Option<u64> {
        match &self.map {
            Some(map) => Some(map.used()),
            None if self.from_formats() => self.info.uncompressed,
            None => Some(self.info.size),
        }
    }

    /// How big a drive has to be to take the image, when that's known.
    pub fn needs(&self) -> Option<u64> {
        self.map.as_ref().map(Map::end).or(self.info.uncompressed)
    }

    /// Gets a format from `formats` ready to read, which may teach it the raw disk's size.
    /// (Other formats are opened by `restore`.)
    pub fn open(&mut self) -> io::Result<()> {
        if self.from_formats() && self.opened.is_none() {
            let opened = formats::open(&self.path, self.info.format)?;
            if opened.size.is_some() {
                self.info.uncompressed = opened.size;
            }
            self.opened = Some(opened);
        }
        Ok(())
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

pub fn format_of(head: &[u8]) -> ImageFormat {
    match head {
        [0x1f, 0x8b, ..] => ImageFormat::Gzip,
        [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => ImageFormat::Xz,
        // A zstd frame, or a skippable frame in front of one (pzstd and DD-GUI write those).
        [0x28, 0xb5, 0x2f, 0xfd, ..] | [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..] => ImageFormat::Zstd,
        [b'P', b'K', 0x03, 0x04, ..] => ImageFormat::Zip,
        _ => ImageFormat::Raw,
    }
}

/// Detects the format by its first bytes and learns what's cheap to learn: reads only
/// headers (the end of the file for xz, zip and zstd seek tables, the start of the data
/// for a tar inside). Damaged metadata just stays unknown; the restore reports it.
pub fn inspect(path: &Path) -> io::Result<Image> {
    let mut image = Image {
        path: path.into(),
        info: ImageInfo {
            format: ImageFormat::Raw,
            size: 0,
            uncompressed: None,
            smart: None,
        },
        map: None,
        head: 0,
        xz: None,
        opened: None,
    };
    if is_drive(path) {
        // A drive is a raw image of itself.
        let size = Disk::open_read(path)?.size;
        image.info.size = size;
        image.info.uncompressed = Some(size);
        return Ok(image);
    }
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    image.info.size = size;
    let mut head = Vec::with_capacity(4096);
    (&mut file).take(4096).read_to_end(&mut head)?;
    if let Some(found) = formats::detect(&mut file, size, &head)? {
        image.info.format = found.format;
        image.info.uncompressed = found.raw_size;
        return Ok(image);
    }
    let format = format_of(&head);
    image.info.format = format;
    file.seek(SeekFrom::Start(0))?;
    match format {
        ImageFormat::Raw => image.info.uncompressed = Some(size),
        ImageFormat::Gzip => {
            let header = image::read_header(&mut BufReader::new(&mut file));
            if let Some(Ok(map)) = header
                .ok()
                .and_then(|h| h.comment)
                .as_deref()
                .and_then(|c| Map::parse(c, image::MAGIC_V1))
            {
                image.set_map(map);
            }
        }
        ImageFormat::Xz => {
            image.xz = xz_index(&mut file, size).ok();
            image.info.uncompressed = image.xz.as_ref().map(|x| x.uncompressed);
        }
        ImageFormat::Zstd => match image::read_head(&mut BufReader::new(&mut file)) {
            Ok(Some(image::Head { map: Ok(map), len })) => {
                image.head = len;
                image.set_map(map);
            }
            // Not a smart image, or one whose map is damaged: plain zstd.
            _ => image.info.uncompressed = zstd_size(&mut file, size),
        },
        ImageFormat::Zip => {
            let mut archive = zip::ZipArchive::new(&mut file).ok();
            image.info.uncompressed = archive.as_mut().and_then(|a| {
                let index = first_file(a)?;
                a.by_index_raw(index).ok().map(|f| f.size())
            });
        }
        _ => unreachable!("format_of only finds the formats handled here"),
    }
    if image.map.is_none()
        && format != ImageFormat::Raw
        && let Some(size) = tar_inside(path, format)
    {
        // A .tar.gz and the like: what gets restored is the first file in the tar.
        image.info.uncompressed = Some(size);
    }
    Ok(image)
}

impl Image {
    fn set_map(&mut self, map: Map) {
        self.info.uncompressed = Some(map.size);
        self.info.smart = Some(SmartInfo {
            disk_size: map.size,
            used: map.used(),
        });
        self.map = Some(map);
    }
}

/// Writes the image to `dst`, from its start.
pub fn restore(
    image: &mut Image,
    dst: &Disk,
    chunk: usize,
    progress: &Arc<Progress>,
) -> Result<(), String> {
    let name = image.name();
    let unpack_err = |e: io::Error| format!("couldn't unpack {name}: {}", why(&e));
    let open_err = |e: io::Error| format!("couldn't open {}: {}", image.path.display(), why(&e));
    let send_all = |reader: &mut dyn Read, feed: &Feed| {
        pump(reader, feed, chunk).map_err(|e| e.map_or_else(|| STOPPED.to_owned(), unpack_err))
    };

    if let Some(map) = &image.map {
        with_writer(dst, chunk, progress, true, |feed| {
            if image.info.format == ImageFormat::Gzip {
                let file = File::open(&image.path).map_err(open_err)?;
                restore_smart_v1(
                    BufReader::with_capacity(1 << 20, file),
                    image.info.size,
                    map,
                    feed,
                    chunk,
                )
            } else {
                restore_smart_v2(&image.path, image.info.size, image.head, map, feed, chunk)
            }
        })?;
        if !dst.is_drive {
            // The file ends in free space: make it full size (sparse where the OS can).
            dst.set_len(map.size)
                .map_err(|e| format!("couldn't write to {}: {}", dst.path.display(), why(&e)))?;
        }
        return Ok(());
    }

    match image.info.format {
        ImageFormat::Raw => {
            with_writer(dst, chunk, progress, true, |feed| {
                if is_drive(&image.path) {
                    let drive = Disk::open_read(&image.path).map_err(open_err)?;
                    send_all(&mut DiskReader::new(&drive), feed)
                } else {
                    let file = File::open(&image.path).map_err(open_err)?;
                    send_all(&mut BufReader::with_capacity(1 << 20, file), feed)
                }
            })?;
        }
        format @ (ImageFormat::Gzip | ImageFormat::Xz | ImageFormat::Zstd | ImageFormat::Zip) => {
            // Progress counts the compressed bytes read.
            let file = File::open(&image.path).map_err(open_err)?;
            let input = Counting {
                inner: file,
                progress: progress.clone(),
            };
            let stream = decoder(format, input, image.xz.as_ref()).map_err(unpack_err)?;
            let mut stream = formats::untar(stream).map_err(unpack_err)?;
            with_writer(dst, chunk, progress, false, |feed| {
                send_all(&mut stream, feed)
            })?;
            // Formats with an index at the end, and tars, may leave some of the file unread.
            progress.done.store(image.info.size, Relaxed);
        }
        _ => {
            // Progress counts the raw bytes that come out: these formats read around in the
            // file, so the compressed bytes read say little.
            image.open().map_err(unpack_err)?;
            let formats::Opened { mut reader, size } =
                image.opened.take().expect("the image was just opened");
            let produced = with_writer(dst, chunk, progress, true, |feed| {
                send_all(&mut reader, feed)
            })?;
            if let Some(size) = size
                && produced != size
            {
                return Err(format!(
                    "couldn't unpack {name}: it gave {produced} bytes, where it says it holds {size}"
                ));
            }
        }
    }
    Ok(())
}

/// A stream of what's in an image of one of the formats decoded here (before any tar).
fn decoder<R: Read + Seek + Send + 'static>(
    format: ImageFormat,
    input: R,
    xz: Option<&XzIndex>,
) -> io::Result<Box<dyn Read + Send>> {
    Ok(match format {
        ImageFormat::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(
            BufReader::with_capacity(1 << 20, input),
        )),
        ImageFormat::Xz => match xz.map_or(1, xz_workers) {
            1 => Box::new(lzma_rust2::XzReader::new(
                BufReader::with_capacity(1 << 20, input),
                true,
            )),
            workers => Box::new(lzma_rust2::XzReaderMt::new(input, false, workers)?),
        },
        ImageFormat::Zstd => Box::new(zstd_decoder(BufReader::with_capacity(1 << 20, input))?),
        ImageFormat::Zip => zip_first_file(input)?,
        other => return Err(io::Error::other(format!("{other:?} isn't decoded here"))),
    })
}

/// Decodes every frame of a zstd file, skipping skippable ones.
fn zstd_decoder<R: BufRead>(input: R) -> io::Result<zstd::stream::read::Decoder<'static, R>> {
    let mut decoder = zstd::stream::read::Decoder::with_buffer(input)?;
    // Images made with `zstd --long` use windows up to 2 GiB; the default limit is 128 MiB.
    decoder.window_log_max(if cfg!(target_pointer_width = "64") {
        31
    } else {
        30
    })?;
    Ok(decoder)
}

/// Feeds everything `reader` gives to the writer; returns how much. Err(None) when the
/// writer stopped.
fn pump(reader: &mut dyn Read, feed: &Feed, chunk: usize) -> Result<u64, Option<io::Error>> {
    let mut pos = 0u64;
    loop {
        let mut buf = feed.buffer().ok_or(None)?;
        let n = read_full(reader, &mut buf[..chunk]).map_err(Some)?;
        if n == 0 {
            return Ok(pos);
        }
        if !feed.send(pos, buf, n) {
            return Err(None);
        }
        pos += n as u64;
    }
}

fn damaged(what: &str) -> String {
    format!("the smart image is damaged ({what})")
}

fn smart_read_error(e: io::Error) -> String {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => damaged(&why(&e)),
        _ => format!("couldn't read the image: {}", why(&e)),
    }
}

/// A frame of a version 2 smart image that holds used data.
#[derive(Clone, Copy)]
struct Job {
    /// Where the frame starts in the file, and its size there.
    at: u64,
    packed: usize,
    /// Where its content goes on the drive, and how much of it there is.
    pos: u64,
    len: usize,
}

/// A frame on its way through the decoders.
#[derive(Default)]
struct Slot {
    packed: Vec<u8>,
    data: Vec<u8>,
}

/// A version 2 smart image: frames of free space are skipped (the seek table says where
/// each frame starts), those with used data are decompressed on a few threads, and only
/// the used data goes to the writer.
fn restore_smart_v2(
    path: &Path,
    file_size: u64,
    head: u64,
    map: &Map,
    feed: &Feed,
    chunk: usize,
) -> Result<(), String> {
    let mut file = File::open(path).map_err(smart_read_error)?;
    let (frames, table_at) =
        image::read_seek_table(&mut file, file_size).map_err(smart_read_error)?;
    // The table has to add up: the map's frame first, then frames covering the drive.
    let packed: u64 = frames.iter().map(|f| f.packed as u64).sum();
    let unpacked: u64 = frames.iter().map(|f| f.len as u64).sum();
    if frames
        .first()
        .is_none_or(|f| f.len != 0 || f.packed as u64 != head)
        || packed != table_at
        || unpacked != map.size
    {
        return Err(damaged("its seek table doesn't match its content"));
    }
    let mut jobs = Vec::new();
    let mut cursor = Cursor::new(map);
    let (mut at, mut pos) = (0u64, 0u64);
    for f in &frames {
        let (packed, len) = (f.packed as u64, f.len as u64);
        if len > 0 && !cursor.is_free(pos, len) {
            // DD-GUI's frames stay within 64 MiB, which also bounds the memory used here.
            if len > image::ZEROS_MAX || packed > 2 * image::ZEROS_MAX {
                return Err(damaged("a frame is too big"));
            }
            jobs.push(Job {
                at,
                packed: packed as usize,
                pos,
                len: len as usize,
            });
        }
        at += packed;
        pos += len;
    }

    let threads = thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 4);
    thread::scope(|s| {
        // Slots go round: reader → decoders → here → reader. Their number bounds the memory.
        let (pool_tx, pool_rx) = mpsc::channel::<Slot>();
        for _ in 0..2 * threads + 2 {
            let _ = pool_tx.send(Slot::default());
        }
        let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Slot)>(threads);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (done_tx, done_rx) = mpsc::channel::<(usize, Result<Slot, String>)>();
        let jobs = &jobs;

        let failed = done_tx.clone();
        s.spawn(move || {
            let mut input = BufReader::with_capacity(1 << 20, file);
            // Reading the seek table left the file at its end.
            let mut at = 0u64;
            let mut rewound = input.seek(SeekFrom::Start(0)).map(|_| ());
            for (seq, job) in jobs.iter().enumerate() {
                let Ok(mut slot) = pool_rx.recv() else { return };
                slot.packed.resize(job.packed, 0);
                let read = std::mem::replace(&mut rewound, Ok(()))
                    .and_then(|()| input.seek_relative((job.at - at) as i64))
                    .and_then(|()| input.read_exact(&mut slot.packed));
                if let Err(e) = read {
                    let _ = failed.send((seq, Err(smart_read_error(e))));
                    return;
                }
                at = job.at + job.packed as u64;
                if job_tx.send((seq, slot)).is_err() {
                    return;
                }
            }
        });

        for _ in 0..threads {
            let (job_rx, done_tx) = (job_rx.clone(), done_tx.clone());
            s.spawn(move || {
                let mut decompressor = match zstd::bulk::Decompressor::new() {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = done_tx.send((0, Err(format!("couldn't decompress: {}", why(&e)))));
                        return;
                    }
                };
                loop {
                    let next = job_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok((seq, mut slot)) = next else { return };
                    let len = jobs[seq].len;
                    slot.data.clear();
                    slot.data.reserve(len);
                    let result =
                        match decompressor.decompress_to_buffer(&slot.packed, &mut slot.data) {
                            Ok(n) if n == len => Ok(slot),
                            Ok(_) => Err(damaged("a frame holds less than its size says")),
                            Err(e) => Err(damaged(&why(&e))),
                        };
                    if done_tx.send((seq, result)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(done_tx);

        let mut out = Staging {
            feed,
            buf: None,
            start: 0,
            len: 0,
            chunk,
        };
        let mut cursor = Cursor::new(map);
        let mut pending = BTreeMap::new();
        let mut next = 0;
        for (seq, slot) in done_rx {
            pending.insert(seq, slot?);
            while let Some(slot) = pending.remove(&next) {
                let mut pos = jobs[next].pos;
                next += 1;
                let data = &slot.data[..];
                let mut at = 0;
                while at < data.len() {
                    let (used, run) = cursor.run(pos, (data.len() - at) as u64);
                    let run = run as usize;
                    if used {
                        out.put(pos, &data[at..at + run])?;
                    }
                    at += run;
                    pos += run as u64;
                }
                let _ = pool_tx.send(slot);
            }
        }
        if next != jobs.len() {
            return Err("internal error: parts of the image went missing".into());
        }
        out.flush()
    })
}

/// A version 1 smart image (gzip): members of free space are skipped (using their `DG`
/// sizes) without decompressing them, and only used data goes to the writer.
fn restore_smart_v1(
    mut input: BufReader<File>,
    file_size: u64,
    map: &Map,
    feed: &Feed,
    chunk: usize,
) -> Result<(), String> {
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
    while !input.fill_buf().map_err(smart_read_error)?.is_empty() {
        let header = image::read_header(&mut input).map_err(smart_read_error)?;
        if let Some((csize, len)) = header.sizes
            && len > 0
            && csize >= header.len + 8
            && pos + len <= map.size
            && cursor.is_free(pos, len)
        {
            input
                .seek_relative((csize - header.len) as i64)
                .map_err(smart_read_error)?;
            pos += len;
            continue;
        }
        let mut crc = flate2::Crc::new();
        let mut decoder = flate2::bufread::DeflateDecoder::new(&mut input);
        loop {
            let n = decoder.read(&mut scratch).map_err(smart_read_error)?;
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
        input.read_exact(&mut trailer).map_err(smart_read_error)?;
        if trailer[..4] != crc.sum().to_le_bytes() || trailer[4..] != crc.amount().to_le_bytes() {
            return Err(damaged("checksum mismatch"));
        }
    }
    out.flush()?;
    let read_to = input.stream_position().map_err(smart_read_error)?;
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
struct Counting<R> {
    inner: R,
    progress: Arc<Progress>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.done.fetch_add(n as u64, Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<R> {
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

/// The first entry that is a regular file with something in it: folders, symbolic links
/// and empty files are skipped (as `formats` does for tar and 7z).
fn first_file<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Option<usize> {
    (0..archive.len()).find(|&i| {
        archive
            .by_index_raw(i)
            .is_ok_and(|f| f.is_file() && f.size() > 0)
    })
}

fn zip_error(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// The first file in a zip archive, as a stream of its own. (The zip crate's entries borrow
/// their archive, so a thread keeps the archive and hands the data over.)
fn zip_first_file<R: Read + Seek + Send + 'static>(input: R) -> io::Result<Box<dyn Read + Send>> {
    let mut archive = zip::ZipArchive::new(input).map_err(zip_error)?;
    let index = first_file(&mut archive).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "the zip archive holds no files")
    })?;
    // Unsupported methods and encryption show up here, before any writing.
    drop(archive.by_index(index).map_err(zip_error)?);
    Ok(Box::new(Piped::spawn(move |tx| {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        loop {
            let mut chunk = vec![0u8; 1 << 20];
            let n = read_full(&mut entry, &mut chunk)?;
            if n == 0 {
                return Ok(());
            }
            chunk.truncate(n);
            if tx.send(Ok(chunk)).is_err() {
                return Ok(()); // nobody reads any more
            }
        }
    })))
}

/// A stream made on another thread, which passes it on in chunks.
struct Piped {
    rx: Receiver<io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    at: usize,
}

impl Piped {
    fn spawn(
        work: impl FnOnce(&SyncSender<io::Result<Vec<u8>>>) -> io::Result<()> + Send + 'static,
    ) -> Piped {
        let (tx, rx) = mpsc::sync_channel(4);
        thread::spawn(move || {
            if let Err(e) = work(&tx) {
                let _ = tx.send(Err(e));
            }
        });
        Piped {
            rx,
            chunk: Vec::new(),
            at: 0,
        }
    }
}

impl Read for Piped {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => (self.chunk, self.at) = (chunk, 0),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0), // all passed on
            }
        }
        let n = buf.len().min(self.chunk.len() - self.at);
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

// ---- Sizes, learned cheaply -------------------------------------------------------------

/// A zstd file's size once decompressed, when that's cheap to learn: from its seek table
/// (zstd's seekable format), or else from the first frame's header (the zstd tool writes
/// one frame per file).
fn zstd_size(file: &mut File, size: u64) -> Option<u64> {
    if let Ok((frames, _)) = image::read_seek_table(file, size) {
        return Some(frames.iter().map(|f| f.len as u64).sum());
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    zstd_content_size(&mut BufReader::new(file)).ok().flatten()
}

/// The content size in the first zstd frame's header, if it has one.
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

/// When an image of one of the formats decoded here holds a tar archive: the size of the
/// first regular file in it (what `formats::untar` gives, and what gets restored).
fn tar_inside(path: &Path, format: ImageFormat) -> Option<u64> {
    let file = File::open(path).ok()?;
    tar_first_file(decoder(format, file, None).ok()?)
}

/// The size of the first regular file in a tar archive, from its headers. None when
/// `stream` doesn't start with a tar archive. Reads a few headers at most.
pub(super) fn tar_first_file(mut stream: impl Read) -> Option<u64> {
    let mut block = [0u8; 512];
    // A pax header's size (or real size, for a sparse file), for the entry after it.
    let mut pax = None;
    // What may be read of entries before the first file (folders, long names, pax data).
    let mut budget = 1u64 << 20;
    for _ in 0..64 {
        stream.read_exact(&mut block).ok()?;
        if block.iter().all(|&b| b == 0) || !tar_checksum_ok(&block) {
            return None;
        }
        let size = tar_number(&block[124..136])?;
        match block[156] {
            b'0' | 0 | b'7' => return Some(pax.unwrap_or(size)),
            // An old GNU sparse file: its real size is further down the header.
            b'S' => return pax.or_else(|| tar_number(&block[483..495])),
            kind => {
                let padded = size.div_ceil(512) * 512;
                budget = budget.checked_sub(padded)?;
                let mut data = Vec::new();
                (&mut stream).take(padded).read_to_end(&mut data).ok()?;
                if (data.len() as u64) < padded {
                    return None;
                }
                match kind {
                    b'x' => pax = pax_size(&data[..size as usize]),
                    // Global pax records and GNU long names don't end an entry.
                    b'g' | b'L' | b'K' => {}
                    _ => pax = None,
                }
            }
        }
    }
    None
}

/// The `size` in pax records (`<length> <key>=<value>\n`), or the real size of a sparse file.
fn pax_size(records: &[u8]) -> Option<u64> {
    let (mut size, mut real) = (None, None);
    let mut rest = records;
    while !rest.is_empty() {
        let space = rest.iter().position(|&b| b == b' ')?;
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        if len <= space + 1 || len > rest.len() {
            return None;
        }
        let record = &rest[space + 1..len - 1];
        if let Some(eq) = record.iter().position(|&b| b == b'=') {
            let value = std::str::from_utf8(&record[eq + 1..])
                .ok()
                .and_then(|v| v.parse::<u64>().ok());
            match &record[..eq] {
                b"size" => size = value,
                b"GNU.sparse.realsize" | b"GNU.sparse.size" => real = value,
                _ => {}
            }
        }
        rest = &rest[len..];
    }
    real.or(size)
}

/// A number in a tar header: octal text, or (GNU) base-256 with the top bit set.
fn tar_number(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        if field[0] == 0xff {
            return None; // negative
        }
        return field[1..]
            .iter()
            .try_fold((field[0] & 0x7f) as u64, |v, &b| {
                v.checked_mul(256)?.checked_add(b as u64)
            });
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

fn tar_checksum_ok(block: &[u8; 512]) -> bool {
    let Some(want) = tar_number(&block[148..156]) else {
        return false;
    };
    let field = 148..156;
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &b) in block.iter().enumerate() {
        let b = if field.contains(&i) { b' ' } else { b };
        unsigned += b as u64;
        signed += b as i8 as i64;
    }
    want == unsigned || want as i64 == signed
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
