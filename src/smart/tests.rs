//! Layouts of real file system images, made with the system's tools, are checked against the
//! tools' own numbers. Then "smart copies" (only the extents copied, everything else filled
//! with garbage) must pass fsck and give back every file byte for byte.
//!
//! Tests whose tools are missing say so and pass. Images go to the system's temp directory,
//! or to `DD_GUI_SMART_SCRATCH`; `DD_GUI_SMART_KEEP=1` keeps them. Needing root for a loop
//! mount, the exFAT test with files is ignored by default:
//! `cargo test smart -- --ignored exfat_with_files` (sudo asks for a password on the terminal;
//! without one, set `DD_GUI_TEST_SUDO_PASSWORD`).

use super::*;
use std::io::Cursor;
use util::{ALIGN, GAP, MAX_EXTENTS};

/// Deterministic pseudo-random numbers (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    /// Usually `sane`, sometimes anything at all.
    fn wild(&mut self, sane: u64) -> u64 {
        if self.below(5) == 0 { self.next() >> self.below(64) } else { sane }
    }
}

/// What every layout promises, whatever the input.
fn check_invariants(l: &Layout) {
    let mut prev_end: Option<u64> = None;
    for e in &l.extents {
        let end = e.start + e.len;
        assert!(e.len > 0 && e.start % ALIGN == 0, "unaligned extent {e:?}");
        assert!(end <= l.size && (end % ALIGN == 0 || end == l.size), "extent {e:?} past {}", l.size);
        if let Some(prev) = prev_end {
            assert!(e.start >= prev + GAP, "extents not merged: {prev} then {e:?}");
        }
        prev_end = Some(end);
    }
    assert!(l.extents.len() <= MAX_EXTENTS);
    for p in &l.partitions {
        assert!(p.start + p.size <= l.size, "partition past the end: {p:?}");
        assert!(p.used <= p.size);
        if !p.understood {
            assert_eq!(p.used, p.size, "a partition that isn't understood must be copied in full: {p:?}");
        }
    }
}

fn analyze_bytes(data: &[u8], size: u64) -> io::Result<Layout> {
    analyze(&mut Cursor::new(data), size)
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(7);
    for size in [0usize, 1, 511, 512, 513, 4096, 70_000, 1 << 20, (3 << 20) + 17] {
        for round in 0..4 {
            let mut data = rng.bytes(size);
            if round % 2 == 1 && size >= 512 {
                // Look like an MBR, so the tables get parsed.
                data[510] = 0x55;
                data[511] = 0xAA;
                for i in 0..4 {
                    data[0x1BE + 16 * i] = 0;
                }
            }
            let layout = analyze_bytes(&data, size as u64).unwrap();
            check_invariants(&layout);
            // Nothing recognisable: everything is copied.
            if layout.table == Table::None {
                assert_eq!(layout.used(), size as u64);
            }
        }
    }
}

#[test]
fn reading_past_the_end_is_an_error_or_conservative() {
    // The drive claims to be bigger than what can be read.
    let data = vec![0u8; 4096];
    if let Ok(layout) = analyze_bytes(&data, 1 << 30) {
        check_invariants(&layout);
        assert_eq!(layout.used(), 1 << 30);
    }
}

/// An MBR (or EBR) sector with the given entries: (boot, type, start, sectors).
fn mbr_sector(entries: &[(u8, u8, u32, u32)]) -> [u8; 512] {
    let mut s = [0u8; 512];
    for (i, &(boot, kind, start, sectors)) in entries.iter().enumerate() {
        let e = 0x1BE + 16 * i;
        s[e] = boot;
        s[e + 4] = kind;
        s[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
        s[e + 12..e + 16].copy_from_slice(&sectors.to_le_bytes());
    }
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

#[test]
fn ebr_loops_and_wild_entries_terminate() {
    let size = 64u64 << 20;
    let mut data = vec![0u8; size as usize];
    // A primary partition past the end, one crossing it, and an extended partition whose
    // EBR links back to itself.
    let mbr = mbr_sector(&[(0, 0x83, 200_000, 100), (0, 0x83, 120_000, 100_000), (0, 0x05, 2048, 20_000)]);
    data[..512].copy_from_slice(&mbr);
    let ebr = mbr_sector(&[(0, 0x83, 2048, 1000), (0, 0x05, 0, 20_000), (0, 0x83, 1, u32::MAX), (0, 0x83, 1, 1)]);
    data[2048 * 512..2049 * 512].copy_from_slice(&ebr);
    let layout = analyze_bytes(&data, size).unwrap();
    check_invariants(&layout);
    assert_eq!(layout.table, Table::Mbr);
    let indexes: Vec<u32> = layout.partitions.iter().map(|p| p.index).collect();
    // The partition past the end is gone, the crossing one is cut at the end, the loop ran once.
    assert_eq!(indexes, vec![2, 5, 6]);
    let crossing = &layout.partitions[0];
    assert_eq!(crossing.start + crossing.size, size);
    assert!(layout.partitions.iter().all(|p| !p.understood));
}

/// GPTs with random fields but valid checksums, so the values reach the parser.
#[test]
fn random_gpts_never_panic() {
    let mut rng = Rng(0x6970);
    let mut parsed = 0;
    for round in 0..2000 {
        let ss: usize = if rng.below(4) == 0 { 4096 } else { 512 };
        let size = (8 * ss as u64) + rng.below(2 << 20);
        let mut data = vec![0u8; size as usize];
        data[..512].copy_from_slice(&mbr_sector(&[(0, 0xEE, 1, u32::MAX)]));
        let sectors = size / ss as u64;
        let count = rng.below(200) + 1;
        let count = rng.wild(count) as u32;
        let entry_size = [128u32, 256, 512, 96, 0, 128 * 3][rng.below(6) as usize];
        let entries_lba = rng.wild(2);
        let first = rng.below(8) + 2;
        let first = rng.wild(first);
        let last = sectors.saturating_sub(1 + rng.below(40));
        let last = rng.wild(last);
        let mut entries = vec![0u8; (u64::from(count) * u64::from(entry_size)).min(1 << 20) as usize];
        for e in entries.chunks_exact_mut(entry_size.max(1) as usize) {
            if rng.below(3) > 0 && e.len() >= 56 {
                e[..16].copy_from_slice(&rng.bytes(16));
                let a = rng.below(sectors + 4);
                let a = rng.wild(a);
                let b = a.saturating_add(rng.below(sectors / 2 + 2));
                let b = rng.wild(b);
                e[32..40].copy_from_slice(&a.to_le_bytes());
                e[40..48].copy_from_slice(&b.to_le_bytes());
            }
        }
        let mut h = [0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        let alternate = rng.wild(sectors - 1);
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&first.to_le_bytes());
        h[48..56].copy_from_slice(&last.to_le_bytes());
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&count.to_le_bytes());
        h[84..88].copy_from_slice(&entry_size.to_le_bytes());
        h[88..92].copy_from_slice(&util::crc32(&entries).to_le_bytes());
        let crc = util::crc32(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        data[ss..ss + 92].copy_from_slice(&h);
        if let Some(at) = entries_lba.checked_mul(ss as u64).filter(|&at| at + entries.len() as u64 <= size) {
            data[at as usize..at as usize + entries.len()].copy_from_slice(&entries);
        }
        match analyze_bytes(&data, size) {
            Ok(layout) => {
                check_invariants(&layout);
                parsed += usize::from(layout.table == Table::Gpt && !layout.partitions.is_empty());
            }
            Err(err) => panic!("round {round}: {err}"),
        }
    }
    // Not a vacuous test: plenty of these made it through the checksums and sanity checks.
    assert!(parsed > 200, "only {parsed} GPTs parsed");
}

/// MBRs and EBR chains with random entries.
#[test]
fn random_mbrs_never_panic() {
    let mut rng = Rng(0x3B2);
    let size = 16u64 << 20;
    let sectors = size / 512;
    let mut data = vec![0u8; size as usize];
    for round in 0..2000 {
        let entry = |rng: &mut Rng| {
            let kind = [0x83, 0x05, 0x0F, 0x85, 0x07, 0x0C, 0xEE, 0][rng.below(8) as usize];
            let start = if rng.below(6) == 0 { rng.next() as u32 } else { rng.below(sectors) as u32 };
            let len = if rng.below(6) == 0 { rng.next() as u32 } else { rng.below(sectors) as u32 };
            (if rng.below(10) == 0 { 0x80 } else { 0 }, kind, start, len)
        };
        let mbr: Vec<_> = (0..4).map(|_| entry(&mut rng)).collect();
        data[..512].copy_from_slice(&mbr_sector(&mbr));
        // EBRs wherever extended entries may point.
        for _ in 0..rng.below(20) {
            let at = rng.below(sectors - 1) as usize * 512;
            let ebr: Vec<_> = (0..rng.below(5)).map(|_| entry(&mut rng)).collect();
            data[at..at + 512].copy_from_slice(&mbr_sector(&ebr));
        }
        match analyze_bytes(&data, size) {
            Ok(layout) => check_invariants(&layout),
            Err(err) => panic!("round {round}: {err}"),
        }
    }
}

#[test]
fn empty_partition_table_copies_only_the_edges() {
    let size = 32u64 << 20;
    let mut data = vec![0u8; size as usize];
    data[..512].copy_from_slice(&mbr_sector(&[]));
    let layout = analyze_bytes(&data, size).unwrap();
    check_invariants(&layout);
    assert_eq!(layout.table, Table::Mbr);
    assert!(layout.partitions.is_empty());
    assert_eq!(layout.used(), 2 << 20);
}

#[test]
fn layout_serializes_for_the_gui() {
    let layout = Layout {
        size: 1 << 20,
        table: Table::Gpt,
        partitions: vec![Partition {
            index: 1,
            start: 4096,
            size: 8192,
            fs: Some("vfat".into()),
            label: None,
            used: 4096,
            understood: true,
        }],
        extents: vec![Extent { start: 0, len: 4096 }],
    };
    let json = serde_json::to_value(&layout).unwrap();
    assert_eq!(json["table"], "gpt");
    assert_eq!(json["partitions"][0]["fs"], "vfat");
    assert_eq!(json["partitions"][0]["label"], serde_json::Value::Null);
    assert_eq!(json["extents"][0]["len"], 4096);
    assert_eq!(serde_json::to_value(Table::None).unwrap(), "none");
    assert_eq!(serde_json::to_value(Table::Mbr).unwrap(), "mbr");
}

#[test]
fn limit_extents_keeps_everything() {
    let mut layout = Layout {
        size: 100 << 20,
        table: Table::None,
        partitions: vec![Partition {
            index: 0,
            start: 0,
            size: 100 << 20,
            fs: None,
            label: None,
            used: 0,
            understood: true,
        }],
        extents: (0..100).map(|i| Extent { start: i << 20, len: 4096 }).collect(),
    };
    let before = layout.extents.clone();
    layout.limit_extents(10);
    assert!(layout.extents.len() <= 10);
    for e in before {
        assert_eq!(util::overlap(&layout.extents, e.start, e.len), e.len);
    }
    assert_eq!(layout.partitions[0].used, layout.used());
}

/// Prints the layout of the image in `SMART_IMAGE`, with the reasons behind every full copy:
/// `SMART_IMAGE=disk.img cargo test inspect -- --ignored --nocapture`
#[test]
#[ignore]
fn inspect() {
    let Ok(path) = std::env::var("SMART_IMAGE") else { return };
    let mut f = std::fs::File::open(&path).unwrap();
    let size = f.metadata().unwrap().len();
    let mode = if std::env::var_os("SMART_ESTIMATE").is_some() { Mode::Estimate } else { Mode::Copy };
    let t = std::time::Instant::now();
    let (layout, notes) = run(&mut f, size, Opts::new(mode)).unwrap();
    println!(
        "{path} {:?}: {} of {} bytes in {} extents ({:?})",
        layout.table,
        layout.used(),
        size,
        layout.extents.len(),
        t.elapsed()
    );
    for p in &layout.partitions {
        println!("  {p:?}");
    }
    for n in notes {
        println!("  note: {n}");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const MB: u64 = 1 << 20;

    /// A directory for one test's images, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let base = std::env::var_os("DD_GUI_SMART_SCRATCH")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("dd-gui-smart-tests"));
            let dir = base.join(format!("{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if std::env::var_os("DD_GUI_SMART_KEEP").is_none() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn on_path(tool: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(tool).is_file()))
    }

    /// True when all the tools are installed; says which aren't otherwise.
    fn have(tools: &[&str]) -> bool {
        let missing: Vec<_> = tools.iter().filter(|t| !on_path(t)).collect();
        if !missing.is_empty() {
            eprintln!("skipped: {missing:?} not installed");
        }
        missing.is_empty()
    }

    /// Whether `mkfs` accepts `args` (older versions lack some options): tried on a scratch file.
    fn supports(s: &Scratch, mkfs: &str, args: &[&str], size: u64) -> bool {
        let img = s.join("probe.img");
        blank(&img, size);
        let mut all = args.to_vec();
        all.push(p(&img));
        let (ok, out) = try_sh(mkfs, &all);
        let _ = fs::remove_file(&img);
        if !ok {
            eprintln!("skipped: {mkfs} {args:?} not supported here: {out}");
        }
        ok
    }

    fn try_sh(cmd: &str, args: &[&str]) -> (bool, String) {
        let out = Command::new(cmd).args(args).stdin(Stdio::null()).output().unwrap_or_else(|e| panic!("{cmd}: {e}"));
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    /// Runs a command, panicking with its output unless it succeeds.
    fn sh(cmd: &str, args: &[&str]) -> String {
        let (ok, text) = try_sh(cmd, args);
        assert!(ok, "{cmd} {args:?} failed:\n{text}");
        text
    }

    fn p(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    fn blank(path: &Path, size: u64) {
        File::create(path).unwrap().set_len(size).unwrap();
    }

    fn file_size(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    /// Copies all of `src` into `img` at `offset`, like `dd conv=notrunc seek=…`.
    fn put(img: &Path, offset: u64, src: &Path) {
        let data = fs::read(src).unwrap();
        let mut f = fs::OpenOptions::new().write(true).open(img).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(&data).unwrap();
    }

    fn put_bytes(img: &Path, offset: u64, data: &[u8]) {
        let mut f = fs::OpenOptions::new().write(true).open(img).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(data).unwrap();
    }

    fn read_range(img: &Path, start: u64, len: u64) -> Vec<u8> {
        let mut f = File::open(img).unwrap();
        f.seek(SeekFrom::Start(start)).unwrap();
        let mut buf = vec![0; len as usize];
        f.read_exact(&mut buf).unwrap();
        buf
    }

    /// Files (path → content) for a file system.
    type Tree = BTreeMap<String, Vec<u8>>;

    /// `count` files of mixed sizes (up to `max`) in a few directories, names prefixed with `tag`.
    fn make_tree(rng: &mut Rng, tag: &str, count: usize, max: u64) -> Tree {
        (0..count)
            .map(|i| {
                let size = match rng.below(10) {
                    0..=2 => rng.below(600),
                    3..=5 => 600 + rng.below(20_000),
                    6..=8 => 20_000 + rng.below(300_000),
                    _ => rng.below(max),
                }
                .min(max);
                let dir = match i % 5 {
                    0 => String::new(),
                    n => format!("dir{n}/"),
                };
                (format!("{dir}{tag}{i}.bin"), rng.bytes(size as usize))
            })
            .collect()
    }

    fn write_tree(tree: &Tree, dir: &Path) {
        for (name, data) in tree {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
    }

    fn read_tree(dir: &Path, prefix: &str, out: &mut Tree) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                if name != "lost+found" {
                    read_tree(&path, &format!("{prefix}{name}/"), out);
                }
            } else {
                out.insert(format!("{prefix}{name}"), fs::read(path).unwrap());
            }
        }
    }

    /// Err describes the first difference between the files in `dir` and `expected`.
    fn compare(expected: &Tree, dir: &Path) -> Result<(), String> {
        let mut found = Tree::new();
        read_tree(dir, "", &mut found);
        for (name, data) in expected {
            match found.get(name) {
                None => return Err(format!("{name} missing")),
                Some(d) if d != data => return Err(format!("{name} differs")),
                _ => {}
            }
        }
        match found.keys().find(|k| !expected.contains_key(*k)) {
            Some(extra) => Err(format!("unexpected file {extra}")),
            None => Ok(()),
        }
    }

    /// The smart copy: a file of the same size where only `extents` come from `src`, and
    /// everything else is garbage (0xA5), which is harsher than zeros.
    fn smart_copy(src: &Path, extents: &[Extent], dst: &Path) {
        let size = file_size(src);
        let mut out = File::create(dst).unwrap();
        let fill = vec![0xA5u8; MB as usize];
        let mut left = size;
        while left > 0 {
            let n = left.min(MB);
            out.write_all(&fill[..n as usize]).unwrap();
            left -= n;
        }
        let mut input = File::open(src).unwrap();
        let mut buf = vec![0u8; 4 * MB as usize];
        for e in extents {
            let mut done = 0;
            while done < e.len {
                let n = (e.len - done).min(buf.len() as u64) as usize;
                input.seek(SeekFrom::Start(e.start + done)).unwrap();
                input.read_exact(&mut buf[..n]).unwrap();
                out.seek(SeekFrom::Start(e.start + done)).unwrap();
                out.write_all(&buf[..n]).unwrap();
                done += n as u64;
            }
        }
    }

    fn layout_of(img: &Path) -> Layout {
        let mut f = File::open(img).unwrap();
        let size = f.metadata().unwrap().len();
        let layout = analyze(&mut f, size).unwrap();
        check_invariants(&layout);
        layout
    }

    fn estimate_of(img: &Path) -> Layout {
        let mut f = File::open(img).unwrap();
        let size = f.metadata().unwrap().len();
        estimate(&mut f, size).unwrap()
    }

    /// What one partition's file system uses, exactly (no merging, no alignment, no drive
    /// edges): its ranges plus the head and tail copied with it. None with the reason when
    /// it isn't understood.
    fn fs_usage(img: &Path, start: u64, size: u64) -> Result<Vec<Extent>, String> {
        let mut f = File::open(img).unwrap();
        let total = f.metadata().unwrap().len();
        let mut disk = Disk::new(&mut f, total);
        let slot = partitions::Slot { index: 1, start, size };
        let found = partition(&mut disk, &slot, Opts { mode: Mode::Copy, gap: 0, align: 1 });
        match found.why {
            Some(why) => Err(why),
            None => Ok(util::normalize(found.used, total, 0, 1)),
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Fat,
        Exfat,
        Ntfs,
        Ext,
    }

    /// A file system image and the files it should hold.
    struct Fixture {
        img: PathBuf,
        kind: Kind,
        files: Tree,
    }

    /// The first number after `key` in the tool's output.
    fn number_after(text: &str, key: &str) -> Option<u64> {
        let at = text.find(key)? + key.len();
        text[at..].trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    }

    /// (bytes the file system uses, bytes its allocation map covers), by the tools' own count.
    /// None when the tool's output isn't what these tests know (older versions).
    fn tool_numbers(kind: Kind, img: &Path) -> Option<(u64, u64)> {
        match kind {
            Kind::Fat => {
                let (_, out) = try_sh("fsck.fat", &["-n", "-v", p(img)]);
                let data = number_after(&out, "Data area starts at byte")?;
                let line = out.lines().find(|l| l.trim_end().ends_with("bytes per cluster"))?;
                let cluster: u64 = line.split_whitespace().next()?.parse().ok()?;
                let counts = out.lines().rev().find(|l| l.contains(" files, "))?;
                let counts = counts.rsplit(", ").next()?.trim_end_matches(" clusters");
                let (used, total) = counts.split_once('/')?;
                let (used, total): (u64, u64) = (used.parse().ok()?, total.parse().ok()?);
                Some((data + used * cluster, data + total * cluster))
            }
            Kind::Exfat => {
                let out = sh("dump.exfat", &[p(img)]);
                let heap = number_after(&out, "Cluster Heap Offset (sector offset):")?
                    * number_after(&out, "Bytes per Sector:")?;
                let cluster = number_after(&out, "Cluster size:")?;
                let total = number_after(&out, "Total Clusters:")?;
                let free = number_after(&out, "Free Clusters:")?;
                Some((heap + (total - free) * cluster, heap + total * cluster))
            }
            Kind::Ntfs => {
                let out = sh("ntfsinfo", &["-m", p(img)]);
                let cluster = number_after(&out, "Cluster Size:")?;
                let total = number_after(&out, "Volume Size in Clusters:")?;
                let free = number_after(&out, "Free Clusters:")?;
                Some(((total - free) * cluster, total * cluster))
            }
            Kind::Ext => {
                let out = sh("dumpe2fs", &["-h", p(img)]);
                let block = number_after(&out, "Block size:")?;
                let blocks = number_after(&out, "Block count:")?;
                let free = number_after(&out, "Free blocks:")?;
                // Block 0 counts as used with 1 KiB blocks too (it's in the block count, never free).
                Some(((blocks - free) * block, blocks * block))
            }
        }
    }

    /// Free block ranges (inclusive) per dumpe2fs, block size.
    fn ext_free_blocks(img: &Path) -> (Vec<(u64, u64)>, u64) {
        let out = sh("dumpe2fs", &[p(img)]);
        let block = number_after(&out, "Block size:").unwrap();
        let mut free = Vec::new();
        // Per group (indented); the superblock's total has the same name.
        for line in out.lines().filter_map(|l| l.strip_prefix("  Free blocks: ")) {
            for range in line.split(", ").filter(|r| !r.trim().is_empty()) {
                let (a, b) = range.split_once('-').unwrap_or((range, range));
                free.push((a.trim().parse().unwrap(), b.trim().parse().unwrap()));
            }
        }
        (free, block)
    }

    /// The tools' numbers against ours, exactly: never less than what the file system uses,
    /// and no more than that plus the partition's head and the space past the file system.
    /// For ext, block by block against dumpe2fs's free lists. Returns how much of the
    /// partition the file system's allocation map covers.
    fn check_numbers(kind: Kind, img: &Path, start: u64, size: u64) -> u64 {
        let fs_img;
        let fs_path = if start == 0 && size == file_size(img) {
            img
        } else {
            fs_img = img.with_extension(format!("part{start}"));
            fs::write(&fs_img, read_range(img, start, size)).unwrap();
            &fs_img
        };
        let ours = fs_usage(img, start, size).unwrap_or_else(|why| panic!("{kind:?} at {start} not understood: {why}"));
        let ours_bytes: u64 = ours.iter().map(|e| e.len).sum();
        let Some((used, covered)) = tool_numbers(kind, fs_path) else {
            eprintln!("{kind:?} at {start}: numbers not checked, unknown tool output");
            return size;
        };
        let tail = size - covered;
        eprintln!("{kind:?} at {start}: ours {ours_bytes}, tool says {used} used (+{tail} past the end)");
        assert!(ours_bytes >= used, "{kind:?}: {ours_bytes} bytes used, the tool counts {used}");
        assert!(ours_bytes <= used + HEAD_KEPT + tail, "{kind:?}: {ours_bytes} bytes used, the tool counts {used}");
        if kind == Kind::Ext {
            let (free, block) = ext_free_blocks(fs_path);
            let head = start + HEAD_KEPT;
            for &(a, b) in &free {
                let (from, to) = (start + a * block, start + (b + 1) * block);
                let from = from.max(head);
                if from < to {
                    assert_eq!(util::overlap(&ours, from, to - from), 0, "free blocks {a}-{b} counted as used");
                }
            }
            // Everything dumpe2fs doesn't list as free must be in ours.
            let mut free = free;
            free.sort_unstable();
            let mut next = 0;
            for (a, b) in free.into_iter().chain([(covered / block, covered / block)]) {
                if a > next {
                    let (from, len) = (start + next * block, (a - next) * block);
                    assert_eq!(util::overlap(&ours, from, len), len, "used blocks {next}-{} missing", a - 1);
                }
                next = next.max(b + 1);
            }
        }
        if fs_path != img {
            let _ = fs::remove_file(fs_path);
        }
        covered
    }

    /// An ntfs-3g mount as the current user (FUSE), unmounted on drop.
    struct Fuse {
        dir: PathBuf,
        child: Option<Child>,
    }

    impl Fuse {
        fn mount(img: &Path, dir: &Path, read_only: bool) -> Option<Fuse> {
            fs::create_dir_all(dir).unwrap();
            let opts = if read_only { "ro,no_def_opts,no_detach" } else { "no_def_opts,no_detach" };
            let child = Command::new("ntfs-3g")
                .args(["-o", opts, p(img), p(dir)])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut fuse = Fuse { dir: dir.to_owned(), child: Some(child) };
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(10) {
                let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
                if mounts.lines().any(|l| l.split(' ').nth(4) == Some(p(dir))) {
                    return Some(fuse);
                }
                if let Some(Ok(Some(_))) = fuse.child.as_mut().map(|c| c.try_wait()) {
                    fuse.child = None;
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Fuse {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = Command::new("fusermount3").args(["-u", p(&self.dir)]).status();
                // Exiting means everything is written back.
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(20) {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn can_fuse() -> bool {
        let ok = Path::new("/dev/fuse").exists() && have(&["ntfs-3g", "fusermount3"]);
        if !ok {
            eprintln!("skipped: no FUSE");
        }
        ok
    }

    /// Runs a command as root. Without a terminal (or cached credentials), the password
    /// comes from `DD_GUI_TEST_SUDO_PASSWORD`.
    fn sudo(args: &[&str]) -> (bool, String) {
        let password = std::env::var("DD_GUI_TEST_SUDO_PASSWORD").ok();
        let mut cmd = Command::new("sudo");
        if password.is_some() {
            cmd.args(["-S", "-p", ""]);
        }
        let mut child = cmd
            .args(args)
            .stdin(if password.is_some() { Stdio::piped() } else { Stdio::inherit() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(pw) = password {
            let _ = child.stdin.take().unwrap().write_all(format!("{pw}\n").as_bytes());
        }
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        )
    }

    /// An exFAT image loop-mounted by root (kernel driver), for the current user.
    struct LoopMount {
        dev: String,
        dir: PathBuf,
        mounted: bool,
    }

    impl LoopMount {
        fn mount(img: &Path, dir: &Path, read_only: bool) -> Option<LoopMount> {
            fs::create_dir_all(dir).unwrap();
            let (ok, out) = sudo(&["losetup", "-f", "--show", p(img)]);
            let dev = out.trim().to_owned();
            if !ok || !dev.starts_with("/dev/loop") {
                eprintln!("skipped: losetup failed: {out}");
                return None;
            }
            let mut m = LoopMount { dev, dir: dir.to_owned(), mounted: false };
            // Leave /dev/loop0 alone (a GUI test drive on the dev machine); ours is detached on drop.
            if m.dev == "/dev/loop0" {
                eprintln!("skipped: got /dev/loop0");
                return None;
            }
            let ids = format!("uid={},gid={}", sh("id", &["-u"]).trim(), sh("id", &["-g"]).trim());
            let opts = if read_only { format!("ro,{ids}") } else { ids };
            let (ok, out) = sudo(&["mount", "-t", "exfat", "-o", &opts, &m.dev, p(dir)]);
            m.mounted = ok;
            if !ok {
                eprintln!("skipped: mount failed: {out}");
                return None;
            }
            Some(m)
        }
    }

    impl Drop for LoopMount {
        fn drop(&mut self) {
            // Never panics: a failed unmount must not keep the device from being detached.
            for _ in 0..10 {
                if !self.mounted {
                    break;
                }
                let (ok, out) = sudo(&["umount", p(&self.dir)]);
                self.mounted = !ok;
                if !ok {
                    eprintln!("umount {}: {out}", self.dir.display());
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            // Only the device this test attached.
            let (ok, out) = sudo(&["losetup", "-d", &self.dev]);
            if !ok {
                eprintln!("losetup -d {}: {out}", self.dev);
            }
        }
    }

    fn can_sudo() -> bool {
        have(&["sudo", "losetup", "mount", "umount"])
    }

    /// fsck (read-only), then every file compared. Err says what's wrong.
    fn verify(kind: Kind, img: &Path, files: &Tree, s: &Scratch, tag: &str) -> Result<(), String> {
        let out = s.join(&format!("{tag}-files"));
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out).unwrap();
        let checked = match kind {
            Kind::Fat => {
                let (ok, text) = try_sh("fsck.fat", &["-n", p(img)]);
                if !ok || text.contains("differ") {
                    return Err(format!("fsck.fat: {text}"));
                }
                let (ok, text) = try_sh("mcopy", &["-s", "-n", "-i", p(img), "::/", p(&out)]);
                if !ok && !files.is_empty() {
                    return Err(format!("mcopy: {text}"));
                }
                compare(files, &out)
            }
            Kind::Ext => {
                let (ok, text) = try_sh("e2fsck", &["-fn", p(img)]);
                if !ok {
                    return Err(format!("e2fsck: {text}"));
                }
                let (_, text) = try_sh("debugfs", &["-R", &format!("rdump / {}", p(&out)), p(img)]);
                compare(files, &out).map_err(|e| format!("{e} ({text})"))
            }
            Kind::Ntfs => {
                let (ok, text) = try_sh("ntfsfix", &["-n", p(img)]);
                if !ok {
                    return Err(format!("ntfsfix: {text}"));
                }
                let (ok, text) = try_sh("ntfsinfo", &["-m", p(img)]);
                if !ok {
                    return Err(format!("ntfsinfo: {text}"));
                }
                let Some(_mount) = Fuse::mount(img, &out, true) else { return Err("ntfs-3g can't mount it".into()) };
                compare(files, &out)
            }
            Kind::Exfat => {
                let (ok, text) = try_sh("fsck.exfat", &["-n", p(img)]);
                if !ok {
                    return Err(format!("fsck.exfat: {text}"));
                }
                if files.is_empty() {
                    Ok(())
                } else {
                    let Some(_mount) = LoopMount::mount(img, &out, true) else { return Err("can't mount it".into()) };
                    compare(files, &out)
                }
            }
        };
        let _ = fs::remove_dir_all(&out);
        checked
    }

    /// The full treatment for one file system at `start..start + size` of `img`: numbers,
    /// then a smart copy that must pass fsck and hold every file, then proof that the check
    /// notices a missing extent.
    fn check_fs(f: &Fixture, s: &Scratch, disk: &Path, start: u64, size: u64, layout: &Layout) {
        let covered = check_numbers(f.kind, disk, start, size);
        let copy = s.join("copy.img");
        // The file system on its own, as the tools want it (the copy itself for a whole drive).
        let whole = start == 0 && size == file_size(disk);
        let part = if whole { copy.clone() } else { s.join("copy-part.img") };
        let cut = |copy: &Path| {
            if !whole {
                fs::write(&part, read_range(copy, start, size)).unwrap();
            }
        };
        smart_copy(disk, &layout.extents, &copy);
        cut(&copy);
        verify(f.kind, &part, &f.files, s, "smart").unwrap_or_else(|e| panic!("{:?} smart copy broken: {e}", f.kind));
        // Negative control: without the biggest extent inside this file system, the checks
        // must fail (they'd be worthless otherwise).
        let inside = layout.extents.iter().filter(|e| e.start >= start && e.start + e.len <= start + covered);
        if let Some(big) = inside.max_by_key(|e| e.len) {
            let fewer: Vec<Extent> = layout.extents.iter().filter(|e| *e != big).copied().collect();
            smart_copy(disk, &fewer, &copy);
            cut(&copy);
            let broken = verify(f.kind, &part, &f.files, s, "broken");
            assert!(broken.is_err(), "{:?}: dropping {big:?} went unnoticed", f.kind);
        }
        let _ = fs::remove_file(&copy);
        let _ = fs::remove_file(&part);
    }

    /// A whole-drive image of one file system.
    fn check_floppy(f: &Fixture, s: &Scratch) {
        let layout = layout_of(&f.img);
        assert_eq!(layout.table, Table::None);
        let part = &layout.partitions[0];
        assert!(part.understood, "{part:?}");
        eprintln!(
            "{:?}: {} of {} bytes in {} extents, label {:?}",
            f.kind,
            layout.used(),
            layout.size,
            layout.extents.len(),
            part.label
        );
        check_fs(f, s, &f.img, 0, layout.size, &layout);
    }

    /// Files to keep, plus files written and then deleted, so free space holds stale data.
    fn trees(rng: &mut Rng, count: usize, max: u64) -> (Tree, Tree) {
        (make_tree(rng, "keep", count, max), make_tree(rng, "gone", count / 3, max))
    }

    fn fat_image(s: &Scratch, name: &str, size: u64, args: &[&str], count: usize, max: u64) -> Fixture {
        let mut rng = Rng(size ^ count as u64);
        let img = s.join(name);
        blank(&img, size);
        let mut mkfs = vec!["-n", "SMARTFAT"];
        mkfs.extend(args);
        mkfs.push(p(&img));
        sh("mkfs.fat", &mkfs);
        let (keep, gone) = trees(&mut rng, count, max);
        let src = s.join(&format!("{name}-src"));
        write_tree(&keep, &src);
        write_tree(&gone, &src);
        let mut args = vec!["-s".to_owned(), "-i".into(), p(&img).into()];
        args.extend(fs::read_dir(&src).unwrap().map(|e| p(&e.unwrap().path()).to_owned()));
        args.push("::/".into());
        sh("mcopy", &args.iter().map(String::as_str).collect::<Vec<_>>());
        let mut del = vec!["-i".to_owned(), p(&img).into()];
        del.extend(gone.keys().map(|k| format!("::/{k}")));
        sh("mdel", &del.iter().map(String::as_str).collect::<Vec<_>>());
        fs::remove_dir_all(&src).unwrap();
        Fixture { img, kind: Kind::Fat, files: keep }
    }

    fn ext_image(s: &Scratch, name: &str, size: u64, args: &[&str], count: usize, max: u64) -> Fixture {
        let mut rng = Rng(size ^ count as u64 ^ args.len() as u64);
        let img = s.join(name);
        let (keep, gone) = trees(&mut rng, count, max);
        let src = s.join(&format!("{name}-src"));
        fs::create_dir_all(&src).unwrap();
        write_tree(&keep, &src);
        write_tree(&gone, &src);
        blank(&img, size);
        let mut mkfs = vec!["-q", "-F", "-L", "smart-ext", "-d", p(&src)];
        mkfs.extend(args);
        mkfs.push(p(&img));
        sh("mke2fs", &mkfs);
        let script = s.join(&format!("{name}-rm"));
        fs::write(&script, gone.keys().map(|k| format!("rm /{k}\n")).collect::<String>()).unwrap();
        sh("debugfs", &["-w", "-f", p(&script), p(&img)]);
        fs::remove_dir_all(&src).unwrap();
        Fixture { img, kind: Kind::Ext, files: keep }
    }

    /// NTFS filled through ntfs-3g (FUSE); None without FUSE.
    fn ntfs_image(s: &Scratch, name: &str, size: u64, args: &[&str], count: usize, max: u64) -> Option<Fixture> {
        let mut rng = Rng(size ^ 0x4E54 ^ args.len() as u64);
        let img = s.join(name);
        blank(&img, size);
        let mut mkfs = vec!["-F", "-Q", "-q", "-L", "SMARTNTFS"];
        mkfs.extend(args);
        mkfs.push(p(&img));
        sh("mkntfs", &mkfs);
        let (keep, gone) = trees(&mut rng, count, max);
        let dir = s.join(&format!("{name}-mnt"));
        {
            let _mount = Fuse::mount(&img, &dir, false)?;
            write_tree(&keep, &dir);
            write_tree(&gone, &dir);
            for k in gone.keys() {
                fs::remove_file(dir.join(k)).unwrap();
            }
        }
        Some(Fixture { img, kind: Kind::Ntfs, files: keep })
    }

    fn exfat_image(s: &Scratch, name: &str, size: u64, args: &[&str]) -> Fixture {
        let img = s.join(name);
        blank(&img, size);
        let mut mkfs = vec!["-L", "SMARTEX"];
        mkfs.extend(args);
        mkfs.push(p(&img));
        sh("mkfs.exfat", &mkfs);
        Fixture { img, kind: Kind::Exfat, files: Tree::new() }
    }

    #[test]
    fn fat12() {
        if !have(&["mkfs.fat", "fsck.fat", "mcopy", "mdel"]) {
            return;
        }
        let s = Scratch::new("fat12");
        let f = fat_image(&s, "fat12.img", 3 * MB, &["-F", "12"], 30, 200_000);
        check_floppy(&f, &s);
        assert_eq!(layout_of(&f.img).partitions[0].label.as_deref(), Some("SMARTFAT"));
    }

    #[test]
    fn fat16() {
        if !have(&["mkfs.fat", "fsck.fat", "mcopy", "mdel"]) {
            return;
        }
        let s = Scratch::new("fat16");
        let f = fat_image(&s, "fat16.img", 48 * MB, &["-F", "16"], 80, 2 * MB);
        check_floppy(&f, &s);
    }

    #[test]
    fn fat32() {
        if !have(&["mkfs.fat", "fsck.fat", "mcopy", "mdel"]) {
            return;
        }
        let s = Scratch::new("fat32");
        // 512-byte clusters: many clusters, a FAT read in several pieces.
        let f = fat_image(&s, "fat32.img", 100 * MB, &["-F", "32", "-s", "1"], 150, 3 * MB);
        check_floppy(&f, &s);
    }

    #[test]
    fn exfat_empty() {
        if !have(&["mkfs.exfat", "fsck.exfat", "dump.exfat"]) {
            return;
        }
        let s = Scratch::new("exfat");
        let f = exfat_image(&s, "exfat.img", 64 * MB, &[]);
        check_floppy(&f, &s);
        assert_eq!(layout_of(&f.img).partitions[0].label.as_deref(), Some("SMARTEX"));
        // Marked dirty: still understood, the directories are walked anyway.
        let mut flags = read_range(&f.img, 106, 2);
        flags[0] |= 2;
        put_bytes(&f.img, 106, &flags);
        assert!(layout_of(&f.img).partitions[0].understood);
    }

    #[test]
    #[ignore]
    fn exfat_with_files() {
        if !have(&["mkfs.exfat", "fsck.exfat", "dump.exfat"]) || !can_sudo() {
            return;
        }
        let s = Scratch::new("exfat-files");
        let mut rng = Rng(0xE8FA7);
        for (name, args, size) in [("exfat-4k.img", &[][..], 128 * MB), ("exfat-32k.img", &["-c", "32K"][..], 256 * MB)]
        {
            let mut f = exfat_image(&s, name, size, args);
            let (keep, gone) = trees(&mut rng, 300, 4 * MB);
            let dir = s.join(&format!("{name}-mnt"));
            {
                let Some(_mount) = LoopMount::mount(&f.img, &dir, false) else { return };
                write_tree(&keep, &dir);
                write_tree(&gone, &dir);
                for k in gone.keys() {
                    fs::remove_file(dir.join(k)).unwrap();
                }
                // A fragmented file: two files grown in turns.
                let (mut a, mut b) = (Vec::new(), Vec::new());
                for _ in 0..40 {
                    a.extend(rng.bytes(40_000));
                    b.extend(rng.bytes(50_000));
                    fs::write(dir.join("frag-a.bin"), &a).unwrap();
                    fs::write(dir.join("frag-b.bin"), &b).unwrap();
                }
                f.files = keep;
                f.files.insert("frag-a.bin".into(), a);
                f.files.insert("frag-b.bin".into(), b);
            }
            check_floppy(&f, &s);

            // A bitmap lagging behind the directories (a volume yanked mid-write): walking the
            // directories still finds every file. Here the bitmap calls all file data free (the
            // system files keep their bits, or Linux won't mount it).
            let out = sh("dump.exfat", &[p(&f.img)]);
            let number = |key| number_after(&out, key).unwrap();
            let heap = number("Cluster Heap Offset (sector offset):") * number("Bytes per Sector:");
            let bitmap = heap + (number("Bitmap start cluster:") - 2) * number("Cluster size:");
            let kept = 8;
            let len = number("Bitmap size:");
            let real = read_range(&f.img, bitmap, len);
            put_bytes(&f.img, bitmap + kept, &vec![0; (len - kept) as usize]);
            let (_, complaints) = try_sh("fsck.exfat", &["-n", p(&f.img)]);
            assert!(complaints.contains("marked as free"), "{complaints}");
            let layout = layout_of(&f.img);
            assert!(layout.partitions[0].understood);
            let copy = s.join("lagging.img");
            smart_copy(&f.img, &layout.extents, &copy);
            // Linux won't mount a volume whose bitmap disagrees with its files: put the real
            // bitmap back in the copy only. A file cluster the walk missed would be garbage.
            put_bytes(&copy, bitmap, &real);
            let dir = s.join("lagging-files");
            let Some(_mount) = LoopMount::mount(&copy, &dir, true) else { return };
            compare(&f.files, &dir).unwrap();
        }
    }

    #[test]
    fn ntfs() {
        if !have(&["mkntfs", "ntfsinfo", "ntfsfix"]) || !can_fuse() {
            return;
        }
        let s = Scratch::new("ntfs");
        let Some(f) = ntfs_image(&s, "ntfs.img", 96 * MB, &[], 120, 3 * MB) else { return };
        check_floppy(&f, &s);
        assert_eq!(layout_of(&f.img).partitions[0].label.as_deref(), Some("SMARTNTFS"));
    }

    #[test]
    fn ntfs_variants() {
        if !have(&["mkntfs", "ntfsinfo", "ntfsfix"]) || !can_fuse() {
            return;
        }
        let s = Scratch::new("ntfs-variants");
        // Tiny and huge clusters, and 4096-byte sectors (so 4 KiB MFT records).
        for (name, args) in [("c512", &["-c", "512"][..]), ("c64k", &["-c", "65536"][..]), ("4kn", &["-s", "4096"][..])]
        {
            eprintln!("--- {name}");
            if !supports(&s, "mkntfs", &[&["-F", "-Q", "-q"][..], args].concat(), 64 * MB) {
                continue;
            }
            let Some(f) = ntfs_image(&s, &format!("{name}.img"), 64 * MB, args, 80, 2 * MB) else { return };
            check_floppy(&f, &s);
            fs::remove_file(&f.img).unwrap();
        }
    }

    #[test]
    fn fat_variants() {
        if !have(&["mkfs.fat", "fsck.fat", "mcopy", "mdel"]) {
            return;
        }
        let s = Scratch::new("fat-variants");
        let variants: &[(&str, &[&str])] = &[
            ("fat12-4kn", &["-F", "12", "-S", "4096"]),
            ("fat16-2k", &["-F", "16", "-S", "2048", "-s", "2"]),
            ("fat32-4kn", &["-F", "32", "-S", "4096", "-s", "1"]),
            ("fat16-32k-clusters", &["-F", "16", "-s", "64"]),
            ("fat32-one-fat", &["-F", "32", "-f", "1"]),
        ];
        for (name, args) in variants {
            eprintln!("--- {name}");
            let (size, count, max) =
                if name.starts_with("fat12") { (8 * MB, 20, 200_000) } else { (200 * MB, 60, 2 * MB) };
            if !supports(&s, "mkfs.fat", args, size) {
                continue;
            }
            let f = fat_image(&s, &format!("{name}.img"), size, args, count, max);
            check_floppy(&f, &s);
            fs::remove_file(&f.img).unwrap();
        }
    }

    #[test]
    fn exfat_variants() {
        if !have(&["mkfs.exfat", "fsck.exfat", "dump.exfat"]) {
            return;
        }
        let s = Scratch::new("exfat-variants");
        let variants: &[(&str, &[&str])] =
            &[("4kn", &["-s", "4096"]), ("1m-clusters", &["-c", "1M"]), ("packed-bitmap", &["--pack-bitmap"])];
        for (name, args) in variants {
            eprintln!("--- {name}");
            if !supports(&s, "mkfs.exfat", args, 128 * MB) {
                continue;
            }
            let f = exfat_image(&s, &format!("{name}.img"), 128 * MB, args);
            check_floppy(&f, &s);
            fs::remove_file(&f.img).unwrap();
        }
    }

    /// The first run of $LogFile, per ntfsinfo, in bytes.
    fn ntfs_log_at(img: &Path) -> u64 {
        let out = sh("ntfsinfo", &["-i", "2", "-v", p(img)]);
        let line = out.lines().skip_while(|l| !l.contains("Runlist:")).nth(1).unwrap();
        let lcn = line.split_whitespace().nth(1).unwrap().trim_start_matches("0x");
        u64::from_str_radix(lcn, 16).unwrap() * number_after(&sh("ntfsinfo", &["-m", p(img)]), "Cluster Size:").unwrap()
    }

    #[test]
    fn ntfs_with_a_journal_to_replay() {
        if !have(&["mkntfs", "ntfsinfo"]) {
            return;
        }
        let s = Scratch::new("ntfs-log");
        let img = s.join("ntfs.img");
        blank(&img, 32 * MB);
        sh("mkntfs", &["-F", "-Q", "-q", p(&img)]);
        assert!(layout_of(&img).partitions[0].understood);
        // A restart page with a client still using the log, and no "clean" flag.
        let mut page = vec![0u8; 4096];
        page[..4].copy_from_slice(b"RSTR");
        page[4..6].copy_from_slice(&0x1Eu16.to_le_bytes());
        page[6..8].copy_from_slice(&9u16.to_le_bytes());
        page[0x10..0x14].copy_from_slice(&4096u32.to_le_bytes());
        page[0x14..0x18].copy_from_slice(&4096u32.to_le_bytes());
        page[0x18..0x1A].copy_from_slice(&0x30u16.to_le_bytes());
        page[0x1C..0x1E].copy_from_slice(&1u16.to_le_bytes());
        let area = 0x30;
        page[area..area + 8].copy_from_slice(&0x1234u64.to_le_bytes());
        page[area + 8..area + 10].copy_from_slice(&1u16.to_le_bytes());
        page[area + 0x0A..area + 0x0C].copy_from_slice(&0xFFFFu16.to_le_bytes());
        page[area + 0x0C..area + 0x0E].copy_from_slice(&0u16.to_le_bytes());
        // Update sequence: number 1 at the end of every 512-byte stride, originals (zeros) saved.
        page[0x1E..0x20].copy_from_slice(&1u16.to_le_bytes());
        for i in 1..=8 {
            page[i * 512 - 2..i * 512].copy_from_slice(&1u16.to_le_bytes());
        }
        let log = ntfs_log_at(&img);
        put_bytes(&img, log, &page);
        let layout = layout_of(&img);
        assert!(!layout.partitions[0].understood);
        assert_eq!(layout.used(), 32 * MB);
        assert!(estimate_of(&img).partitions[0].understood);
        // Marked clean: fine again.
        page[area + 0x0E..area + 0x10].copy_from_slice(&2u16.to_le_bytes());
        put_bytes(&img, log, &page);
        assert!(layout_of(&img).partitions[0].understood);
        // A torn write (update sequence mismatch) makes the restart page invalid: not clean.
        page[1022] = 7;
        put_bytes(&img, log, &page);
        assert!(!layout_of(&img).partitions[0].understood);
    }

    #[test]
    fn ext2() {
        if !have(&["mke2fs", "e2fsck", "debugfs", "dumpe2fs"]) {
            return;
        }
        let s = Scratch::new("ext2");
        let f = ext_image(&s, "ext2.img", 64 * MB, &["-t", "ext2", "-b", "4096"], 100, 2 * MB);
        check_floppy(&f, &s);
        assert_eq!(layout_of(&f.img).partitions[0].fs.as_deref(), Some("ext2"));
    }

    #[test]
    fn ext3() {
        if !have(&["mke2fs", "e2fsck", "debugfs", "dumpe2fs"]) {
            return;
        }
        let s = Scratch::new("ext3");
        let f = ext_image(&s, "ext3.img", 64 * MB, &["-t", "ext3"], 100, 2 * MB);
        check_floppy(&f, &s);
        assert_eq!(layout_of(&f.img).partitions[0].fs.as_deref(), Some("ext3"));
    }

    #[test]
    fn ext4_many_groups() {
        if !have(&["mke2fs", "e2fsck", "debugfs", "dumpe2fs"]) {
            return;
        }
        let s = Scratch::new("ext4");
        // 4 KiB blocks, 8 MiB groups: 32 groups in two flex groups, most never initialized.
        let f = ext_image(&s, "ext4.img", 256 * MB, &["-t", "ext4", "-b", "4096", "-g", "2048"], 700, MB);
        let out = sh("dumpe2fs", &[p(&f.img)]);
        assert!(out.contains("BLOCK_UNINIT") && out.contains("flex_bg") && out.contains("64bit"), "{out}");
        check_floppy(&f, &s);
        let layout = layout_of(&f.img);
        assert_eq!(layout.partitions[0].fs.as_deref(), Some("ext4"));
        assert_eq!(layout.partitions[0].label.as_deref(), Some("smart-ext"));
    }

    #[test]
    fn ext4_small_blocks() {
        if !have(&["mke2fs", "e2fsck", "debugfs", "dumpe2fs"]) {
            return;
        }
        let s = Scratch::new("ext4-1k");
        let f = ext_image(&s, "ext4-1k.img", 64 * MB, &["-t", "ext4", "-b", "1024"], 150, MB);
        check_floppy(&f, &s);
    }

    #[test]
    fn ext4_variants() {
        if !have(&["mke2fs", "e2fsck", "debugfs", "dumpe2fs"]) {
            return;
        }
        let s = Scratch::new("ext4-variants");
        let variants: &[(&str, &[&str])] = &[
            // Group descriptors spread over meta groups.
            ("meta_bg", &["-t", "ext4", "-b", "1024", "-g", "1024", "-O", "meta_bg,^resize_inode"]),
            // Clusters of 16 blocks.
            ("bigalloc", &["-t", "ext4", "-O", "bigalloc", "-C", "65536"]),
            // CRC-16 group descriptor checksums instead of metadata_csum.
            ("uninit_bg", &["-t", "ext4", "-b", "4096", "-g", "4096", "-O", "^metadata_csum,uninit_bg"]),
            // Only two backup superblocks.
            ("sparse_super2", &["-t", "ext4", "-b", "4096", "-g", "4096", "-O", "sparse_super2"]),
            // Every group has a backup.
            ("no_sparse_super", &["-t", "ext4", "-b", "1024", "-O", "^sparse_super,^resize_inode"]),
            // No flex_bg: each group's metadata at its start.
            ("no_flex_bg", &["-t", "ext4", "-b", "4096", "-g", "4096", "-O", "^flex_bg"]),
        ];
        for (i, (name, args)) in variants.iter().enumerate() {
            eprintln!("--- {name}");
            if !supports(&s, "mke2fs", &[&["-q", "-F"][..], args].concat(), 128 * MB) {
                continue;
            }
            let size = if *name == "bigalloc" { 256 * MB } else { 128 * MB };
            let f = ext_image(&s, &format!("{name}.img"), size, args, 200 + i, 2 * MB);
            check_floppy(&f, &s);
            fs::remove_file(&f.img).unwrap();
        }
    }

    #[test]
    fn ext4_not_cleanly_unmounted() {
        if !have(&["mke2fs", "debugfs"]) {
            return;
        }
        let s = Scratch::new("ext4-dirty");
        let img = s.join("dirty.img");
        blank(&img, 64 * MB);
        sh("mke2fs", &["-q", "-F", "-t", "ext4", p(&img)]);
        assert!(layout_of(&img).partitions[0].understood);
        sh("debugfs", &["-w", "-R", "feature needs_recovery", p(&img)]);
        let layout = layout_of(&img);
        assert!(!layout.partitions[0].understood);
        assert_eq!(layout.used(), 64 * MB);
        assert!(estimate_of(&img).partitions[0].understood);
        sh("debugfs", &["-w", "-R", "feature ^needs_recovery", p(&img)]);
        sh("debugfs", &["-w", "-R", "ssv state 0", p(&img)]);
        assert!(!layout_of(&img).partitions[0].understood);
        assert!(estimate_of(&img).partitions[0].understood);
    }

    #[test]
    fn corrupt_ext_metadata_falls_back() {
        if !have(&["mke2fs"]) {
            return;
        }
        let s = Scratch::new("ext4-corrupt");
        let img = s.join("ext4.img");
        blank(&img, 64 * MB);
        sh("mke2fs", &["-q", "-F", "-t", "ext4", "-b", "4096", p(&img)]);
        let good = fs::read(&img).unwrap();
        // The superblock, a group descriptor, the first block bitmap: each checksummed.
        let bitmap = u64::from(u32::from_le_bytes(good[4096..4100].try_into().unwrap())) * 4096;
        for at in [1024 + 0x20, 4096 + 8, bitmap as usize + 100] {
            let mut bad = good.clone();
            bad[at] ^= 0x10;
            let layout = analyze_bytes(&bad, bad.len() as u64).unwrap();
            check_invariants(&layout);
            assert!(!layout.partitions[0].understood, "corruption at {at} unnoticed");
        }
    }

    #[test]
    fn signatures_by_name_only() {
        let size = 8 * MB;
        let mut rng = Rng(99);
        let cases: &[(&str, usize, &[u8])] = &[
            ("crypto_LUKS", 0, b"LUKS\xba\xbe"),
            ("xfs", 0, b"XFSB"),
            ("btrfs", 0x10040, b"_BHRfS_M"),
            ("apfs", 32, b"NXSB"),
            ("swap", 4086, b"SWAPSPACE2"),
            ("iso9660", 0x8001, b"CD001"),
            ("squashfs", 0, b"hsqs"),
            ("BitLocker", 3, b"-FVE-FS-"),
        ];
        for &(name, at, magic) in cases {
            let mut data = rng.bytes(size as usize);
            data[..512].fill(0);
            data[at..at + magic.len()].copy_from_slice(magic);
            let layout = analyze_bytes(&data, size).unwrap();
            check_invariants(&layout);
            assert_eq!(layout.table, Table::None);
            assert_eq!(layout.partitions[0].fs.as_deref(), Some(name));
            assert!(!layout.partitions[0].understood);
            assert_eq!(layout.used(), size);
        }
    }

    /// A drive with partitions made from separate images: (partition number, kind of content).
    #[derive(Clone, Copy, Debug)]
    enum Content {
        Fs(Kind),
        Garbage,
        Swap,
    }

    struct Built {
        disk: PathBuf,
        fixtures: Vec<(u32, Option<Fixture>, u64, u64)>,
        layout: Layout,
    }

    /// Builds a disk image: an sfdisk table (`label`, then `(first sector, sectors, type)` lines,
    /// extended partitions included), and the contents of the partitions `parts` (number,
    /// first sector, sectors, content), made separately and written in.
    fn build_disk(
        s: &Scratch,
        size: u64,
        ss: u64,
        label: &str,
        table: &[(u64, u64, &str)],
        parts: &[(u32, u64, u64, Content)],
    ) -> Option<Built> {
        let disk = s.join("disk.img");
        blank(&disk, size);
        let mut script = format!("label: {label}\n");
        for (first, sectors, kind) in table {
            script += &format!("start={first}, size={sectors}, type={kind}\n");
        }
        let mut child = Command::new("sfdisk")
            .args(["-q", "--sector-size", &ss.to_string(), p(&disk)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "sfdisk: {}", String::from_utf8_lossy(&out.stderr));
        let mut rng = Rng(size ^ ss);
        let mut fixtures = Vec::new();
        for &(index, first, sectors, content) in parts {
            let (start, len) = (first * ss, sectors * ss);
            let name = format!("p{index}.img");
            let fixture = match content {
                Content::Fs(Kind::Fat) => {
                    // FAT32 needs more clusters than 4 KiB sectors give a small partition.
                    let args: &[&str] = if ss == 4096 { &["-F", "16", "-S", "4096", "-s", "1"] } else { &["-F", "32"] };
                    Some(fat_image(s, &name, len, args, 60, 2 * MB))
                }
                Content::Fs(Kind::Ext) => Some(ext_image(s, &name, len, &["-t", "ext4"], 80, 2 * MB)),
                Content::Fs(Kind::Ntfs) => Some(ntfs_image(s, &name, len, &[], 50, 2 * MB)?),
                Content::Fs(Kind::Exfat) => Some(exfat_image(s, &name, len, &[])),
                Content::Garbage => {
                    fs::write(s.join(&name), rng.bytes(len as usize)).unwrap();
                    None
                }
                Content::Swap => {
                    blank(&s.join(&name), len);
                    sh("mkswap", &[p(&s.join(&name))]);
                    None
                }
            };
            put(&disk, start, &s.join(&name));
            fs::remove_file(s.join(&name)).unwrap();
            fixtures.push((index, fixture.map(|f| Fixture { img: disk.clone(), ..f }), start, len));
        }
        let layout = layout_of(&disk);
        Some(Built { disk, fixtures, layout })
    }

    /// Checks every partition of a built disk, and that bytes outside all partitions and
    /// tables weren't copied.
    fn check_disk(b: &Built, s: &Scratch, table: Table, gaps: &[(u64, u64)]) {
        let l = &b.layout;
        eprintln!("{:?}: {} of {} bytes in {} extents", l.table, l.used(), l.size, l.extents.len());
        for part in &l.partitions {
            eprintln!("  {part:?}");
        }
        assert_eq!(l.table, table);
        assert_eq!(l.partitions.len(), b.fixtures.len());
        for (part, (index, fixture, start, len)) in l.partitions.iter().zip(&b.fixtures) {
            assert_eq!((part.index, part.start, part.size), (*index, *start, *len));
            assert_eq!(part.understood, fixture.is_some(), "{part:?}");
            if let Some(f) = fixture {
                check_fs(f, s, &b.disk, *start, *len, l);
            } else {
                assert_eq!(part.used, part.size);
            }
        }
        // Unallocated space holds random bytes here: it must not be copied.
        for &(start, len) in gaps {
            let inner = (start + GAP + ALIGN, len.saturating_sub(2 * (GAP + ALIGN)));
            assert_eq!(util::overlap(&l.extents, inner.0, inner.1), 0, "unallocated space at {start} copied");
        }
        // The first and last MiB always are.
        assert_eq!(util::overlap(&l.extents, 0, MB), MB);
        assert_eq!(util::overlap(&l.extents, l.size - MB, MB), MB);
    }

    #[test]
    fn gpt_mixed() {
        let tools = ["sfdisk", "mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools)
            || !have(&["mkntfs", "ntfsinfo", "ntfsfix", "mkfs.exfat", "fsck.exfat", "dump.exfat", "mkswap"])
        {
            return;
        }
        if !can_fuse() {
            return;
        }
        let s = Scratch::new("gpt");
        let mib = 2048;
        const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
        // In MiB: FAT32 1-41, ext4 41-89, NTFS 89-129, garbage 129-137, exFAT 137-153, a gap
        // of random bytes 153-165, swap 165-173, random bytes up to the backup GPT (191-192).
        let parts = [
            (1, mib, 40 * mib, Content::Fs(Kind::Fat)),
            (2, 41 * mib, 48 * mib, Content::Fs(Kind::Ext)),
            (3, 89 * mib, 40 * mib, Content::Fs(Kind::Ntfs)),
            (4, 129 * mib, 8 * mib, Content::Garbage),
            (5, 137 * mib, 16 * mib, Content::Fs(Kind::Exfat)),
            (6, 165 * mib, 8 * mib, Content::Swap),
        ];
        let types = ["U", "L", BASIC_DATA, "L", BASIC_DATA, "S"];
        let table: Vec<_> = parts.iter().zip(types).map(|(&(_, first, n, _), t)| (first, n, t)).collect();
        let Some(b) = build_disk(&s, 192 * MB, 512, "gpt", &table, &parts) else { return };
        let mut rng = Rng(5);
        let gap = (153 * MB, 12 * MB);
        put_bytes(&b.disk, gap.0, &rng.bytes(gap.1 as usize));
        let tail = (173 * MB, 18 * MB);
        put_bytes(&b.disk, tail.0, &rng.bytes(tail.1 as usize));
        let b = Built { layout: layout_of(&b.disk), ..b };
        let names: Vec<_> = b.layout.partitions.iter().map(|p| p.fs.as_deref()).collect();
        assert_eq!(names, [Some("vfat"), Some("ext4"), Some("ntfs"), None, Some("exfat"), Some("swap")]);
        check_disk(&b, &s, Table::Gpt, &[gap, tail]);

        // Wreck the primary GPT header: the backup takes over.
        let good = b.layout.clone();
        let mut header = read_range(&b.disk, 512, 512);
        header[100] ^= 0xFF;
        header[24] ^= 1;
        put_bytes(&b.disk, 512, &header);
        let backup = layout_of(&b.disk);
        assert_eq!(backup.table, Table::Gpt);
        assert_eq!(backup.extents, good.extents);
        // And the backup too: nothing's left to trust, the protective partition is copied whole.
        let size = file_size(&b.disk);
        let mut last = read_range(&b.disk, size - 512, 512);
        last[30] ^= 0xFF;
        put_bytes(&b.disk, size - 512, &last);
        let none = layout_of(&b.disk);
        assert_eq!(none.table, Table::Mbr);
        assert_eq!(none.used(), size);
    }

    #[test]
    fn mbr_with_logical_partitions() {
        let tools = ["sfdisk", "mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools) || !have(&["mkfs.exfat", "fsck.exfat", "dump.exfat"]) {
            return;
        }
        let s = Scratch::new("mbr");
        let mib = 2048;
        // In MiB: FAT32 1-41; extended 41-144 holding ext4 42-90, garbage 91-99, ext4 100-132
        // and random bytes 132-144 outside any logical partition; exFAT 144-160.
        let table = [
            (mib, 40 * mib, "c"),
            (41 * mib, 103 * mib, "5"),
            (144 * mib, 16 * mib, "7"),
            (42 * mib, 48 * mib, "83"),
            (91 * mib, 8 * mib, "83"),
            (100 * mib, 32 * mib, "83"),
        ];
        let parts = [
            (1, mib, 40 * mib, Content::Fs(Kind::Fat)),
            (3, 144 * mib, 16 * mib, Content::Fs(Kind::Exfat)),
            (5, 42 * mib, 48 * mib, Content::Fs(Kind::Ext)),
            (6, 91 * mib, 8 * mib, Content::Garbage),
            (7, 100 * mib, 32 * mib, Content::Fs(Kind::Ext)),
        ];
        let Some(b) = build_disk(&s, 168 * MB, 512, "dos", &table, &parts) else { return };
        let mut rng = Rng(6);
        let hole = (132 * MB, 12 * MB);
        put_bytes(&b.disk, hole.0, &rng.bytes(hole.1 as usize));
        let b = Built { layout: layout_of(&b.disk), ..b };
        let order: Vec<u32> = b.layout.partitions.iter().map(|p| p.index).collect();
        assert_eq!(order, [1, 3, 5, 6, 7]);
        check_disk(&b, &s, Table::Mbr, &[hole]);
        // Every EBR is kept: follow the chain here too.
        let mut ebr = 41 * mib;
        for _ in 0..3 {
            assert_eq!(util::overlap(&b.layout.extents, ebr * 512, 512), 512, "EBR at sector {ebr} not copied");
            let sector = read_range(&b.disk, ebr * 512, 512);
            let next = u64::from(u32::from_le_bytes(sector[0x1CE + 8..0x1CE + 12].try_into().unwrap()));
            if next == 0 {
                break;
            }
            ebr = 41 * mib + next;
        }
    }

    /// Old-style partitions at odd sectors, and file systems smaller than their partitions:
    /// what lies past a file system's end is copied as is.
    #[test]
    fn unaligned_partitions_and_short_file_systems() {
        let tools = ["sfdisk", "mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools) {
            return;
        }
        let s = Scratch::new("unaligned");
        let disk = s.join("disk.img");
        blank(&disk, 160 * MB);
        let mut child = Command::new("sfdisk").args(["-q", p(&disk)]).stdin(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"label: dos\nstart=63, size=131009, type=c\nstart=131135, size=160001, type=83\n")
            .unwrap();
        assert!(child.wait().unwrap().success());
        let mut rng = Rng(63);
        // FAT32 on all of partition 1; ext4 on 60 MiB of partition 2's 78 MiB, with junk after it.
        let fat = fat_image(&s, "p1.img", 131_009 * 512, &["-F", "32"], 60, 2 * MB);
        put(&disk, 63 * 512, &fat.img);
        let ext = ext_image(&s, "p2.img", 60 * MB, &["-t", "ext4", "-b", "4096"], 60, 2 * MB);
        let (start2, size2) = (131_135 * 512, 160_001 * 512);
        put(&disk, start2, &ext.img);
        let junk = rng.bytes((size2 - 60 * MB) as usize);
        put_bytes(&disk, start2 + 60 * MB, &junk);
        let layout = layout_of(&disk);
        assert!(layout.partitions.iter().all(|p| p.understood), "{:?}", layout.partitions);
        assert_eq!(util::overlap(&layout.extents, start2 + 60 * MB, size2 - 60 * MB), size2 - 60 * MB);
        check_fs(&Fixture { img: disk.clone(), ..fat }, &s, &disk, 63 * 512, 131_009 * 512, &layout);
        check_fs(&Fixture { img: disk.clone(), ..ext }, &s, &disk, start2, size2, &layout);
        // The junk past ext4's end made it into the copy.
        let copy = s.join("tail.img");
        smart_copy(&disk, &layout.extents, &copy);
        assert_eq!(read_range(&copy, start2 + 60 * MB, size2 - 60 * MB), junk);
    }

    #[test]
    fn gpt_with_4k_sectors() {
        let tools = ["sfdisk", "mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools) {
            return;
        }
        let s = Scratch::new("gpt4k");
        // 4096-byte sectors: 256 to the MiB.
        let mib = 256;
        let table = [(mib, 48 * mib, "U"), (49 * mib, 48 * mib, "L")];
        let parts = [(1, mib, 48 * mib, Content::Fs(Kind::Fat)), (2, 49 * mib, 48 * mib, Content::Fs(Kind::Ext))];
        let Some(b) = build_disk(&s, 100 * MB, 4096, "gpt", &table, &parts) else { return };
        check_disk(&b, &s, Table::Gpt, &[]);
    }

    #[test]
    fn mbr_with_4k_sectors() {
        let tools = ["sfdisk", "mkfs.fat", "fsck.fat", "mcopy", "mdel", "mke2fs", "e2fsck", "debugfs", "dumpe2fs"];
        if !have(&tools) {
            return;
        }
        let s = Scratch::new("mbr4k");
        // 4096-byte sectors: 256 to the MiB.
        let mib = 256;
        let table = [(mib, 48 * mib, "c"), (49 * mib, 48 * mib, "83")];
        let parts = [(1, mib, 48 * mib, Content::Fs(Kind::Fat)), (2, 49 * mib, 48 * mib, Content::Fs(Kind::Ext))];
        let Some(b) = build_disk(&s, 100 * MB, 4096, "dos", &table, &parts) else { return };
        check_disk(&b, &s, Table::Mbr, &[]);
    }

    #[test]
    fn md_raid_members_are_copied_whole() {
        if !have(&["mke2fs"]) {
            return;
        }
        let s = Scratch::new("md");
        let img = s.join("member.img");
        blank(&img, 64 * MB);
        sh("mke2fs", &["-q", "-F", "-t", "ext4", "-b", "4096", p(&img), "60M"]);
        assert!(layout_of(&img).partitions[0].understood);
        // md metadata 1.0 sits 8 KiB before the end.
        let at = ((64 * MB / 512 - 16) & !7) * 512;
        let mut sb = 0xA92B_4EFCu32.to_le_bytes().to_vec();
        sb.extend(1u32.to_le_bytes());
        put_bytes(&img, at, &sb);
        let layout = layout_of(&img);
        assert_eq!(layout.partitions[0].fs.as_deref(), Some("linux_raid_member"));
        assert!(!layout.partitions[0].understood);
    }

    #[test]
    fn isohybrid_image_is_kept_whole() {
        if !have(&["sfdisk"]) {
            return;
        }
        let s = Scratch::new("iso");
        let img = s.join("iso.img");
        blank(&img, 64 * MB);
        let mut child = Command::new("sfdisk").args(["-q", p(&img)]).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"label: dos\nstart=2048, size=40960, type=ef\n").unwrap();
        assert!(child.wait().unwrap().success());
        // An ISO 9660 primary volume descriptor at 32 KiB: 20 MiB of 2 KiB blocks.
        let mut pvd = vec![0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[0x50..0x54].copy_from_slice(&10_240u32.to_le_bytes());
        pvd[0x80..0x82].copy_from_slice(&2048u16.to_le_bytes());
        put_bytes(&img, 0x8000, &pvd);
        let layout = layout_of(&img);
        assert_eq!(layout.table, Table::Mbr);
        assert_eq!(util::overlap(&layout.extents, 0, 20 * MB), 20 * MB);
    }

    #[test]
    fn truncated_images_stay_conservative() {
        if !have(&["mkfs.fat", "mke2fs", "mkntfs", "mkfs.exfat"]) {
            return;
        }
        let s = Scratch::new("truncated");
        let img = s.join("fs.img");
        for mkfs in [
            &["mkfs.fat", "-F", "32"][..],
            &["mke2fs", "-q", "-F", "-t", "ext4"][..],
            &["mkntfs", "-F", "-Q", "-q"][..],
            &["mkfs.exfat"][..],
        ] {
            blank(&img, 64 * MB);
            let mut args = mkfs[1..].to_vec();
            args.push(p(&img));
            sh(mkfs[0], &args);
            let data = fs::read(&img).unwrap();
            for cut in [512, 4096, 70_000, MB as usize + 1, 16 * MB as usize, 40 * MB as usize] {
                // A drive bigger than what can be read, and a file system bigger than its drive.
                for size in [data.len() as u64, cut as u64] {
                    if let Ok(layout) = analyze_bytes(&data[..cut], size) {
                        check_invariants(&layout);
                        assert!(layout.partitions.iter().all(|p| !p.understood), "{mkfs:?} cut at {cut}");
                    }
                }
            }
        }
    }

    /// Random corruption in the metadata of real file systems (and tables): no panics, no
    /// broken promises. Positions are drawn from what the analysis reads.
    #[test]
    fn corrupted_metadata_never_panics() {
        let tools = ["sfdisk", "mkfs.fat", "mke2fs", "mkntfs", "mkfs.exfat"];
        if !have(&tools) {
            return;
        }
        let s = Scratch::new("fuzz");
        let mut rng = Rng(0xF022);
        let mut images = Vec::new();
        let img = s.join("fs.img");
        for mkfs in [
            &["mkfs.fat", "-F", "12"][..],
            &["mkfs.fat", "-F", "16"][..],
            &["mkfs.fat", "-F", "32", "-s", "1"][..],
            &["mke2fs", "-q", "-F", "-t", "ext4", "-b", "1024", "-g", "1024", "-O", "meta_bg,^resize_inode"][..],
            &["mke2fs", "-q", "-F", "-t", "ext4", "-b", "4096", "-g", "2048"][..],
            &["mke2fs", "-q", "-F", "-t", "ext2"][..],
            &["mkntfs", "-F", "-Q", "-q", "-c", "1024"][..],
            &["mkfs.exfat"][..],
        ] {
            let size = if mkfs.contains(&"12") { 4 * MB } else { 48 * MB };
            blank(&img, size);
            let mut args = mkfs[1..].to_vec();
            args.push(p(&img));
            sh(mkfs[0], &args);
            images.push((mkfs.join(" "), fs::read(&img).unwrap()));
        }
        // Partition tables too.
        blank(&img, 16 * MB);
        let mut child = Command::new("sfdisk").args(["-q", p(&img)]).stdin(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"label: dos\nstart=2048,size=16384,type=5\nstart=4096,size=2048\nstart=8192,size=2048\n")
            .unwrap();
        assert!(child.wait().unwrap().success());
        images.push(("mbr".into(), fs::read(&img).unwrap()));
        let mut child = Command::new("sfdisk").args(["-q", p(&img)]).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"label: gpt\nsize=4096\nsize=4096\n").unwrap();
        assert!(child.wait().unwrap().success());
        images.push(("gpt".into(), fs::read(&img).unwrap()));

        // More with DD_GUI_SMART_FUZZ_ROUNDS.
        let rounds = std::env::var("DD_GUI_SMART_FUZZ_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(300);
        for (name, mut data) in images {
            let size = data.len() as u64;
            let clean = analyze_bytes(&data, size).unwrap();
            check_invariants(&clean);
            let targets: Vec<Extent> = clean.extents.clone();
            for round in 0..rounds {
                let mut undo = Vec::new();
                for _ in 0..1 + rng.below(8) {
                    let e = targets[rng.below(targets.len() as u64) as usize];
                    // Mostly near the start of an extent, where headers are.
                    let span = if rng.below(2) == 0 { e.len.min(8192) } else { e.len };
                    let at = (e.start + rng.below(span)) as usize;
                    let width = [1, 2, 4, 8][rng.below(4) as usize].min(data.len() - at);
                    let value: [u8; 8] = match rng.below(4) {
                        0 => [0; 8],
                        1 => [0xFF; 8],
                        _ => rng.next().to_le_bytes(),
                    };
                    undo.push((at, data[at..at + width].to_vec()));
                    data[at..at + width].copy_from_slice(&value[..width]);
                }
                match analyze_bytes(&data, size) {
                    Ok(layout) => check_invariants(&layout),
                    Err(err) => panic!("{name} round {round}: {err}"),
                }
                for (at, old) in undo.into_iter().rev() {
                    data[at..at + old.len()].copy_from_slice(&old);
                }
            }
        }
    }

    /// Analysis speed on 2 TB (sparse) images: `cargo test smart -- --ignored two_terabytes --nocapture`.
    #[test]
    #[ignore]
    fn two_terabytes() {
        let s = Scratch::new("2tb");
        let img = s.join("big.img");
        let cases: &[(&str, &[&str])] = &[
            ("mke2fs", &["-q", "-F", "-t", "ext4", "-E", "lazy_itable_init=1,lazy_journal_init=1"]),
            ("mkntfs", &["-F", "-Q", "-q"]),
            ("mkfs.exfat", &[]),
            ("mkfs.fat", &["-F", "32", "-S", "4096"]),
        ];
        for &(mkfs, args) in cases {
            if !have(&[mkfs]) {
                continue;
            }
            blank(&img, 2_000_000_000_000);
            let mut all = args.to_vec();
            all.push(p(&img));
            let t = Instant::now();
            sh(mkfs, &all);
            let made = t.elapsed();
            let t = Instant::now();
            let layout = layout_of(&img);
            eprintln!(
                "{mkfs}: made in {made:?}, analyzed in {:?}: {} bytes in {} extents, {:?}",
                t.elapsed(),
                layout.used(),
                layout.extents.len(),
                layout.partitions[0]
            );
            assert!(layout.partitions[0].understood);
            fs::remove_file(&img).unwrap();
        }
    }
}
