//! VHDX (Hyper-V): two headers (the one with the higher sequence number wins), a region
//! table that locates the block allocation table (BAT) and the metadata (block size,
//! disk size, sector size), and payload blocks of 1–256 MiB. Fixed and dynamic disks
//! read the same way; differencing disks aren't supported.
//!
//! A file that wasn't closed cleanly has a log of metadata changes that have to be
//! applied before reading it (MS-VHDX, "Log Replay"). We apply it in memory, on top of
//! the file, which stays untouched.

use super::source::{Patch, Source, bad, crc32c, le16, le32, le64, size_text, unsupported};
use super::vdisk::{Layout, Piece, Table, VDisk};
use super::{Detected, ImageFormat, Opened};
use std::io;
use std::sync::Arc;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
/// VHDX files hold at most 64 TiB of disk, and their metadata: offsets past this are
/// damage.
const MAX_FILE: u64 = 128 << 40;

/// A GUID as VHDX stores it: the first three fields little-endian.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5],
        d4[6], d4[7],
    ]
}

pub const BAT_REGION: [u8; 16] = guid(
    0x2dc2_7766,
    0xf623,
    0x4200,
    [0x9d, 0x64, 0x11, 0x5e, 0x9b, 0xfd, 0x4a, 0x08],
);
pub const METADATA_REGION: [u8; 16] = guid(
    0x8b7c_a206,
    0x4790,
    0x4b9a,
    [0xb8, 0xfe, 0x57, 0x5f, 0x05, 0x0f, 0x88, 0x6e],
);
pub const FILE_PARAMETERS: [u8; 16] = guid(
    0xcaa1_6737,
    0xfa36,
    0x4d43,
    [0xb3, 0xb6, 0x33, 0xf0, 0xaa, 0x44, 0xe7, 0x6b],
);
const VIRTUAL_DISK_SIZE: [u8; 16] = guid(
    0x2fa5_4224,
    0xcd1b,
    0x4876,
    [0xb2, 0x11, 0x5d, 0xbe, 0xd8, 0x3b, 0xf4, 0xb8],
);
const PAGE_83_DATA: [u8; 16] = guid(
    0xbeca_12ab,
    0xb2e6,
    0x4523,
    [0x93, 0xef, 0xc3, 0x09, 0xe0, 0x00, 0xc7, 0x46],
);
const LOGICAL_SECTOR_SIZE: [u8; 16] = guid(
    0x8141_bf1d,
    0xa96f,
    0x4709,
    [0xba, 0x47, 0xf2, 0x33, 0xa8, 0xfa, 0xab, 0x5f],
);
const PHYSICAL_SECTOR_SIZE: [u8; 16] = guid(
    0xcda3_48c7,
    0x445d,
    0x4471,
    [0x9c, 0xc9, 0xe9, 0x88, 0x52, 0x51, 0xc5, 0x56],
);
const PARENT_LOCATOR: [u8; 16] = guid(
    0xa8d3_5f2d,
    0xb30b,
    0x454d,
    [0xab, 0xf7, 0xd3, 0xd8, 0x48, 0x34, 0xab, 0x0c],
);

pub const SIGNATURE: &[u8] = b"vhdxfile";

/// The active header.
struct Header {
    log_guid: [u8; 16],
    log_version: u16,
    log_length: u64,
    log_offset: u64,
}

/// What the metadata says about the disk.
struct Disk {
    size: u64,
    block_size: u64,
    logical_sector: u64,
    bat_offset: u64,
    bat_entries: u64,
}

pub fn detect(src: &Source, head: &[u8]) -> Option<Detected> {
    if !head.starts_with(SIGNATURE) {
        return None;
    }
    // The size is only known for sure once a pending log has been applied.
    let raw_size = header(src)
        .ok()
        .filter(|h| h.log_guid == [0; 16])
        .and_then(|_| disk(src).ok())
        .map(|d| d.size);
    Some(Detected {
        format: ImageFormat::Vhdx,
        raw_size,
    })
}

pub fn open(mut src: Source) -> io::Result<Opened> {
    let header = header(&src)?;
    if header.log_guid != [0; 16] && header.log_length != 0 {
        let (patches, end) = replay_log(&src, &header)?;
        src.set_patches(patches, end);
    }
    let disk = disk(&src)?;
    let size = disk.size;
    let chunk_ratio = (1u64 << 23) * disk.logical_sector / disk.block_size;
    let layout = Blocks {
        bat: Table::new(disk.bat_offset, disk.bat_entries, 8),
        chunk_ratio,
        block_size: disk.block_size,
        next: 0,
        blocks: size.div_ceil(disk.block_size),
    };
    Ok(Opened {
        reader: Box::new(VDisk::new(Arc::new(src), size, Box::new(layout))),
        size: Some(size),
    })
}

/// The current header: of the two, the valid one with the higher sequence number.
fn header(src: &Source) -> io::Result<Header> {
    let mut best: Option<(u64, Vec<u8>)> = None;
    for offset in [64 * KIB, 128 * KIB] {
        let Ok(mut h) = src.read_vec(offset, 4 * KIB as usize) else {
            continue;
        };
        if !h.starts_with(b"head") || !checksum_ok(&mut h, 4) || le16(&h, 66) != 1 {
            continue;
        }
        let seq = le64(&h, 8);
        if best.as_ref().is_none_or(|(s, _)| seq > *s) {
            best = Some((seq, h));
        }
    }
    let (_, h) = best.ok_or_else(|| bad("the VHDX is damaged (no valid header)"))?;
    Ok(Header {
        log_guid: h[48..64].try_into().unwrap_or_default(),
        log_version: le16(&h, 64),
        log_length: le32(&h, 68) as u64,
        log_offset: le64(&h, 72),
    })
}

/// Checks a CRC-32C at `at` that covers all of `b` (with that field taken as zero).
fn checksum_ok(b: &mut [u8], at: usize) -> bool {
    let Some(field) = b.get_mut(at..at + 4) else {
        return false;
    };
    let stored = u32::from_le_bytes([field[0], field[1], field[2], field[3]]);
    field.fill(0);
    let ok = crc32c(0, b) == stored;
    b[at..at + 4].copy_from_slice(&stored.to_le_bytes());
    ok
}

fn disk(src: &Source) -> io::Result<Disk> {
    // The region table, or its copy.
    let mut regions = None;
    for offset in [192 * KIB, 256 * KIB] {
        if let Ok(mut t) = src.read_vec(offset, 64 * KIB as usize)
            && t.starts_with(b"regi")
            && checksum_ok(&mut t, 4)
        {
            regions = Some(t);
            break;
        }
    }
    let t = regions.ok_or_else(|| bad("the VHDX is damaged (no valid region table)"))?;
    let count = le32(&t, 8) as usize;
    if count > 2047 {
        return Err(bad("the VHDX is damaged (region table)"));
    }
    let (mut bat, mut metadata) = (None, None);
    for e in t[16..].as_chunks::<32>().0.iter().take(count) {
        let region = (le64(e, 16), le32(e, 24) as u64);
        match &e[..16] {
            id if id == BAT_REGION => bat = Some(region),
            id if id == METADATA_REGION => metadata = Some(region),
            _ if le32(e, 28) & 1 != 0 => {
                return Err(unsupported(
                    "this VHDX uses a feature DD-GUI doesn't know (an unknown required region)",
                ));
            }
            _ => {}
        }
    }
    let (bat_offset, bat_len) = bat.ok_or_else(|| bad("the VHDX is damaged (no BAT region)"))?;
    let (meta_offset, meta_len) =
        metadata.ok_or_else(|| bad("the VHDX is damaged (no metadata region)"))?;

    // The metadata table: up to 2047 entries of 32 bytes after a 32-byte header.
    let table = src.read_vec(meta_offset, (64 * KIB).min(meta_len.max(32)) as usize)?;
    if !table.starts_with(b"metadata") {
        return Err(bad("the VHDX is damaged (metadata table)"));
    }
    let entries = le16(&table, 10) as usize;
    let item = |id: &[u8; 16], len: usize| -> io::Result<Option<Vec<u8>>> {
        for e in table[32..].as_chunks::<32>().0.iter().take(entries) {
            if e[..16] == *id {
                let (offset, length) = (le32(e, 16) as u64, le32(e, 20) as usize);
                if length < len || offset + len as u64 > meta_len {
                    return Err(bad("the VHDX is damaged (metadata item out of place)"));
                }
                return src
                    .read_vec(meta_offset.saturating_add(offset), len)
                    .map(Some);
            }
        }
        Ok(None)
    };
    let known = [
        FILE_PARAMETERS,
        VIRTUAL_DISK_SIZE,
        PAGE_83_DATA,
        LOGICAL_SECTOR_SIZE,
        PHYSICAL_SECTOR_SIZE,
        PARENT_LOCATOR,
    ];
    for e in table[32..].as_chunks::<32>().0.iter().take(entries) {
        let required = le32(e, 24) & 4 != 0;
        if required && !known.iter().any(|k| e[..16] == *k) {
            return Err(unsupported(
                "this VHDX uses a feature DD-GUI doesn't know (an unknown required metadata item)",
            ));
        }
    }
    let params = item(&FILE_PARAMETERS, 8)?
        .ok_or_else(|| bad("the VHDX is damaged (no file parameters)"))?;
    let block_size = le32(&params, 0) as u64;
    let has_parent = le32(&params, 4) & 2 != 0;
    let has_locator = table[32..]
        .as_chunks::<32>()
        .0
        .iter()
        .take(entries)
        .any(|e| e[..16] == PARENT_LOCATOR);
    if has_parent || has_locator {
        return Err(unsupported(
            "this is a differencing VHDX: it only holds the changes made on top of a parent disk, which DD-GUI doesn't support. Merge it into its parent first (Hyper-V Manager's Edit Disk, or `Merge-VHD` in PowerShell).",
        ));
    }
    let size = item(&VIRTUAL_DISK_SIZE, 8)?
        .map(|b| le64(&b, 0))
        .ok_or_else(|| bad("the VHDX is damaged (no disk size)"))?;
    let logical_sector = item(&LOGICAL_SECTOR_SIZE, 4)?
        .map(|b| le32(&b, 0) as u64)
        .ok_or_else(|| bad("the VHDX is damaged (no sector size)"))?;
    if !(MIB..=256 * MIB).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(bad(format!(
            "the VHDX is damaged (block size {})",
            size_text(block_size)
        )));
    }
    if logical_sector != 512 && logical_sector != 4096 {
        return Err(bad(format!(
            "the VHDX is damaged (sector size {logical_sector})"
        )));
    }
    if size > 64 << 40 {
        return Err(bad(format!(
            "the VHDX is damaged (a disk of {})",
            size_text(size)
        )));
    }
    // Payload entries, with a sector bitmap entry after every `chunk_ratio` of them.
    let chunk_ratio = (1u64 << 23) * logical_sector / block_size;
    let blocks = size.div_ceil(block_size);
    let bat_entries = if blocks == 0 {
        0
    } else {
        blocks + (blocks - 1) / chunk_ratio
    };
    if bat_entries * 8 > bat_len {
        return Err(bad(
            "the VHDX is damaged (its BAT is too small for the disk)",
        ));
    }
    Ok(Disk {
        size,
        block_size,
        logical_sector,
        bat_offset,
        bat_entries,
    })
}

struct Blocks {
    bat: Table,
    chunk_ratio: u64,
    block_size: u64,
    next: u64,
    blocks: u64,
}

impl Layout for Blocks {
    fn next(&mut self, src: &Source) -> io::Result<Option<Piece>> {
        if self.next >= self.blocks {
            return Ok(None);
        }
        let i = self.next;
        self.next += 1;
        let entry = self
            .bat
            .get(src, i + i / self.chunk_ratio)?
            .map_or(0, |b| le64(b, 0));
        let offset = entry & 0xffff_ffff_fff0_0000;
        Ok(Some(match entry & 7 {
            // Fully present.
            6 if offset >= MIB => Piece::stored(offset, self.block_size),
            // Not present, undefined, zero, unmapped (two spellings): zeros.
            0 | 1 | 2 | 3 | 5 => Piece::zeros(self.block_size),
            state => {
                return Err(bad(format!(
                    "the VHDX is damaged (block {i} is in state {state} at byte {offset})"
                )));
            }
        }))
    }
}

/// A change a log entry makes: file offset, length, the new bytes (None: zeros).
type Change = (u64, u64, Option<Box<[u8]>>);

/// A log entry that checked out.
struct Entry {
    seq: u64,
    /// Bytes it takes in the log.
    len: u64,
    tail: u64,
    flushed_file_offset: u64,
    last_file_offset: u64,
    /// Its changes, in order.
    changes: Vec<Change>,
}

/// Finds the active sequence of the log and turns it into patches over the file
/// (MS-VHDX, "Log Replay"). Returns them with the size the file should have.
fn replay_log(src: &Source, header: &Header) -> io::Result<(Vec<Patch>, u64)> {
    let len = header.log_length;
    if header.log_version != 0
        || header.log_offset < MIB
        || !header.log_offset.is_multiple_of(MIB)
        || !len.is_multiple_of(MIB)
    {
        return Err(bad("the VHDX is damaged (its log is out of place)"));
    }
    if len > 32 * MIB {
        return Err(unsupported(format!(
            "this VHDX wasn't closed properly, and its log ({}) is too big for DD-GUI to replay. Attach and detach it in Windows (Disk Management) first, then try again.",
            size_text(len)
        )));
    }
    let log = src.read_vec(header.log_offset, len as usize)?;
    let sector = 4 * KIB;
    // Entries as found at each offset (they may wrap around the end of the log). Checking
    // an entry reads all of it; a damaged log full of look-alike headers could make that
    // endless, so there's a budget.
    let mut budget = 16 * len.max(16 * MIB);
    let mut entry_at = |at: u64| {
        let found = parse_entry(&log, at, &header.log_guid, &mut budget);
        (budget > 0)
            .then_some(found)
            .ok_or_else(|| bad("the VHDX is damaged (its log can't be made sense of)"))
    };

    // The spec's scan: grow a sequence of consecutive entries from `tail`; keep the
    // valid one with the highest sequence number; move on past it (or one sector); stop
    // once the scan wraps around.
    let mut candidate: Option<Vec<(u64, Entry)>> = None;
    let mut candidate_seq = 0;
    let (mut tail, mut old_tail) = (0u64, 0u64);
    for _ in 0..=len / sector {
        let mut seq: Vec<(u64, Entry)> = Vec::new();
        let mut head = tail;
        let mut covered = 0;
        while covered < len {
            let Some(e) = entry_at(head)? else { break };
            if seq.last().is_some_and(|(_, last)| e.seq != last.seq + 1) {
                break;
            }
            covered += e.len;
            let next = (head + e.len) % len;
            seq.push((head, e));
            head = next;
        }
        // Valid if the newest entry's tail is one of the sequence's entries.
        let valid = seq
            .last()
            .is_some_and(|(_, last)| seq.iter().any(|(at, _)| *at == last.tail));
        if valid {
            let newest = seq.last().map_or(0, |(_, e)| e.seq);
            if newest > candidate_seq {
                candidate_seq = newest;
                candidate = Some(seq);
            }
            tail = head;
        } else {
            tail = (tail + sector) % len;
        }
        if tail <= old_tail {
            break;
        }
        old_tail = tail;
    }
    // No complete entry: nothing was logged before the writer stopped, so the file is as
    // it was (QEMU reads such files the same way).
    let Some(seq) = candidate else {
        return Ok((Vec::new(), src.size));
    };
    let Some((_, newest)) = seq.last() else {
        return Ok((Vec::new(), src.size));
    };
    if newest.flushed_file_offset > src.size {
        return Err(bad(format!(
            "the VHDX is cut short: its log says the file had {}, but it has {}",
            size_text(newest.flushed_file_offset),
            size_text(src.size)
        )));
    }
    let end = newest.last_file_offset;
    let first = newest.tail;
    let mut patches = Vec::new();
    let mut end_of_file = src.size.max(end);
    for (_, e) in seq.into_iter().skip_while(|(at, _)| *at != first) {
        for (offset, length, data) in e.changes {
            end_of_file = end_of_file.max(offset + length);
            patches.push(Patch {
                offset,
                len: length,
                data,
            });
        }
    }
    Ok((patches, end_of_file))
}

/// Reads and checks the log entry at `at` (MS-VHDX, "Log Entry").
fn parse_entry(log: &[u8], at: u64, log_guid: &[u8; 16], budget: &mut u64) -> Option<Entry> {
    const SECTOR: usize = 4096;
    let len = log.len() as u64;
    // Bytes of the circular log from `at`.
    let bytes = |from: u64, n: usize| -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut pos = from % len;
        while out.len() < n {
            let take = (n - out.len()).min((len - pos) as usize);
            out.extend_from_slice(&log[pos as usize..pos as usize + take]);
            pos = (pos + take as u64) % len;
        }
        out
    };
    let h = bytes(at, 64);
    if &h[..4] != b"loge" {
        return None;
    }
    let entry_len = le32(&h, 8) as u64;
    let tail = le32(&h, 12) as u64;
    let seq = le64(&h, 16);
    let descriptors = le32(&h, 24) as usize;
    if entry_len == 0
        || !entry_len.is_multiple_of(SECTOR as u64)
        || entry_len > len
        || !tail.is_multiple_of(SECTOR as u64)
        || tail >= len
        || seq == 0
        || le32(&h, 28) != 0
        || h[32..48] != *log_guid
    {
        return None;
    }
    // The header and descriptors fill whole sectors; one data sector follows per data
    // descriptor.
    let desc_sectors = (descriptors as u64 + 2).div_ceil(128);
    if desc_sectors * SECTOR as u64 > entry_len {
        return None;
    }
    *budget = budget.saturating_sub(entry_len);
    let mut entry = bytes(at, entry_len as usize);
    if !checksum_ok(&mut entry, 4) {
        return None;
    }
    let mut changes = Vec::new();
    let mut data_sector = desc_sectors as usize;
    for d in 0..descriptors {
        let desc = entry.get(64 + d * 32..96 + d * 32)?;
        let file_offset = le64(desc, 16);
        if le64(desc, 24) != seq || !file_offset.is_multiple_of(SECTOR as u64) {
            return None;
        }
        match &desc[..4] {
            b"zero" => {
                let zero_len = le64(desc, 8);
                if !zero_len.is_multiple_of(SECTOR as u64)
                    || le32(desc, 4) != 0
                    || zero_len > MAX_FILE
                    || file_offset > MAX_FILE
                {
                    return None;
                }
                changes.push((file_offset, zero_len, None));
            }
            b"desc" => {
                if file_offset > MAX_FILE {
                    return None;
                }
                let s = entry.get(data_sector * SECTOR..(data_sector + 1) * SECTOR)?;
                data_sector += 1;
                let data_seq = (le32(s, 4) as u64) << 32 | le32(s, 4092) as u64;
                if &s[..4] != b"data" || data_seq != seq {
                    return None;
                }
                // The first 8 and last 4 bytes of the sector are kept in the descriptor.
                let mut page = vec![0u8; SECTOR].into_boxed_slice();
                page[..8].copy_from_slice(&desc[8..16]);
                page[8..4092].copy_from_slice(&s[8..4092]);
                page[4092..].copy_from_slice(&desc[4..8]);
                changes.push((file_offset, SECTOR as u64, Some(page)));
            }
            _ => return None,
        }
    }
    Some(Entry {
        seq,
        len: entry_len,
        tail,
        flushed_file_offset: le64(&h, 48),
        last_file_offset: le64(&h, 56).min(MAX_FILE),
        changes,
    })
}
