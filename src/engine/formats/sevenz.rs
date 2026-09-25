//! 7z archives: the first file in them (the first one that isn't a folder or empty).
//! Decoding is sevenz-rust2's: LZMA, LZMA2, PPMd, BZip2, Deflate, Zstandard, LZ4, Copy,
//! with the BCJ/BCJ2/ARM/… and Delta filters. Encrypted archives (AES) are refused.
//!
//! The decoder borrows the archive it reads, so it runs on a thread of its own that
//! hands the file's contents over in pieces; LZMA2 is decoded single-threaded, whose
//! memory use stays at the dictionary's size.

use super::source::{Reader, Source, bad, le32, read_full, size_text, unsupported};
use sevenz_rust2::{Archive, BlockDecoder, EncoderMethod, Error, Password};
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

pub const SIGNATURE: &[u8] = &[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c];

/// The most memory a coder may ask for (a dictionary, PPMd's model).
const MAX_MEMORY: u64 = 1536 << 20;
/// Pieces handed from the decoding thread.
const PIECE: usize = 1 << 20;

/// The first file: its block and size.
struct First {
    block: usize,
    size: u64,
}

fn read_archive(file: &mut Reader) -> io::Result<Archive> {
    Archive::read(file, &Password::empty()).map_err(|e| error(e, "the 7z archive is damaged"))
}

fn first_file(archive: &Archive) -> io::Result<First> {
    let index = archive
        .files
        .iter()
        .position(|f| f.has_stream && f.size > 0 && !f.is_directory && !f.is_anti_item)
        .ok_or_else(|| bad("the 7z archive holds no files"))?;
    let block = archive
        .stream_map
        .file_block_index
        .get(index)
        .copied()
        .flatten()
        .ok_or_else(|| bad("the 7z archive is damaged"))?;
    let coders = &archive
        .blocks
        .get(block)
        .ok_or_else(|| bad("the 7z archive is damaged"))?
        .coders;
    for coder in coders {
        let id = coder.encoder_method_id();
        let props = coder.properties();
        if id == EncoderMethod::ID_AES256_SHA256 {
            return Err(encrypted());
        }
        let memory = if id == EncoderMethod::ID_LZMA || id == EncoderMethod::ID_PPMD {
            le32(props, 1) as u64
        } else if id == EncoderMethod::ID_LZMA2 {
            match props.first() {
                Some(&d) if d < 40 => (2 | (d as u64 & 1)) << (d / 2 + 11),
                _ => u32::MAX as u64,
            }
        } else {
            0
        };
        if memory > MAX_MEMORY {
            return Err(unsupported(format!(
                "this 7z archive needs {} of memory to unpack, more than DD-GUI allows",
                size_text(memory)
            )));
        }
    }
    Ok(First {
        block,
        size: archive.files[index].size,
    })
}

fn encrypted() -> io::Error {
    unsupported(
        "this 7z archive is password-protected, which DD-GUI doesn't support. Unpack it with 7-Zip first.",
    )
}

/// A 7z error in plain words.
fn error(e: Error, context: &str) -> io::Error {
    match e {
        Error::PasswordRequired | Error::MaybeBadPassword(_) => encrypted(),
        Error::UnsupportedCompressionMethod(m) if m.contains("AES") => encrypted(),
        Error::UnsupportedCompressionMethod(m) => unsupported(format!(
            "this 7z archive uses a compression method DD-GUI doesn't support ({m})"
        )),
        Error::ExternalUnsupported => {
            unsupported("this 7z archive uses a feature DD-GUI doesn't support (external data)")
        }
        Error::ChecksumVerificationFailed | Error::NextHeaderCrcMismatch => {
            bad(format!("{context} (checksum mismatch)"))
        }
        Error::Io(e, _) | Error::FileOpen(e, _) if e.kind() != io::ErrorKind::Other => e,
        other => bad(format!("{context} ({other})")),
    }
}

/// For `detect`: the first file's size, and the first bytes of it (to see if it's a tar).
pub fn peek(src: &Arc<Source>) -> io::Result<(u64, Vec<u8>)> {
    let mut file = src.clone().reader(0);
    let archive = read_archive(&mut file)?;
    let first = first_file(&archive)?;
    let mut head = Vec::new();
    let password = Password::empty();
    BlockDecoder::new(1, first.block, &archive, &password, &mut file)
        .for_each_entries(&mut |entry, reader| {
            if !(entry.has_stream && entry.size > 0) {
                return Ok(true);
            }
            head = vec![0u8; 8192.min(entry.size as usize)];
            let n = read_full(reader, &mut head)?;
            head.truncate(n);
            Ok(false)
        })
        .map_err(|e| error(e, "the 7z archive is damaged"))?;
    Ok((first.size, head))
}

/// The first file's contents, and its size.
pub fn open(src: &Arc<Source>) -> io::Result<(Box<dyn Read + Send>, u64)> {
    let mut file = src.clone().reader(0);
    let archive = read_archive(&mut file)?;
    let first = first_file(&archive)?;
    let size = first.size;
    let (tx, rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(4);
    std::thread::Builder::new()
        .name("dd-gui-7z".into())
        .spawn(move || {
            let password = Password::empty();
            let mut sent = 0u64;
            let result = BlockDecoder::new(1, first.block, &archive, &password, &mut file)
                .for_each_entries(&mut |entry, reader| {
                    if !(entry.has_stream && entry.size > 0) {
                        return Ok(true);
                    }
                    loop {
                        let mut piece = vec![0u8; PIECE];
                        let n = read_full(reader, &mut piece)?;
                        if n == 0 {
                            return Ok(false);
                        }
                        piece.truncate(n);
                        sent += n as u64;
                        if tx.send(Ok(piece)).is_err() {
                            return Err(Error::Other("stopped".into()));
                        }
                    }
                });
            let end = match result {
                Ok(_) if sent == first.size => Ok(Vec::new()),
                Ok(_) => Err(bad("the 7z archive is cut short")),
                Err(e) => Err(error(e, "the 7z archive is damaged")),
            };
            let _ = tx.send(end);
        })?;
    Ok((
        Box::new(Pieces {
            rx,
            piece: Vec::new(),
            at: 0,
            done: false,
        }),
        size,
    ))
}

/// Reads what the decoding thread sends: pieces, then an empty one at the end.
struct Pieces {
    rx: Receiver<io::Result<Vec<u8>>>,
    piece: Vec<u8>,
    at: usize,
    done: bool,
}

impl Read for Pieces {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.piece.len() {
            if self.done || buf.is_empty() {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Ok(piece)) if piece.is_empty() => self.done = true,
                Ok(Ok(piece)) => (self.piece, self.at) = (piece, 0),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(bad("the 7z decoder stopped unexpectedly")),
            }
        }
        let n = (self.piece.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}
