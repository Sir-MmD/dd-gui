//! Virtual disks (DMG, VHD, VHDX, VMDK, QCOW2) read from start to end.
//!
//! Each format describes its disk as a series of pieces, in order: zeros, bytes stored
//! as they are somewhere in the file, or a compressed block. [`VDisk`] turns that into a
//! plain stream of the raw disk. Compressed blocks are decoded ahead of time by a few
//! worker threads (they're independent of each other), in a bounded window, so memory
//! stays small and the order is kept.

use super::codec::{self, Codec};
use super::source::{Source, bad, size_text};
use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

/// Where a piece of the disk comes from.
#[derive(Clone, Copy, Debug)]
pub enum Run {
    /// Reads as zeros (unallocated, or marked as zeros).
    Zeros,
    /// Stored as is, from this offset in the file.
    Stored(u64),
    /// A compressed block; the piece is the start of what it decodes to.
    Packed(Packed),
}

#[derive(Clone, Copy, Debug)]
pub struct Packed {
    /// Where the compressed data starts in the file.
    pub offset: u64,
    /// How many bytes to read from there (less where the file ends).
    pub len: u32,
    pub codec: Codec,
    /// What the block decodes to, at most.
    pub size: u32,
    /// If it decodes to less than the piece needs, the rest reads as zeros (instead of
    /// being an error).
    pub short_ok: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Piece {
    pub len: u64,
    pub run: Run,
}

impl Piece {
    pub fn zeros(len: u64) -> Piece {
        Piece {
            len,
            run: Run::Zeros,
        }
    }

    pub fn stored(offset: u64, len: u64) -> Piece {
        Piece {
            len,
            run: Run::Stored(offset),
        }
    }
}

/// A format's map of its disk.
pub trait Layout: Send {
    /// The next piece, in order from the start of the disk. None once there's nothing
    /// more (the rest of the disk reads as zeros). Pieces may run past the disk's end.
    fn next(&mut self, src: &Source) -> io::Result<Option<Piece>>;

    /// Sees the disk's bytes as they're returned, in order (DMG checks its checksums).
    fn check(&mut self, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// The biggest compressed block we decode (7-Zip's limit for DMG chunks too).
pub const MAX_BLOCK: u32 = 256 << 20;
/// Pieces looked at ahead of the reader.
const MAX_AHEAD: usize = 4096;
/// How far ahead of the reader the layout is followed (so that neighbouring stored pieces
/// become one read, and the workers have blocks to decode).
const LOOK_AHEAD: u64 = 64 << 20;
/// Decoded bytes in flight at most (besides the block being returned).
const IN_FLIGHT_BYTES: u64 = 256 << 20;

enum Slot {
    Zeros {
        len: u64,
    },
    Stored {
        offset: u64,
        len: u64,
    },
    Packed {
        len: u64,
        /// Where the piece starts on the disk (for messages).
        pos: u64,
        packed: Packed,
        job: Option<Receiver<io::Result<Vec<u8>>>>,
        data: Option<Vec<u8>>,
        at: usize,
    },
}

pub struct VDisk {
    src: Arc<Source>,
    layout: Box<dyn Layout>,
    size: u64,
    /// Bytes returned so far.
    pos: u64,
    /// Bytes the layout has described so far.
    mapped: u64,
    map_done: bool,
    ahead: VecDeque<Slot>,
    pool: Option<Pool>,
    pool_tried: bool,
    /// Empty pieces in a row from the layout.
    empty: u32,
    /// Packed slots in `ahead` with a job running, and their decoded sizes.
    in_flight: usize,
    in_flight_bytes: u64,
}

impl VDisk {
    pub fn new(src: Arc<Source>, size: u64, layout: Box<dyn Layout>) -> VDisk {
        VDisk {
            src,
            layout,
            size,
            pos: 0,
            mapped: 0,
            map_done: size == 0,
            ahead: VecDeque::new(),
            pool: None,
            pool_tried: false,
            empty: 0,
            in_flight: 0,
            in_flight_bytes: 0,
        }
    }

    /// Gets pieces from the layout: at least one, and more while compressed blocks can be
    /// handed to the workers.
    fn fill(&mut self) -> io::Result<()> {
        while !self.map_done
            && self.ahead.len() < MAX_AHEAD
            && (self.ahead.is_empty()
                || self.mapped < self.pos + LOOK_AHEAD
                    && self.pool.as_ref().is_none_or(|p| {
                        self.in_flight < p.workers * 2 && self.in_flight_bytes < IN_FLIGHT_BYTES
                    }))
        {
            match self.layout.next(&self.src)? {
                None => self.map_done = true,
                Some(piece) if piece.len == 0 => {
                    self.empty += 1;
                    if self.empty > 1 << 16 {
                        return Err(bad("the image is damaged (its map goes nowhere)"));
                    }
                }
                Some(piece) => {
                    self.empty = 0;
                    let pos = self.mapped;
                    self.mapped = self.mapped.saturating_add(piece.len);
                    self.map_done = self.mapped >= self.size;
                    self.push(piece, pos)?;
                }
            }
        }
        Ok(())
    }

    fn push(&mut self, piece: Piece, pos: u64) -> io::Result<()> {
        let len = piece.len;
        let merged = match (piece.run, self.ahead.back_mut()) {
            (Run::Zeros, Some(Slot::Zeros { len: last })) => {
                *last += len;
                true
            }
            (
                Run::Stored(offset),
                Some(Slot::Stored {
                    offset: o,
                    len: last,
                }),
            ) if *o + *last == offset => {
                *last += len;
                true
            }
            _ => false,
        };
        if merged {
            return Ok(());
        }
        match piece.run {
            Run::Zeros => self.ahead.push_back(Slot::Zeros { len }),
            Run::Stored(offset) => self.ahead.push_back(Slot::Stored { offset, len }),
            Run::Packed(packed) => {
                if packed.size > MAX_BLOCK || packed.len > MAX_BLOCK || len > packed.size as u64 {
                    return Err(bad(format!(
                        "the image is damaged (a compressed block at byte {} of the file claims to hold {})",
                        packed.offset,
                        size_text(len.max(packed.size.max(packed.len) as u64))
                    )));
                }
                if !self.pool_tried {
                    self.pool_tried = true;
                    self.pool = Pool::start(&self.src);
                }
                let job = self.pool.as_ref().and_then(|pool| pool.submit(packed));
                if job.is_some() {
                    self.in_flight += 1;
                    self.in_flight_bytes += packed.size as u64;
                }
                self.ahead.push_back(Slot::Packed {
                    len,
                    pos,
                    packed,
                    job,
                    data: None,
                    at: 0,
                });
            }
        }
        Ok(())
    }
}

impl Read for VDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = (self.size - self.pos).min(buf.len() as u64) as usize;
        let mut n = 0;
        while n < want {
            self.fill()?;
            let room = (want - n) as u64;
            let Some(slot) = self.ahead.front_mut() else {
                // The layout ended early: the rest is zeros.
                buf[n..want].fill(0);
                n = want;
                break;
            };
            let (k, done) = match slot {
                Slot::Zeros { len } => {
                    let k = (*len).min(room) as usize;
                    buf[n..n + k].fill(0);
                    *len -= k as u64;
                    (k, *len == 0)
                }
                Slot::Stored { offset, len } => {
                    let k = (*len).min(room) as usize;
                    self.src.read_at(&mut buf[n..n + k], *offset)?;
                    *offset += k as u64;
                    *len -= k as u64;
                    (k, *len == 0)
                }
                Slot::Packed {
                    len,
                    pos,
                    packed,
                    job,
                    data,
                    at,
                } => {
                    if data.is_none() {
                        let result = match job.take() {
                            Some(job) => {
                                self.in_flight -= 1;
                                self.in_flight_bytes -= packed.size as u64;
                                job.recv().unwrap_or_else(|_| decode(&self.src, packed))
                            }
                            None => decode(&self.src, packed),
                        };
                        let got = result.map_err(|e| damaged(*pos, packed, &e))?;
                        if (got.len() as u64) < *len && !packed.short_ok {
                            return Err(damaged(
                                *pos,
                                packed,
                                &bad(format!(
                                    "it holds {} instead of {}",
                                    size_text(got.len() as u64),
                                    size_text(*len)
                                )),
                            ));
                        }
                        *data = Some(got);
                    }
                    let got = data.as_deref().unwrap_or_default();
                    let k = (*len - *at as u64).min(room) as usize;
                    // Past what the block decoded to (allowed for VMDK grains): zeros.
                    let from = (*at).min(got.len());
                    let have = (got.len() - from).min(k);
                    buf[n..n + have].copy_from_slice(&got[from..from + have]);
                    buf[n + have..n + k].fill(0);
                    *at += k;
                    (k, *at as u64 == *len)
                }
            };
            n += k;
            if done {
                self.ahead.pop_front();
            }
        }
        self.pos += n as u64;
        self.layout.check(&buf[..n])?;
        Ok(n)
    }
}

fn damaged(pos: u64, packed: &Packed, err: &io::Error) -> io::Error {
    bad(format!(
        "the image is damaged: the {} block for the disk's data at byte {pos} (stored at byte {} of the file) doesn't decode: {err}",
        packed.codec.name(),
        packed.offset
    ))
}

/// Reads and decodes one block.
fn decode(src: &Source, packed: &Packed) -> io::Result<Vec<u8>> {
    let input = src.read_upto(packed.offset, packed.len as usize)?;
    let mut out = Vec::new();
    codec::decode(packed.codec, &input, &mut out, packed.size as usize)?;
    Ok(out)
}

struct Job {
    packed: Packed,
    reply: SyncSender<io::Result<Vec<u8>>>,
}

/// Threads that decode compressed blocks. They stop once the pool is dropped.
struct Pool {
    jobs: Sender<Job>,
    workers: usize,
}

impl Pool {
    /// None on a single CPU: blocks are then decoded as they're reached.
    fn start(src: &Arc<Source>) -> Option<Pool> {
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        if workers < 2 {
            return None;
        }
        let (jobs, queue) = mpsc::channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        let mut started = 0;
        for _ in 0..workers {
            let queue = queue.clone();
            let src = src.clone();
            let spawned = std::thread::Builder::new()
                .name("dd-gui-decode".into())
                .spawn(move || {
                    loop {
                        let job = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(job) = job else { break };
                        let _ = job.reply.send(decode(&src, &job.packed));
                    }
                });
            if spawned.is_ok() {
                started += 1;
            }
        }
        (started > 0).then_some(Pool {
            jobs,
            workers: started,
        })
    }

    fn submit(&self, packed: Packed) -> Option<Receiver<io::Result<Vec<u8>>>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.jobs.send(Job { packed, reply }).ok()?;
        Some(result)
    }
}

/// Reads a table of fixed-size entries (a BAT, an L1 table…) a window at a time, so a
/// huge table never has to be in memory at once.
pub struct Table {
    offset: u64,
    entries: u64,
    entry_size: usize,
    window: Vec<u8>,
    first: u64,
}

impl Table {
    const WINDOW: usize = 1 << 20;

    pub fn new(offset: u64, entries: u64, entry_size: usize) -> Table {
        Table {
            offset,
            entries,
            entry_size,
            window: Vec::new(),
            first: 0,
        }
    }

    /// The raw bytes of entry `i` (None past the end of the table).
    pub fn get(&mut self, src: &Source, i: u64) -> io::Result<Option<&[u8]>> {
        if i >= self.entries {
            return Ok(None);
        }
        let per_window = (Self::WINDOW / self.entry_size) as u64;
        let loaded = (self.window.len() / self.entry_size) as u64;
        if i < self.first || i >= self.first + loaded {
            self.first = i - i % per_window;
            let count = per_window.min(self.entries - self.first);
            self.window = src.read_vec(
                self.offset + self.first * self.entry_size as u64,
                count as usize * self.entry_size,
            )?;
        }
        let at = (i - self.first) as usize * self.entry_size;
        Ok(Some(&self.window[at..at + self.entry_size]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A layout from a list, for testing the reader alone.
    struct List(std::vec::IntoIter<Piece>);

    impl Layout for List {
        fn next(&mut self, _: &Source) -> io::Result<Option<Piece>> {
            Ok(self.0.next())
        }
    }

    #[test]
    fn pieces_in_order_with_parallel_blocks() {
        let dir = std::env::temp_dir().join(format!("dd-gui-vdisk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blocks");
        // The file: 1000 stored bytes, then 300 zlib blocks of 10000 bytes each.
        let stored: Vec<u8> = (0..1000u32).map(|i| (i * 13) as u8).collect();
        let mut file = stored.clone();
        let mut pieces = vec![Piece::zeros(7), Piece::stored(0, 1000)];
        let mut want = vec![0u8; 7];
        want.extend(&stored);
        for b in 0..300u32 {
            let block: Vec<u8> = (0..10_000u32).map(|i| ((i / 7) ^ b) as u8).collect();
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            z.write_all(&block).unwrap();
            let packed = z.finish().unwrap();
            pieces.push(Piece {
                len: if b == 299 { 5000 } else { 10_000 },
                run: Run::Packed(Packed {
                    offset: file.len() as u64,
                    len: packed.len() as u32,
                    codec: Codec::Zlib,
                    size: 10_000,
                    short_ok: false,
                }),
            });
            want.extend(&block[..if b == 299 { 5000 } else { 10_000 }]);
            file.extend(packed);
            if b % 50 == 0 {
                pieces.push(Piece::zeros(3));
                want.extend([0; 3]);
            }
        }
        std::fs::write(&path, &file).unwrap();
        let src = Arc::new(Source::new(std::fs::File::open(&path).unwrap()).unwrap());
        // The disk is a bit longer than the pieces: zeros at the end.
        let size = want.len() as u64 + 100;
        want.extend([0; 100]);
        let mut disk = VDisk::new(src, size, Box::new(List(pieces.into_iter())));
        let mut got: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 777];
        loop {
            let n = disk.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend(&buf[..n]);
        }
        assert!(got == want, "the disk doesn't read back right");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
