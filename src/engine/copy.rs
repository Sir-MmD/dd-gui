//! Smart copies: only the extents the analysis found, onto a drive or into a smart image.

use super::disk::{AlignedBuf, Disk};
use super::image::{self, Map, Zeros};
use super::progress::Progress;
use super::why;
use crate::fmt;
use crate::smart::Extent;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Members of a smart image hold up to this much data.
pub const IMAGE_CHUNK: usize = 4 << 20;

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

enum Piece {
    Data {
        deflated: Vec<u8>,
        crc: u32,
        buf: AlignedBuf,
        len: usize,
    },
    Free(u64),
}

/// Writes a smart image of `src` (see image.rs) to `out`.
///
/// A reader thread reads the used parts in order, a few threads compress them into
/// members, and this thread writes the members in order, with free space in between.
pub fn to_image(src: &Disk, map: &Map, out: &Disk, progress: &Progress) -> Result<(), String> {
    let threads = thread::available_parallelism()
        .map_or(2, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 8);
    let write_err =
        |e: std::io::Error| format!("couldn't write to {}: {}", out.path.display(), why(&e));
    thread::scope(|s| {
        let (pool_tx, pool_rx) = mpsc::channel();
        for _ in 0..threads + 4 {
            let _ = pool_tx.send(AlignedBuf::new(IMAGE_CHUNK));
        }
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
            let (tx, job_rx) = (piece_tx.clone(), job_rx.clone());
            s.spawn(move || {
                loop {
                    let job = job_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok((seq, buf, len)) = job else { return };
                    let (deflated, crc) = image::data_member(&buf[..len]);
                    if tx
                        .send((
                            seq,
                            Ok(Piece::Data {
                                deflated,
                                crc,
                                buf,
                                len,
                            }),
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        drop(piece_tx);

        let mut w = BufWriter::with_capacity(1 << 20, out.file());
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let first = image::map_member(map, mtime);
        w.write_all(&first).map_err(write_err)?;
        progress.add(0, first.len() as u64);
        let mut zeros = Zeros::default();
        let mut pending = BTreeMap::new();
        let mut next = 0;
        for (seq, piece) in piece_rx {
            pending.insert(seq, piece?);
            while let Some(piece) = pending.remove(&next) {
                next += 1;
                match piece {
                    Piece::Free(len) => {
                        let n = zeros.write(&mut w, len).map_err(write_err)?;
                        progress.add(0, n);
                    }
                    Piece::Data {
                        deflated,
                        crc,
                        buf,
                        len,
                    } => {
                        let n = image::write_member(&mut w, &deflated, crc, len as u64)
                            .map_err(write_err)?;
                        progress.add(len as u64, n);
                        let _ = pool_tx.send(buf);
                    }
                }
            }
        }
        w.flush().map_err(write_err)?;
        if !pending.is_empty() {
            return Err("internal error: parts of the image went missing".into());
        }
        Ok(())
    })
}
