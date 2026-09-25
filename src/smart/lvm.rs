//! LVM2 physical volumes: the volume group's metadata (text, in a ring buffer in each metadata
//! area) says which physical extents each logical volume uses. Extents no LV uses are free.
//!
//! A plain LV (visible, linear, all on this PV, nothing else built on it) is read like a
//! partition of its own, through a map of its extents, and what its file system uses is
//! mapped back. Everything else keeps all its extents here: thin pools, RAID and mirror
//! legs, snapshots and their origins, caches, striped LVs, LVs spanning other PVs.
//!
//! Kept: the label, the metadata areas, everything before the first extent and after the
//! last. The metadata goes by the copy with the highest sequence number whose checksums
//! match. A PV whose metadata isn't here (no metadata area, or ignored) is copied in full.

use super::util::{Bad, Disk, Part, Res, Usage, at, ensure, u32_at, u64_at};
use super::{Extent, partitions::Slot};
use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom};

/// An LVM2 label in one of the first four sectors.
pub(crate) fn detect(head: &[u8]) -> bool {
    (0..4).any(|s| at(head, s * 512, b"LABELONE") && at(head, s * 512 + 24, b"LVM2 001"))
}

/// The seed of LVM's CRC-32.
const INITIAL_CRC: u32 = 0xF597_A6CF;
const MDA_MAGIC: &[u8] = b" LVM2 x[5A%r0N*>";
const MDA_HEADER: u64 = 512;
const RAW_LOCN_IGNORED: u32 = 1;
const PV_EXT_USED: u32 = 1;
/// Largest metadata text read.
const MAX_TEXT: u64 = 16 << 20;
/// LVM inside an LV inside...: looked into this deep at most.
const MAX_NESTING: u32 = 3;

fn crc(data: &[u8]) -> u32 {
    super::util::crc32_le(INITIAL_CRC, data)
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// One more level of LVM being looked into, while it lives.
struct Nested;

impl Nested {
    fn enter() -> Option<Nested> {
        DEPTH.with(|d| (d.get() < MAX_NESTING).then(|| d.set(d.get() + 1)).map(|()| Nested))
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// What the label's PV header says.
struct Header {
    uuid: String,
    data_start: u64,
    /// Metadata areas and boot loader areas: (offset, size).
    areas: Vec<(u64, u64)>,
    mdas: Vec<(u64, u64)>,
    used_flag: bool,
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let Some(_nested) = Nested::enter() else {
        return Err(Bad("LVM nested too deeply".into()));
    };
    let header = pv_header(head)?;
    ensure(header.data_start > 0 && header.data_start <= p.size, "bad data area")?;
    let mut best: Option<(u64, Section)> = None;
    let mut ignored = false;
    for &(offset, size) in &header.mdas {
        match metadata(p, offset, size)? {
            Text::Ignored => ignored = true,
            Text::Empty => {}
            Text::Vg(seqno, vg) => {
                if best.as_ref().is_none_or(|(s, _)| seqno > *s) {
                    best = Some((seqno, vg));
                }
            }
        }
    }
    let mut used = p.ranges();
    used.add(0, header.data_start);
    for &(offset, size) in header.mdas.iter().chain(&header.areas) {
        used.add(offset, size);
    }
    let Some((_, vg)) = best else {
        // An orphan PV (in no VG) holds nothing; one whose metadata is elsewhere can't be told.
        ensure(!header.mdas.is_empty() && !ignored && !header.used_flag, "volume group metadata not on this PV")?;
        return Ok(Usage { used, end: p.size });
    };
    *label = Some(vg.name.clone());
    let vg = VolumeGroup::new(&vg, &header, p.size)?;
    let extent = vg.extent;
    let pv_end = header.data_start + vg.pe_count * extent;
    // No extent in two LVs, before anything is read through them.
    let mut claimed: Vec<(u64, u64)> = vg.lvs.iter().flat_map(|lv| lv.areas.iter().copied()).collect();
    claimed.sort_unstable();
    ensure(claimed.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "LVs overlap")?;
    for lv in &vg.lvs {
        if lv.simple {
            // Its extents, as (LV offset, PV offset, length) in bytes.
            let map: Vec<(u64, u64, u64)> = lv
                .areas
                .iter()
                .scan(0, |lv_at, &(pe, n)| {
                    let seg = (*lv_at, header.data_start + pe * extent, n * extent);
                    *lv_at += n * extent;
                    Some(seg)
                })
                .collect();
            for e in inside(p, &map)? {
                for (from, len) in map_back(&map, e) {
                    used.add(from, len);
                }
            }
        } else {
            for &(pe, n) in &lv.areas {
                used.add(header.data_start + pe * extent, n * extent);
            }
        }
    }
    Ok(Usage { used, end: pv_end })
}

fn pv_header(head: &[u8]) -> Res<Header> {
    let sector = (0..4).find(|&s| at(head, s * 512, b"LABELONE")).ok_or_else(|| Bad("no LVM label".into()))?;
    let label = &head[sector * 512..(sector + 1) * 512];
    ensure(u64_at(label, 8) == sector as u64 && at(label, 24, b"LVM2 001"), "bad LVM label")?;
    ensure(crc(&label[20..]) == u32_at(label, 16), "LVM label checksum mismatch")?;
    let mut at = u32_at(label, 20) as usize;
    ensure((32..=512 - 40).contains(&at), "bad LVM label")?;
    let uuid = String::from_utf8_lossy(&label[at..at + 32]).into_owned();
    at += 32 + 8;
    // Lists of (offset, size), each ending with a zero entry: data areas, metadata areas,
    // then (in the header extension) boot loader areas.
    let list = |at: &mut usize| -> Res<Vec<(u64, u64)>> {
        let mut v = Vec::new();
        loop {
            ensure(*at + 16 <= 512, "bad LVM label")?;
            let (offset, size) = (u64_at(label, *at), u64_at(label, *at + 8));
            *at += 16;
            if offset == 0 && size == 0 {
                return Ok(v);
            }
            v.push((offset, size));
        }
    };
    let data = list(&mut at)?;
    let mdas = list(&mut at)?;
    ensure(data.len() == 1, "not one data area")?;
    // The extension: version, flags, boot loader areas.
    let (mut areas, mut used_flag) = (Vec::new(), false);
    if at + 8 <= 512 && u32_at(label, at) >= 1 {
        used_flag = u32_at(label, at + 4) & PV_EXT_USED != 0;
        at += 8;
        if u32_at(label, at - 8) >= 2 {
            areas = list(&mut at)?;
        }
    }
    Ok(Header { uuid, data_start: data[0].0, areas, mdas, used_flag })
}

enum Text {
    /// The metadata area is marked to be ignored: the metadata's elsewhere.
    Ignored,
    /// No volume group.
    Empty,
    /// The volume group section, by sequence number.
    Vg(u64, Section),
}

/// Reads the committed metadata in the metadata area at `offset` (`size` bytes).
fn metadata(p: &mut Part, offset: u64, size: u64) -> Res<Text> {
    ensure(size > MDA_HEADER && offset.checked_add(size).is_some_and(|end| end <= p.size), "bad metadata area")?;
    let h = p.read_vec(offset, MDA_HEADER as usize)?;
    ensure(crc(&h[4..]) == u32_at(&h, 0) && at(&h, 4, MDA_MAGIC), "bad metadata area header")?;
    ensure(u32_at(&h, 20) == 1 && u64_at(&h, 24) == offset && u64_at(&h, 32) == size, "bad metadata area header")?;
    let (at, len, sum, flags) = (u64_at(&h, 40), u64_at(&h, 48), u32_at(&h, 56), u32_at(&h, 60));
    if flags & RAW_LOCN_IGNORED != 0 {
        return Ok(Text::Ignored);
    }
    if at == 0 && len == 0 {
        return Ok(Text::Empty);
    }
    ensure(
        at >= MDA_HEADER && at < size && len > 0 && len <= MAX_TEXT && len <= size - MDA_HEADER,
        "bad metadata location",
    )?;
    // A ring buffer after the header: the text may wrap around.
    let first = len.min(size - at);
    let mut text = p.read_vec(offset + at, first as usize)?;
    text.extend(p.read_vec(offset + MDA_HEADER, (len - first) as usize)?);
    ensure(crc(&text) == sum, "metadata checksum mismatch")?;
    let text = text.split(|&b| b == 0).next().unwrap_or_default();
    let top = parse(text)?;
    let mut sections = top.entries.into_iter().filter_map(|(k, item)| match item {
        Item::Section(mut s) => {
            s.name = k;
            Some(s)
        }
        Item::Value(_) => None,
    });
    let vg = sections.next().ok_or_else(|| Bad("no volume group in the metadata".into()))?;
    ensure(sections.next().is_none(), "several volume groups in the metadata")?;
    let seqno = vg.num("seqno").ok_or_else(|| Bad("metadata without a sequence number".into()))?;
    Ok(Text::Vg(seqno, vg))
}

/// An LV, as far as this PV goes: its extents here (first PE, count) in LV order.
struct Lv {
    areas: Vec<(u64, u64)>,
    /// Read like a partition of its own.
    simple: bool,
}

struct VolumeGroup {
    /// Extent size in bytes.
    extent: u64,
    pe_count: u64,
    lvs: Vec<Lv>,
}

impl VolumeGroup {
    fn new(vg: &Section, header: &Header, part_size: u64) -> Res<VolumeGroup> {
        if let Some(format) = vg.string("format") {
            ensure(format == "lvm2", "not LVM2 metadata")?;
        }
        let extent = vg.num("extent_size").and_then(|s| s.checked_mul(512)).filter(|&e| e > 0);
        let extent = extent.ok_or_else(|| Bad("bad extent size".into()))?;
        // This PV in the metadata, by UUID.
        let pvs = vg.section("physical_volumes").ok_or_else(|| Bad("no physical volumes".into()))?;
        let pv_names: Vec<&str> = pvs.sections().map(|s| s.name.as_str()).collect();
        let ours = pvs
            .sections()
            .find(|s| s.string("id").is_some_and(|id| id.replace('-', "") == header.uuid))
            .ok_or_else(|| Bad("this PV isn't in its volume group".into()))?;
        let pe_start = ours.num("pe_start").and_then(|s| s.checked_mul(512));
        ensure(pe_start == Some(header.data_start), "extents don't start where the label says")?;
        let pe_count = ours.num("pe_count").ok_or_else(|| Bad("no extent count".into()))?;
        let end = pe_count.checked_mul(extent).and_then(|n| n.checked_add(header.data_start));
        ensure(end.is_some_and(|end| end <= part_size), "extents past the end of the partition")?;
        let mut lvs = Vec::new();
        let empty = Section::default();
        let all = vg.section("logical_volumes").unwrap_or(&empty);
        let lv_names: Vec<&str> = all.sections().map(|s| s.name.as_str()).collect();
        // LVs other LVs are built on (pools, legs, origins, COW stores...).
        let mut referenced: Vec<&str> = Vec::new();
        for lv in all.sections() {
            lv.strings(&mut |s| {
                if s != lv.name && lv_names.contains(&s) {
                    referenced.push(s);
                }
            });
        }
        for lv in all.sections() {
            lvs.push(Self::lv(lv, &ours.name, &pv_names, &lv_names, &referenced, pe_count)?);
        }
        Ok(VolumeGroup { extent, pe_count, lvs })
    }

    /// An LV's extents on PV `pv`, and whether it's a plain one.
    fn lv(lv: &Section, pv: &str, pv_names: &[&str], lv_names: &[&str], referenced: &[&str], pe_count: u64) -> Res<Lv> {
        let status = lv.list_strings("status");
        let mut simple = status.contains(&"VISIBLE")
            && status.iter().all(|s| matches!(*s, "READ" | "WRITE" | "VISIBLE"))
            && !referenced.contains(&lv.name.as_str());
        let mut segments: Vec<(u64, u64, &Section)> = Vec::new();
        for seg in lv.sections() {
            let (start, count) = (seg.num("start_extent"), seg.num("extent_count"));
            let (Some(start), Some(count)) = (start, count) else {
                return Err(Bad("bad LV segment".into()));
            };
            ensure(count > 0, "empty LV segment")?;
            segments.push((start, count, seg));
        }
        ensure(lv.num("segment_count") == Some(segments.len() as u64), "LV segment count mismatch")?;
        segments.sort_unstable_by_key(|s| s.0);
        let mut areas = Vec::new();
        let mut next = 0u64;
        for (start, count, seg) in segments {
            // Contiguous from the first logical extent on.
            simple &= start == next;
            next = start.checked_add(count).ok_or_else(|| Bad("bad LV segment".into()))?;
            let kind = seg.string("type").ok_or_else(|| Bad("LV segment without a type".into()))?;
            match kind {
                // Areas on PVs, each (extent_count / stripes) long.
                "striped" => {
                    let stripes =
                        seg.num("stripe_count").filter(|&n| n > 0).ok_or_else(|| Bad("bad stripe count".into()))?;
                    ensure(count % stripes == 0, "bad striped segment")?;
                    let list = seg.list("stripes").ok_or_else(|| Bad("striped segment without stripes".into()))?;
                    let pairs = pairs(list)?;
                    ensure(pairs.len() as u64 == stripes, "bad striped segment")?;
                    simple &= stripes == 1;
                    for (name, pe) in pairs {
                        ensure(pv_names.contains(&name), "stripe on something that isn't a PV")?;
                        if name == pv {
                            areas.push(area(pe, count / stripes, pe_count)?);
                        } else {
                            simple = false;
                        }
                    }
                }
                // Mirror legs: LVs, or PVs while pvmove runs.
                "mirror" => {
                    simple = false;
                    if let Some(list) = seg.list("mirrors") {
                        for (name, offset) in pairs(list)? {
                            if name == pv {
                                areas.push(area(offset, count, pe_count)?);
                            } else {
                                ensure(pv_names.contains(&name) || lv_names.contains(&name), "unknown mirror leg")?;
                            }
                        }
                    }
                }
                // Built on other LVs only: those have the extents.
                k if k.starts_with("raid")
                    || matches!(
                        k,
                        "thin-pool"
                            | "thin"
                            | "snapshot"
                            | "cache"
                            | "cache-pool"
                            | "writecache"
                            | "integrity"
                            | "vdo"
                            | "vdo-pool"
                            | "zero"
                            | "error"
                    ) =>
                {
                    simple = false;
                }
                _ => return Err(Bad(format!("unknown LV segment type {kind:?}"))),
            }
        }
        Ok(Lv { simple: simple && !areas.is_empty(), areas })
    }
}

/// (name, number) pairs of an areas list.
fn pairs(list: &[Value]) -> Res<Vec<(&str, u64)>> {
    ensure(list.len().is_multiple_of(2), "bad areas list")?;
    list.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| match pair {
            [Value::Str(name), Value::Num(n)] => Ok((name.as_str(), *n)),
            _ => Err(Bad("bad areas list".into())),
        })
        .collect()
}

fn area(pe: u64, count: u64, pe_count: u64) -> Res<(u64, u64)> {
    ensure(pe.checked_add(count).is_some_and(|end| end <= pe_count), "LV past the end of its PV")?;
    Ok((pe, count))
}

/// Maps an extent of an LV (per `map`: LV offset, PV offset, length) onto the PV.
fn map_back(map: &[(u64, u64, u64)], e: Extent) -> Vec<(u64, u64)> {
    let end = e.start.saturating_add(e.len);
    map.iter()
        .filter_map(|&(lv, pv, len)| {
            let (from, to) = (e.start.max(lv), end.min(lv + len));
            (from < to).then(|| (pv + (from - lv), to - from))
        })
        .collect()
}

/// What the file system on a plain LV uses, in LV offsets.
fn inside(p: &mut Part, map: &[(u64, u64, u64)]) -> Res<Vec<Extent>> {
    let size = map.last().map_or(0, |&(lv, _, len)| lv + len);
    let opts = p.opts;
    let mut reader = LvReader { part: p, map, pos: 0, size };
    let mut disk = Disk::new(&mut reader, size);
    let found = super::partition(&mut disk, &Slot { index: 0, start: 0, size }, opts);
    Ok(found.used)
}

/// An LV read through the extents it has on the PV.
struct LvReader<'r, 'p, 'd> {
    part: &'r mut Part<'p, 'd>,
    map: &'r [(u64, u64, u64)],
    pos: u64,
    size: u64,
}

impl Read for LvReader<'_, '_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let i = self.map.partition_point(|&(lv, _, len)| lv + len <= self.pos);
        let Some(&(lv, pv, len)) = self.map.get(i) else {
            return Ok(0);
        };
        let n = (buf.len() as u64).min(lv + len - self.pos) as usize;
        self.part.read_at(pv + (self.pos - lv), &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for LvReader<'_, '_, '_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

// The metadata's text format: `name = value` and `name { ... }`, values being strings,
// numbers or lists of them; `#` starts a comment.

#[derive(Debug)]
enum Value {
    Str(String),
    Num(u64),
    /// Negative or fractional: not needed here.
    Other,
    List(Vec<Value>),
}

#[derive(Debug)]
enum Item {
    Value(Value),
    Section(Section),
}

#[derive(Debug, Default)]
struct Section {
    name: String,
    entries: Vec<(String, Item)>,
}

impl Section {
    fn value(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find_map(|(k, v)| match v {
            Item::Value(v) if k == key => Some(v),
            _ => None,
        })
    }

    fn num(&self, key: &str) -> Option<u64> {
        match self.value(key)? {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn string(&self, key: &str) -> Option<&str> {
        match self.value(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    fn list(&self, key: &str) -> Option<&[Value]> {
        match self.value(key)? {
            Value::List(v) => Some(v),
            _ => None,
        }
    }

    fn list_strings(&self, key: &str) -> Vec<&str> {
        self.list(key)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| match v {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn section(&self, key: &str) -> Option<&Section> {
        self.sections().find(|s| s.name == key)
    }

    fn sections(&self) -> impl Iterator<Item = &Section> {
        self.entries.iter().filter_map(|(_, v)| match v {
            Item::Section(s) => Some(s),
            Item::Value(_) => None,
        })
    }

    /// Calls `f` with every string value in the section, however deep.
    fn strings<'a>(&'a self, f: &mut impl FnMut(&'a str)) {
        fn walk<'a>(v: &'a Value, f: &mut impl FnMut(&'a str)) {
            match v {
                Value::Str(s) => f(s),
                Value::List(items) => items.iter().for_each(|v| walk(v, f)),
                _ => {}
            }
        }
        for (_, item) in &self.entries {
            match item {
                Item::Value(v) => walk(v, f),
                Item::Section(s) => s.strings(f),
            }
        }
    }
}

const MAX_DEPTH: usize = 16;
const MAX_ITEMS: usize = 1 << 20;

struct Parser<'t> {
    text: &'t [u8],
    at: usize,
    items: usize,
}

fn parse(text: &[u8]) -> Res<Section> {
    let mut parser = Parser { text, at: 0, items: 0 };
    let top = parser.section(0)?;
    parser.skip();
    ensure(parser.at == text.len(), "bad metadata text")?;
    Ok(top)
}

impl Parser<'_> {
    fn bad<T>(&self) -> Res<T> {
        Err(Bad(format!("bad metadata text at byte {}", self.at)))
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    /// Skips white space and comments.
    fn skip(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'#' {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.at += 1;
                }
            } else if c.is_ascii_whitespace() {
                self.at += 1;
            } else {
                break;
            }
        }
    }

    fn word(&mut self) -> &[u8] {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || b"_.+-".contains(&c)) {
            self.at += 1;
        }
        &self.text[start..self.at]
    }

    /// Entries up to a closing brace (or the end, at the top).
    fn section(&mut self, depth: usize) -> Res<Section> {
        if depth > MAX_DEPTH {
            return self.bad();
        }
        let mut s = Section::default();
        loop {
            self.skip();
            match self.peek() {
                None if depth == 0 => return Ok(s),
                Some(b'}') if depth > 0 => {
                    self.at += 1;
                    return Ok(s);
                }
                None | Some(b'}') => return self.bad(),
                _ => {}
            }
            self.items += 1;
            if self.items > MAX_ITEMS {
                return self.bad();
            }
            let key = String::from_utf8_lossy(self.word()).into_owned();
            if key.is_empty() {
                return self.bad();
            }
            self.skip();
            match self.peek() {
                Some(b'{') => {
                    self.at += 1;
                    let mut inner = self.section(depth + 1)?;
                    inner.name = key.clone();
                    s.entries.push((key, Item::Section(inner)));
                }
                Some(b'=') => {
                    self.at += 1;
                    let v = self.value(depth)?;
                    s.entries.push((key, Item::Value(v)));
                }
                _ => return self.bad(),
            }
        }
    }

    fn value(&mut self, depth: usize) -> Res<Value> {
        self.skip();
        match self.peek() {
            Some(b'"') => {
                self.at += 1;
                let mut s = Vec::new();
                loop {
                    match self.peek() {
                        None => return self.bad(),
                        Some(b'"') => break,
                        Some(b'\\') => {
                            self.at += 1;
                            s.extend(self.peek());
                        }
                        Some(c) => s.push(c),
                    }
                    self.at += 1;
                }
                self.at += 1;
                Ok(Value::Str(String::from_utf8_lossy(&s).into_owned()))
            }
            Some(b'[') => {
                if depth > MAX_DEPTH {
                    return self.bad();
                }
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip();
                    if self.peek() == Some(b']') {
                        self.at += 1;
                        return Ok(Value::List(items));
                    }
                    if !items.is_empty() {
                        if self.peek() != Some(b',') {
                            return self.bad();
                        }
                        self.at += 1;
                    }
                    self.items += 1;
                    if self.items > MAX_ITEMS {
                        return self.bad();
                    }
                    items.push(self.value(depth + 1)?);
                }
            }
            Some(c) if c.is_ascii_digit() || c == b'-' => {
                let word = String::from_utf8_lossy(self.word()).into_owned();
                Ok(word.parse().map(Value::Num).unwrap_or(Value::Other))
            }
            _ => self.bad(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_text() {
        let text = br#"vg1 {
id = "abc"
seqno = 7
status = ["READ", "WRITE"] # comment
flags = []
x = -3
y = 0.5
physical_volumes {
pv0 { id = "a-b"
pe_count = 10 }
}
}
# Generated
contents = "Text Format Volume Group"
description = "quoted \" and \\ inside"
"#;
        let top = parse(text).unwrap();
        let vg = top.sections().next().unwrap();
        assert_eq!(vg.name, "vg1");
        assert_eq!(vg.num("seqno"), Some(7));
        assert_eq!(vg.list_strings("status"), ["READ", "WRITE"]);
        assert_eq!(vg.section("physical_volumes").unwrap().section("pv0").unwrap().num("pe_count"), Some(10));
        assert_eq!(top.string("description"), Some("quoted \" and \\ inside"));
        for bad in [&b"a {"[..], b"a = [1, 2", b"a = \"x", b"}", b"a b", b"= 1", b"a = [1 2]"] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // Deep nesting is refused, not a stack overflow.
        let deep = "a {".repeat(100_000);
        assert!(parse(deep.as_bytes()).is_err());
        let deep = format!("a = {}", "[".repeat(100_000));
        assert!(parse(deep.as_bytes()).is_err());
    }
}
