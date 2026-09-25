//! bzip2 (.bz2): one stream after another, as `pbzip2` and `cat a.bz2 b.bz2` make them.
//! Zeros after the last stream (padding) are fine; anything else is an error, so a
//! damaged stream header can't pass for the end of the data.
//!
//! With several CPUs, blocks are decoded in parallel ([`ParBz2`]): each block starts with
//! a 48-bit magic number, at any bit position. A thread finds them, and workers decode
//! each block as a stream of its own (a header, the block, an end marker with the block's
//! CRC). Every block's CRC and every stream's combined CRC are checked. Anything odd (a
//! magic number that was really part of the data, damage) makes the plain decoder take
//! over from the start of that stream, so results are always the plain decoder's.

use super::source::{Reader, Source, bad};
use std::io::{self, BufRead, BufReader, Read};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

/// "BZh", a block size 1–9, then a block's magic (π) or the end of an empty stream (√π).
pub fn detect(head: &[u8]) -> bool {
    head.len() >= 10
        && head.starts_with(b"BZh")
        && (b'1'..=b'9').contains(&head[3])
        && (head[4..10] == [0x31, 0x41, 0x59, 0x26, 0x53, 0x59]
            || head[4..10] == [0x17, 0x72, 0x45, 0x38, 0x50, 0x90])
}

pub struct Bz2<R> {
    src: R,
    stream: Option<bzip2::Decompress>,
}

impl<R: BufRead> Bz2<R> {
    pub fn new(src: R) -> Self {
        Bz2 { src, stream: None }
    }
}

impl<R: BufRead> Read for Bz2<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(stream) = &mut self.stream else {
                // Between streams: another one, padding, or the end.
                let input = self.src.fill_buf()?;
                if input.is_empty() {
                    return Ok(0);
                }
                if input.starts_with(b"BZh") || (input.len() < 3 && b"BZh".starts_with(input)) {
                    self.stream = Some(bzip2::Decompress::new(false));
                    continue;
                }
                let zeros = input.iter().take_while(|&&b| b == 0).count();
                if zeros == 0 {
                    return Err(bad("the bzip2 file has unexpected data after its end"));
                }
                self.src.consume(zeros);
                continue;
            };
            let input = self.src.fill_buf()?;
            let (in_before, out_before) = (stream.total_in(), stream.total_out());
            let status = stream
                .decompress(input, buf)
                .map_err(|e| bad(format!("the bzip2 data is damaged ({e})")))?;
            let used = (stream.total_in() - in_before) as usize;
            let made = (stream.total_out() - out_before) as usize;
            let ended = input.is_empty();
            self.src.consume(used);
            if status == bzip2::Status::StreamEnd {
                self.stream = None;
            } else if made == 0 && ended {
                return Err(bad("the bzip2 data is cut short"));
            } else if made == 0 && used == 0 {
                return Err(bad("the bzip2 data is damaged"));
            }
            if made > 0 {
                return Ok(made);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// In parallel

/// A block's start (π, as bits).
const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
/// The end of a stream (√π), followed by the stream's CRC.
const END_MAGIC: u64 = 0x1772_4538_5090;
const MASK48: u64 = (1 << 48) - 1;
/// The most compressed bytes a block takes (900k of input, and then some).
const MAX_PACKED_BLOCK: u64 = 2 << 20;
/// The most a block decodes to (runs of 4–255 equal bytes take 5 bytes of a 900k block).
const MAX_BLOCK_OUT: usize = 48 << 20;
/// Workers hand decoded data over in pieces of this size, so a block of zeros that
/// decodes to 46 MB never sits in memory whole.
const PIECE: usize = 2 << 20;
/// How much the splitter reads at a time.
const READ: u64 = 4 << 20;

/// For each byte value: at which shifts (bits 0–7) it can be the byte before the last
/// byte of a magic number. (That byte lies wholly inside the magic, whatever its shift.)
const CANDIDATES: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut s = 0;
    while s < 8 {
        t[((BLOCK_MAGIC >> (8 - s)) & 0xff) as usize] |= 1 << s;
        t[((END_MAGIC >> (8 - s)) & 0xff) as usize] |= 1 << s;
        s += 1;
    }
    t
};

/// What a worker sends back: pieces of the block, then an empty piece at its end.
type Pieces = Receiver<io::Result<Vec<u8>>>;

/// What the splitter finds, in the file's order.
enum Item {
    /// A stream starts at this offset of the file.
    Stream(u64),
    /// A block: its CRC, and its data as a worker decodes it.
    Block(u32, Pieces),
    /// The end of a stream, with its combined CRC.
    StreamEnd(u32),
    /// The end of the file.
    End,
    /// Something a bzip2 file shouldn't hold here (or a read error): the plain decoder
    /// takes over and says what it is.
    Trouble,
}

struct Job {
    stream: Vec<u8>,
    pieces: SyncSender<io::Result<Vec<u8>>>,
}

/// bzip2 decoded by several threads, one block each.
pub struct ParBz2 {
    src: Arc<Source>,
    items: Option<Receiver<Item>>,
    /// The block being returned: its CRC and what's still to come of it.
    block: Option<(u32, Pieces)>,
    piece: Vec<u8>,
    at: usize,
    /// Where the current stream starts, and how much of it has been returned.
    stream_start: u64,
    stream_out: u64,
    /// The combined CRC of the stream's blocks so far.
    combined: u32,
    /// The plain decoder, once it has taken over.
    plain: Option<Bz2<BufReader<Reader>>>,
    /// The end has been reached.
    done: bool,
    /// For tests: treat this block (counting from 1) as undecodable.
    fail_block: u64,
    blocks: u64,
}

impl ParBz2 {
    /// None on a single CPU, where the plain decoder does as well.
    pub fn new(src: Arc<Source>) -> Option<ParBz2> {
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        if workers < 2 {
            return None;
        }
        let (jobs, queue) = mpsc::channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        for _ in 0..workers {
            let queue = queue.clone();
            std::thread::Builder::new()
                .name("dd-gui-bzip2".into())
                .spawn(move || {
                    loop {
                        let job = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(job) = job else { break };
                        decode_block(&job.stream, &job.pieces);
                    }
                })
                .ok()?;
        }
        // Blocks in flight: one per worker and a couple waiting. (Each holds at most two
        // pieces: the one being filled, and one ready.)
        let (items_tx, items) = mpsc::sync_channel::<Item>(workers + 2);
        let from = src.clone();
        std::thread::Builder::new()
            .name("dd-gui-bzip2-split".into())
            .spawn(move || {
                let mut window = Window {
                    src: from,
                    buf: Vec::new(),
                    base: 0,
                };
                let item = match split(&mut window, &items_tx, &jobs) {
                    Ok(true) => Item::End,
                    Ok(false) => return, // the reader is gone
                    Err(_) => Item::Trouble,
                };
                let _ = items_tx.send(item);
            })
            .ok()?;
        Some(ParBz2 {
            src,
            items: Some(items),
            block: None,
            piece: Vec::new(),
            at: 0,
            stream_start: 0,
            stream_out: 0,
            combined: 0,
            plain: None,
            done: false,
            fail_block: u64::MAX,
            blocks: 0,
        })
    }

    /// The plain decoder takes over, from the start of the current stream (skipping what
    /// of it was returned already).
    fn fall_back(&mut self) -> io::Result<()> {
        self.items = None;
        self.block = None;
        let reader = BufReader::with_capacity(1 << 20, self.src.clone().reader(self.stream_start));
        let mut plain = Bz2::new(reader);
        let skipped = io::copy(&mut (&mut plain).take(self.stream_out), &mut io::sink())?;
        if skipped != self.stream_out {
            return Err(bad("the bzip2 data is cut short"));
        }
        self.plain = Some(plain);
        Ok(())
    }
}

impl Read for ParBz2 {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.done {
            return Ok(0);
        }
        loop {
            if let Some(plain) = &mut self.plain {
                return plain.read(buf);
            }
            if self.at < self.piece.len() {
                let n = (self.piece.len() - self.at).min(buf.len());
                buf[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
                self.at += n;
                self.stream_out += n as u64;
                return Ok(n);
            }
            if let Some((crc, pieces)) = &self.block {
                match pieces.recv() {
                    // The end of the block.
                    Ok(Ok(piece)) if piece.is_empty() => {
                        self.combined = self.combined.rotate_left(1) ^ *crc;
                        self.block = None;
                    }
                    Ok(Ok(piece)) => (self.piece, self.at) = (piece, 0),
                    // It doesn't decode, or the worker is gone.
                    _ => self.fall_back()?,
                }
                continue;
            }
            match self.items.as_ref().and_then(|items| items.recv().ok()) {
                Some(Item::Stream(offset)) => {
                    (self.stream_start, self.stream_out, self.combined) = (offset, 0, 0);
                }
                Some(Item::Block(crc, pieces)) => {
                    self.blocks += 1;
                    if self.blocks == self.fail_block {
                        self.fall_back()?;
                    } else {
                        self.block = Some((crc, pieces));
                    }
                }
                Some(Item::StreamEnd(crc)) if crc == self.combined => {}
                Some(Item::End) => {
                    self.done = true;
                    return Ok(0);
                }
                // A checksum that doesn't add up, trouble, or the splitter gone.
                _ => self.fall_back()?,
            }
        }
    }
}

/// The file's bytes around where the splitter is.
struct Window {
    src: Arc<Source>,
    /// File bytes from `base`.
    buf: Vec<u8>,
    base: u64,
}

impl Window {
    /// Makes sure the bytes before file offset `end` are in. False if the file ends first.
    fn fill_to(&mut self, end: u64) -> io::Result<bool> {
        let have = self.base + self.buf.len() as u64;
        if end <= have {
            return Ok(true);
        }
        if have >= self.src.size {
            return Ok(false);
        }
        let want = (end - have).max(READ).min(self.src.size - have) as usize;
        let at = self.buf.len();
        self.buf.resize(at + want, 0);
        self.src.read_at(&mut self.buf[at..], have)?;
        Ok(end <= self.base + self.buf.len() as u64)
    }

    /// Forgets what's before file offset `keep` (now and then, not to move bytes often).
    fn keep_from(&mut self, keep: u64) {
        let n = keep.saturating_sub(self.base) as usize;
        if n >= 8 << 20 {
            self.buf.drain(..n.min(self.buf.len()));
            self.base = keep;
        }
    }

    fn byte(&self, at: u64) -> u8 {
        self.buf[(at - self.base) as usize]
    }

    /// `n` (≤ 56) bits from bit `bit` of the file, most significant first. Bits past what's
    /// loaded read as zeros.
    fn bits(&self, bit: u64, n: u32) -> u64 {
        let i = (bit / 8 - self.base) as usize;
        let mut b = [0u8; 8];
        let avail = self.buf.len().saturating_sub(i).min(8);
        b[..avail].copy_from_slice(&self.buf[i..i + avail]);
        (u64::from_be_bytes(b) << (bit % 8)) >> (64 - n)
    }

    /// The first magic number that starts at bit `from` or later, and not later than bit
    /// `limit`: (its first bit, whether it ends a stream). None if there's none (or the
    /// file ends first). `from` must be at least 64 bits into the window.
    fn find(&mut self, from: u64, limit: u64) -> io::Result<Option<(u64, bool)>> {
        // Bytes that could hold the last bit of such a magic.
        let mut j = (from + 47) / 8;
        let last = (limit + 47) / 8;
        while j <= last {
            if !self.fill_to(j + 1)? {
                return Ok(None);
            }
            let end = (self.base + self.buf.len() as u64).min(last + 1);
            // For each byte j that could end a magic, the byte before it tells which shifts
            // are worth a look; then the 8 bytes up to j make a 64-bit register.
            let first = (j - 7 - self.base) as usize;
            let count = (end - j) as usize;
            let before_last = &self.buf[first + 6..first + 6 + count];
            for (i, &b) in before_last.iter().enumerate() {
                let candidates = CANDIDATES[b as usize];
                if candidates == 0 {
                    continue;
                }
                let k = first + i;
                let reg = u64::from_be_bytes(self.buf[k..k + 8].try_into().unwrap());
                // A bigger shift means an earlier start.
                for s in (0..8).rev() {
                    if candidates & (1 << s) == 0 {
                        continue;
                    }
                    let v = (reg >> s) & MASK48;
                    let start = (j + i as u64 + 1) * 8 - s - 48;
                    if (v == BLOCK_MAGIC || v == END_MAGIC) && start >= from && start <= limit {
                        return Ok(Some((start, v == END_MAGIC)));
                    }
                }
            }
            j = end;
        }
        Ok(None)
    }

    /// The block from bit `a` to bit `b` as a stream of its own: the header, the block, an
    /// end marker, and the block's CRC as the stream's.
    fn stream(&self, level: u8, a: u64, b: u64, crc: u32) -> Vec<u8> {
        let bits = b - a;
        let whole = (bits / 8) as usize;
        let mut out = Vec::with_capacity(whole + 16);
        out.extend_from_slice(b"BZh");
        out.push(level);
        let src = &self.buf[(a / 8 - self.base) as usize..];
        let shift = (a % 8) as u32;
        if shift == 0 {
            out.extend_from_slice(&src[..whole]);
        } else {
            out.extend(
                src.windows(2)
                    .take(whole)
                    .map(|w| (w[0] << shift) | (w[1] >> (8 - shift))),
            );
        }
        let mut w = BitWriter { out, acc: 0, n: 0 };
        let rest = (bits % 8) as u32;
        if rest > 0 {
            w.put(self.bits(a + whole as u64 * 8, rest), rest);
        }
        w.put(END_MAGIC, 48);
        w.put(crc as u64, 32);
        w.finish()
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u64, bits: u32) {
        self.acc = (self.acc << bits) | (v & ((1 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
        }
        self.out
    }
}

/// Finds the streams and blocks and hands the blocks to the workers. Ok(true) at a clean
/// end of the file, Ok(false) once the reader is gone.
fn split(w: &mut Window, items: &SyncSender<Item>, jobs: &mpsc::Sender<Job>) -> io::Result<bool> {
    let trouble = || Err(bad("not what a bzip2 file holds"));
    let mut pos = 0u64; // where the next stream starts
    loop {
        // A stream's header, zeros up to the end, or the end.
        if !w.fill_to(pos + 1)? {
            return Ok(true);
        }
        if w.byte(pos) == 0 {
            while w.fill_to(pos + 1)? {
                if w.byte(pos) != 0 {
                    return trouble();
                }
                pos += 1;
            }
            return Ok(true);
        }
        if !w.fill_to(pos + 14)? {
            return trouble();
        }
        let level = w.byte(pos + 3);
        if w.bits(pos * 8, 24) != 0x42_5a68 || !(b'1'..=b'9').contains(&level) {
            return trouble();
        }
        if items.send(Item::Stream(pos)).is_err() {
            return Ok(false);
        }
        let mut a = (pos + 4) * 8;
        match w.bits(a, 48) {
            BLOCK_MAGIC => {}
            END_MAGIC => {
                // An empty stream.
                if items
                    .send(Item::StreamEnd(w.bits(a + 48, 32) as u32))
                    .is_err()
                {
                    return Ok(false);
                }
                pos = (a + 80).div_ceil(8);
                continue;
            }
            _ => return trouble(),
        }
        loop {
            // The next magic, after this block's header and within a block's reach.
            let Some((b, end)) = w.find(a + 80, a + MAX_PACKED_BLOCK * 8)? else {
                return trouble();
            };
            let crc = w.bits(a + 48, 32) as u32;
            let stream = w.stream(level, a, b, crc);
            let (pieces, rx) = mpsc::sync_channel(1);
            if jobs.send(Job { stream, pieces }).is_err()
                || items.send(Item::Block(crc, rx)).is_err()
            {
                return Ok(false);
            }
            if end {
                if !w.fill_to((b + 80).div_ceil(8))? {
                    return trouble();
                }
                if items
                    .send(Item::StreamEnd(w.bits(b + 48, 32) as u32))
                    .is_err()
                {
                    return Ok(false);
                }
                pos = (b + 80).div_ceil(8);
                w.keep_from(pos);
                break;
            }
            a = b;
            w.keep_from(a / 8);
        }
    }
}

/// Decodes a stream of one block, handing its data over a piece at a time, then an empty
/// piece. (Stops quietly once nobody wants the pieces.)
fn decode_block(stream: &[u8], pieces: &SyncSender<io::Result<Vec<u8>>>) {
    let mut bz = bzip2::Decompress::new(false);
    let mut total = 0usize;
    loop {
        let mut piece = Vec::with_capacity(PIECE);
        let ended = loop {
            let before = (bz.total_in(), bz.total_out());
            let status = bz.decompress_vec(&stream[bz.total_in() as usize..], &mut piece);
            match status {
                Ok(bzip2::Status::StreamEnd) => break true,
                Ok(_) if piece.len() == piece.capacity() => break false,
                Ok(_) if (bz.total_in(), bz.total_out()) != before => {}
                Ok(_) => {
                    let _ = pieces.send(Err(bad("the bzip2 data is cut short")));
                    return;
                }
                Err(e) => {
                    let _ = pieces.send(Err(bad(format!("the bzip2 data is damaged ({e})"))));
                    return;
                }
            }
        };
        total += piece.len();
        if total > MAX_BLOCK_OUT {
            let _ = pieces.send(Err(bad("a bzip2 block decodes to too much")));
            return;
        }
        if !piece.is_empty() && pieces.send(Ok(piece)).is_err() {
            return;
        }
        if ended {
            let _ = pieces.send(Ok(Vec::new()));
            return;
        }
    }
}

#[cfg(test)]
impl ParBz2 {
    /// Pretends block `n` (counting from 1) didn't decode: the plain decoder has to take
    /// over.
    pub fn failing_at(mut self, n: u64) -> Self {
        self.fail_block = n;
        self
    }

    /// Whether the plain decoder had to take over.
    pub fn fell_back(&self) -> bool {
        self.plain.is_some()
    }
}
