//! Smart copies (only the extents the analysis found, onto a drive or into a smart image),
//! and writing zeros.

use super::disk::{AlignedBuf, Disk};
use super::image::{self, Frame, Map, Zeros};
use super::progress::Progress;
use super::why;
use crate::fmt;
use crate::smart::Extent;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Frames of a smart image hold up to this much used data.
pub const IMAGE_CHUNK: usize = image::DATA_MAX;

/// Returned by producers once the writer has stopped; the writer's own error explains why.
pub const STOPPED: &str = "the writer stopped";

/// `extents` in pieces of at most `chunk` bytes: (offset, length).
pub fn pieces(extents: &[Extent], chunk: usize) -> impl Iterator<Item = (u64, usize)> + '_ {
    extents.iter().flat_map(move |e| {
        let end = e.start + e.len;
        (e.start..end)
            .step_by(chunk)
            .map(move |off| (off, (end - off).min(chunk as u64) as usize))
    })
}

pub fn read_error(disk: &Disk, off: u64, err: &std::io::Error) -> String {
    format!(
        "couldn't read {} at {}: {}",
        disk.path.display(),
        fmt::bytes(off),
        why(err)
    )
}

fn write_error(disk: &Disk, off: u64, err: &std::io::Error) -> String {
    format!(
        "couldn't write to {} at {}: {}",
        disk.path.display(),
        fmt::bytes(off),
        why(err)
    )
}

/// Hands filled buffers to the writer thread of [`with_writer`].
pub struct Feed {
    pool: Receiver<AlignedBuf>,
    chunks: SyncSender<(u64, AlignedBuf, usize)>,
}

impl Feed {
    /// An empty buffer, once the writer is done with one. None once it has stopped.
    pub fn buffer(&self) -> Option<AlignedBuf> {
        self.pool.recv().ok()
    }

    /// Queues `len` bytes of `buf` for `off` on the target. False once the writer has stopped.
    pub fn send(&self, off: u64, buf: AlignedBuf, len: usize) -> bool {
        self.chunks.send((off, buf, len)).is_ok()
    }
}

/// Runs `produce` while a second thread writes what it produces to `dst`, so reading
/// (or decompressing) overlaps with writing. With `count_done`, progress counts the
/// written bytes as done too.
pub fn with_writer<T>(
    dst: &Disk,
    chunk: usize,
    progress: &Progress,
    count_done: bool,
    produce: impl FnOnce(&Feed) -> Result<T, String>,
) -> Result<T, String> {
    let buffers = ((64 << 20) / chunk).clamp(2, 8);
    thread::scope(|s| {
        let (pool_tx, pool_rx) = mpsc::channel();
        for _ in 0..buffers {
            let _ = pool_tx.send(AlignedBuf::new(chunk));
        }
        let (chunk_tx, chunk_rx) = mpsc::sync_channel::<(u64, AlignedBuf, usize)>(buffers);
        let writer = s.spawn(move || -> Result<(), String> {
            for (off, buf, len) in chunk_rx {
                if dst.is_drive && off + len as u64 > dst.size {
                    return Err(format!(
                        "The image doesn't fit on {}: it holds more than the drive's {}.",
                        dst.path.display(),
                        fmt::bytes(dst.size)
                    ));
                }
                dst.write_at(off, &buf[..len])
                    .map_err(|e| write_error(dst, off, &e))?;
                progress.add(if count_done { len as u64 } else { 0 }, len as u64);
                let _ = pool_tx.send(buf);
            }
            Ok(())
        });
        let feed = Feed {
            pool: pool_rx,
            chunks: chunk_tx,
        };
        let produced = produce(&feed);
        drop(feed);
        writer
            .join()
            .unwrap_or_else(|_| Err("the writer thread crashed".into()))?;
        produced
    })
}

/// Copies the extents from `src` to the same offsets on `dst`.
pub fn to_drive(
    src: &Disk,
    dst: &Disk,
    extents: &[Extent],
    chunk: usize,
    progress: &Progress,
) -> Result<(), String> {
    with_writer(dst, chunk, progress, true, |feed| {
        for (off, len) in pieces(extents, chunk) {
            let mut buf = feed.buffer().ok_or(STOPPED)?;
            src.read_at(off, &mut buf, len)
                .map_err(|e| read_error(src, off, &e))?;
            if !feed.send(off, buf, len) {
                return Err(STOPPED.into());
            }
        }
        Ok(())
    })
}

/// Writes `len` bytes of zeros to `dst`, from its start.
pub fn zeros(dst: &Disk, len: u64, chunk: usize, progress: &Progress) -> Result<(), String> {
    with_writer(dst, chunk, progress, true, |feed| {
        let mut off = 0;
        while off < len {
            let n = (len - off).min(chunk as u64) as usize;
            // The writer's buffers start out as zeros, and nothing here fills them.
            let buf = feed.buffer().ok_or(STOPPED)?;
            if !feed.send(off, buf, n) {
                return Err(STOPPED.into());
            }
            off += n as u64;
        }
        Ok(())
    })
}

enum Piece {
    /// A frame of `len` bytes of used data.
    Data { frame: Vec<u8>, len: usize },
    /// Free space.
    Free(u64),
}

/// Writes a smart image of `src` (see image.rs) to `out`.
///
/// A reader thread reads the used parts in order, a few threads compress them into
/// frames, and this thread writes the frames in order, with free space in between, and
/// the seek table at the end. Compressing takes longer for some data than for other, so
/// buffers go back to the reader as soon as they're compressed, and a slow frame holds up
/// only the writing: up to 2 frames per thread wait to be written.
pub fn to_image(src: &Disk, map: &Map, out: &Disk, progress: &Progress) -> Result<(), String> {
    let threads = thread::available_parallelism()
        .map_or(2, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 8);
    let write_err =
        |e: std::io::Error| format!("couldn't write to {}: {}", out.path.display(), why(&e));
    let zstd_err = |e: std::io::Error| format!("couldn't compress: {}", why(&e));
    thread::scope(|s| {
        let (pool_tx, pool_rx) = mpsc::channel();
        for _ in 0..threads + 2 {
            let _ = pool_tx.send(AlignedBuf::new(IMAGE_CHUNK));
        }
        // A permit per piece of used data that's read but not written yet.
        let permits = 2 * threads + 2;
        let (permit_tx, permit_rx) = mpsc::sync_channel::<()>(permits);
        for _ in 0..permits {
            let _ = permit_tx.send(());
        }
        // Frames' memory goes round too.
        let (frames_tx, frames_rx) = mpsc::channel::<Vec<u8>>();
        let frames_rx = Arc::new(Mutex::new(frames_rx));
        let (job_tx, job_rx) = mpsc::sync_channel::<(u64, AlignedBuf, usize)>(threads);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (piece_tx, piece_rx) = mpsc::channel::<(u64, Result<Piece, String>)>();

        let tx = piece_tx.clone();
        s.spawn(move || {
            let mut seq = 0;
            let mut pos = 0;
            for extent in &map.extents {
                if extent.start > pos {
                    if tx.send((seq, Ok(Piece::Free(extent.start - pos)))).is_err() {
                        return;
                    }
                    seq += 1;
                }
                for (off, len) in pieces(std::slice::from_ref(extent), IMAGE_CHUNK) {
                    let Ok(()) = permit_rx.recv() else { return };
                    let Ok(mut buf) = pool_rx.recv() else { return };
                    if let Err(e) = src.read_at(off, &mut buf, len) {
                        let _ = tx.send((seq, Err(read_error(src, off, &e))));
                        return;
                    }
                    if job_tx.send((seq, buf, len)).is_err() {
                        return;
                    }
                    seq += 1;
                }
                pos = extent.start + extent.len;
            }
            if map.size > pos {
                let _ = tx.send((seq, Ok(Piece::Free(map.size - pos))));
            }
        });

        for _ in 0..threads {
            let (tx, job_rx, frames_rx) = (piece_tx.clone(), job_rx.clone(), frames_rx.clone());
            let pool_tx = pool_tx.clone();
            s.spawn(move || {
                let mut compressor = match image::compressor() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send((0, Err(zstd_err(e))));
                        return;
                    }
                };
                loop {
                    let job = job_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok((seq, buf, len)) = job else { return };
                    let mut frame = frames_rx
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .try_recv()
                        .unwrap_or_default();
                    let piece = image::compress(&mut compressor, &buf[..len], &mut frame)
                        .map(|()| Piece::Data { frame, len })
                        .map_err(zstd_err);
                    let _ = pool_tx.send(buf);
                    if tx.send((seq, piece)).is_err() {
                        return;
                    }
                }
            });
        }
        drop((piece_tx, pool_tx));

        let mut w = BufWriter::with_capacity(1 << 20, out.file());
        let mut table = Vec::new();
        let mut put = |bytes: &[u8], len: u64, table: &mut Vec<Frame>| -> Result<u64, String> {
            w.write_all(bytes).map_err(write_err)?;
            // Frames never exceed 64 MiB, compressed or not.
            table.push(Frame {
                packed: bytes.len() as u32,
                len: len as u32,
            });
            Ok(bytes.len() as u64)
        };
        let first = image::map_frame(map);
        progress.add(0, put(&first, 0, &mut table)?);
        let mut zeros = Zeros::default();
        let mut pending = BTreeMap::new();
        let mut next = 0;
        for (seq, piece) in piece_rx {
            pending.insert(seq, piece?);
            while let Some(piece) = pending.remove(&next) {
                next += 1;
                match piece {
                    Piece::Free(len) => {
                        for piece in Zeros::pieces(len) {
                            let frame = zeros.frame(piece).map_err(zstd_err)?;
                            progress.add(0, put(frame, piece, &mut table)?);
                        }
                    }
                    Piece::Data { frame, len } => {
                        progress.add(len as u64, put(&frame, len as u64, &mut table)?);
                        let _ = frames_tx.send(frame);
                        let _ = permit_tx.send(());
                    }
                }
            }
        }
        if !pending.is_empty() {
            return Err("internal error: parts of the image went missing".into());
        }
        let seek_table = image::seek_table(&table);
        w.write_all(&seek_table).map_err(write_err)?;
        w.flush().map_err(write_err)?;
        progress.add(0, seek_table.len() as u64);
        Ok(())
    })
}
