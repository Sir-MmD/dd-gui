//! NTFS: `$Bitmap` holds one bit per cluster. A volume marked dirty, or whose `$LogFile`
//! still has changes to replay, gets copied in full: replaying the journal may allocate
//! clusters that the bitmap on disk calls free.

use super::util::{Bad, CHUNK, Mode, Part, Res, Usage, at, bit_runs, ensure, u16_at, u32_at, u64_at, utf16_label};

const DATA: u32 = 0x80;
const VOLUME_NAME: u32 = 0x60;
const VOLUME_INFORMATION: u32 = 0x70;
/// $LogFile, $Volume and $Bitmap.
const LOG_FILE: u64 = 2;
const VOLUME: u64 = 3;
const BITMAP: u64 = 6;

struct Vol {
    cluster: u64,
    clusters: u64,
}

/// A data run: `len` clusters from `vcn`, stored at `lcn` (None when sparse).
#[derive(Clone, Copy, Debug)]
struct Run {
    vcn: u64,
    len: u64,
    lcn: Option<u64>,
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let b = head.get(..512).ok_or_else(|| Bad("partition too small for NTFS".into()))?;
    ensure(at(b, 3, b"NTFS    "), "not NTFS")?;
    let bps = u64::from(u16_at(b, 11));
    ensure(bps.is_power_of_two() && (256..=4096).contains(&bps), "bad bytes per sector")?;
    // Above 128, sectors per cluster is a negative power of two (for clusters over 64 KiB).
    let spc = match b[13] {
        n @ 0..=128 => u64::from(n),
        n => 1u64.checked_shl(256 - u32::from(n)).unwrap_or(0),
    };
    ensure(spc.is_power_of_two(), "bad sectors per cluster")?;
    let cluster = bps.saturating_mul(spc);
    ensure(cluster <= 2 << 20, "clusters too large")?;
    let fat_fields = u16_at(b, 14) | u16::from(b[16]) | u16_at(b, 17) | u16_at(b, 19) | u16_at(b, 22);
    ensure(fat_fields == 0 && u32_at(b, 32) == 0, "FAT fields set in an NTFS boot sector")?;
    let sectors = u64_at(b, 40);
    let clusters = sectors / spc;
    ensure(clusters > 0, "empty volume")?;
    ensure(sectors.checked_mul(bps).is_some_and(|n| n <= p.size), "file system larger than its partition")?;
    let mft = u64_at(b, 48);
    ensure(mft < clusters, "MFT outside the volume")?;
    // Clusters per MFT record, or (when negative) the record size as a power of two.
    let record = match b[64] as i8 {
        n @ 1.. => u64::from(n.unsigned_abs()) * cluster,
        n @ -31..=-1 => 1 << n.unsigned_abs(),
        _ => 0,
    };
    ensure(record.is_power_of_two() && (512..=65536).contains(&record), "bad MFT record size")?;
    let v = Vol { cluster, clusters };

    // Record 0, $MFT itself, tells where the rest of the MFT is.
    let mut mft_record = p.read_vec(mft * cluster, record as usize)?;
    fixup(&mut mft_record, &[b"FILE"])?;
    let mft_runs = runs(data(&mft_record)?, &v)?;
    ensure(mft_runs.first().and_then(|r| r.lcn) == Some(mft), "MFT doesn't start where the boot sector says")?;
    let read_record = |p: &mut Part, n: u64| -> Res<Vec<u8>> {
        let mut r = vec![0; record as usize];
        read_stream(p, &v, &mft_runs, n * record, &mut r, None)?;
        fixup(&mut r, &[b"FILE"])?;
        ensure(u16_at(&r, 0x16) & 1 != 0, "system file record not in use")?;
        Ok(r)
    };

    let volume = read_record(p, VOLUME)?;
    *label = attribute(&volume, VOLUME_NAME)?.and_then(resident).and_then(utf16_label);
    if p.opts.mode == Mode::Copy {
        let info = attribute(&volume, VOLUME_INFORMATION)?.and_then(resident);
        let flags = info.filter(|i| i.len() >= 12).map(|i| u16_at(i, 10));
        ensure(flags.is_some_and(|f| f & 1 == 0), "volume marked dirty (needs chkdsk)")?;
        let log = read_record(p, LOG_FILE)?;
        ensure(log_is_clean(p, &v, &log)?, "$LogFile has changes to replay (not cleanly unmounted)")?;
    }

    let mut used = p.ranges();
    // $Boot: in the bitmap too, but never worth a doubt.
    used.add(0, (8 * 1024).max(cluster).min(p.size));
    let bitmap = read_record(p, BITMAP)?;
    let attr = data(&bitmap)?;
    let need = clusters.div_ceil(8);
    let mut mark = |map: &[u8], first_bit: u64| {
        let bits = (clusters - first_bit).min(map.len() as u64 * 8);
        bit_runs(map, bits, first_bit, |first, n| used.add(first * cluster, n * cluster));
    };
    if attr[8] == 0 {
        let map = resident(attr).ok_or_else(|| Bad("bad resident $Bitmap".into()))?;
        ensure(map.len() as u64 >= need, "$Bitmap too small")?;
        mark(map, 0);
    } else {
        let bitmap_runs = runs(attr, &v)?;
        let (size, initialized) = (u64_at(attr, 0x30), u64_at(attr, 0x38));
        ensure(size >= need, "$Bitmap too small")?;
        let mut done = 0;
        while done < need {
            let n = (CHUNK as u64).min(need - done);
            let mut map = vec![0; n as usize];
            // Sparse or uninitialized parts read as zeros; count them as used all the same.
            read_stream(p, &v, &bitmap_runs, done, &mut map, Some(0xFF))?;
            if initialized < done + n {
                map[initialized.saturating_sub(done) as usize..].fill(0xFF);
            }
            mark(&map, done * 8);
            done += n;
        }
    }
    Ok(Usage { used, end: clusters * cluster })
}

/// Undoes the update sequence ("fixups") of a multi-sector record, checking for torn writes.
fn fixup(buf: &mut [u8], magics: &[&[u8; 4]]) -> Res<()> {
    ensure(magics.iter().any(|m| at(buf, 0, *m)), "bad record magic")?;
    let offset = usize::from(u16_at(buf, 4));
    let count = usize::from(u16_at(buf, 6));
    let sane = count >= 2 && (count - 1) * 512 == buf.len() && offset % 2 == 0 && offset + 2 * count <= 512;
    ensure(sane, "bad update sequence")?;
    let usn = [buf[offset], buf[offset + 1]];
    for i in 1..count {
        let end = i * 512;
        ensure(buf[end - 2..end] == usn, "torn write in a metadata record")?;
        buf[end - 2] = buf[offset + 2 * i];
        buf[end - 1] = buf[offset + 2 * i + 1];
    }
    Ok(())
}

/// The first attribute of type `kind` in an MFT record (unnamed, for $DATA).
fn attribute(record: &[u8], kind: u32) -> Res<Option<&[u8]>> {
    let used = (u32_at(record, 0x18) as usize).min(record.len());
    let mut off = usize::from(u16_at(record, 0x14));
    loop {
        ensure(off + 4 <= used, "attributes run past the record")?;
        let t = u32_at(record, off);
        if t == 0xFFFF_FFFF {
            return Ok(None);
        }
        let len = u32_at(record, off + 4) as usize;
        ensure(len >= 24 && len.is_multiple_of(8) && off + len <= used, "bad attribute")?;
        let attr = &record[off..off + len];
        if t == kind && (kind != DATA || attr[9] == 0) {
            return Ok(Some(attr));
        }
        off += len;
    }
}

/// The unnamed $DATA attribute, which must be in the base record.
fn data(record: &[u8]) -> Res<&[u8]> {
    attribute(record, DATA)?.ok_or_else(|| Bad("$DATA not in the base record (attribute list)".into()))
}

/// A resident attribute's value.
fn resident(attr: &[u8]) -> Option<&[u8]> {
    let len = u32_at(attr, 0x10) as usize;
    let off = usize::from(u16_at(attr, 0x14));
    (attr[8] == 0).then(|| attr.get(off..off.checked_add(len)?)).flatten()
}

/// Decodes the data runs of a non-resident attribute that lies entirely in this record.
fn runs(attr: &[u8], v: &Vol) -> Res<Vec<Run>> {
    ensure(attr[8] == 1, "resident where non-resident was expected")?;
    // Compressed (0x00FF) or encrypted (0x4000) data isn't where the runs say.
    ensure(u16_at(attr, 0x0C) & 0x40FF == 0, "compressed or encrypted system file")?;
    ensure(u64_at(attr, 0x10) == 0, "attribute split over several records")?;
    let last = u64_at(attr, 0x18);
    let mut pos = usize::from(u16_at(attr, 0x20));
    let (mut vcn, mut lcn) = (0u64, 0i64);
    let mut out = Vec::new();
    loop {
        let header = *attr.get(pos).ok_or_else(|| Bad("truncated data runs".into()))?;
        if header == 0 {
            break;
        }
        let (len_size, off_size) = (usize::from(header & 0xF), usize::from(header >> 4));
        ensure((1..=8).contains(&len_size) && off_size <= 8, "bad data run")?;
        let field =
            attr.get(pos + 1..pos + 1 + len_size + off_size).ok_or_else(|| Bad("truncated data runs".into()))?;
        let len = field[..len_size].iter().rev().fold(0u64, |n, &b| n << 8 | u64::from(b));
        ensure(len > 0 && len <= v.clusters, "bad data run length")?;
        let run_lcn = if off_size == 0 {
            None
        } else {
            // A signed offset from the previous run's LCN.
            let raw = field[len_size..].iter().rev().fold(0u64, |n, &b| n << 8 | u64::from(b));
            let shift = 64 - 8 * off_size as u32;
            let delta = ((raw << shift) as i64) >> shift;
            lcn = lcn.checked_add(delta).filter(|&l| l >= 0).ok_or_else(|| Bad("bad data run offset".into()))?;
            let start = lcn as u64;
            ensure(start.checked_add(len).is_some_and(|end| end <= v.clusters), "data run outside the volume")?;
            Some(start)
        };
        out.push(Run { vcn, len, lcn: run_lcn });
        vcn = vcn.checked_add(len).ok_or_else(|| Bad("data runs overflow".into()))?;
        pos += 1 + len_size + off_size;
    }
    ensure(last.checked_add(1) == Some(vcn), "data runs don't match the attribute's size")?;
    Ok(out)
}

/// Reads `buf.len()` bytes at byte `pos` of an attribute's data. Sparse runs read as
/// `sparse`, or are an error when that's None.
fn read_stream(p: &mut Part, v: &Vol, runs: &[Run], pos: u64, buf: &mut [u8], sparse: Option<u8>) -> Res<()> {
    let mut done = 0;
    while done < buf.len() {
        let at = pos + done as u64;
        let vcn = at / v.cluster;
        let run = runs.iter().find(|r| r.vcn <= vcn && vcn - r.vcn < r.len);
        let run = run.ok_or_else(|| Bad("read past the end of an attribute".into()))?;
        let into_run = at - run.vcn * v.cluster;
        let n = (run.len * v.cluster - into_run).min((buf.len() - done) as u64) as usize;
        let out = &mut buf[done..done + n];
        match run.lcn {
            Some(lcn) => p.read_at(lcn * v.cluster + into_run, out)?,
            None => out.fill(sparse.ok_or_else(|| Bad("sparse system file".into()))?),
        }
        done += n;
    }
    Ok(())
}

/// Whether $LogFile says the volume was cleanly unmounted, like ntfs-3g checks it: empty
/// (all 0xFF, as mkntfs and ntfs-3g leave it), or the newer restart area marked clean.
fn log_is_clean(p: &mut Part, v: &Vol, record: &[u8]) -> Res<bool> {
    let attr = data(record)?;
    let log_runs = runs(attr, v)?;
    let size = u64_at(attr, 0x30);
    let mut empty = true;
    let mut newest: Option<(u64, bool)> = None;
    let mut pos = 0;
    while pos < size.min(64 * 1024) {
        let mut block = [0u8; 512];
        read_stream(p, v, &log_runs, pos, &mut block, Some(0))?;
        if u32_at(&block, 0) == 0xFFFF_FFFF {
            if !empty {
                break;
            }
        } else {
            empty = false;
            if at(&block, 0, b"RCRD") {
                break;
            }
            if (at(&block, 0, b"RSTR") || at(&block, 0, b"CHKD"))
                && let Some(area) = restart_area(p, v, &log_runs, pos, &block)?
            {
                newest = newest.max(Some(area));
                // The second restart page: done.
                if pos != 0 {
                    break;
                }
            }
        }
        pos = if pos == 0 { 512 } else { pos * 2 };
    }
    Ok(empty || newest.is_some_and(|(_, clean)| clean))
}

/// (current LSN, clean) of a valid restart page at `pos`.
fn restart_area(p: &mut Part, v: &Vol, runs: &[Run], pos: u64, block: &[u8]) -> Res<Option<(u64, bool)>> {
    let page_size = u64::from(u32_at(block, 0x10));
    let log_page = u64::from(u32_at(block, 0x14));
    let sane = |n: u64| n.is_power_of_two() && (512..=65536).contains(&n);
    if !sane(page_size) || !sane(log_page) || (pos != 0 && pos != page_size) {
        return Ok(None);
    }
    let mut page = vec![0; page_size as usize];
    read_stream(p, v, runs, pos, &mut page, Some(0))?;
    if fixup(&mut page, &[b"RSTR", b"CHKD"]).is_err() {
        return Ok(None);
    }
    let area = usize::from(u16_at(&page, 0x18));
    if area % 8 != 0 || area + 0x30 > page.len() {
        return Ok(None);
    }
    let lsn = u64_at(&page, area);
    let clients_in_use = u16_at(&page, area + 0x0C);
    let flags = u16_at(&page, area + 0x0E);
    Ok(Some((lsn, clients_in_use == 0xFFFF || flags & 0x0002 != 0)))
}
