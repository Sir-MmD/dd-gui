//! Turns "copy this to that" into a dd command line, and checks it makes sense.
//!
//! The dd options that only affect speed and safety (block size, direct I/O,
//! flushing, sparse output) are picked automatically for each kind of job.

use crate::drives::Drive;
use crate::engine::{ImageFormat, ImageInfo};
use crate::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub enum Source {
    None,
    File { path: PathBuf, info: ImageInfo },
    Drive(Drive),
    Zeros,
}

#[derive(Clone, Debug)]
pub enum Target {
    None,
    File(PathBuf),
    Drive(Drive),
}

/// What the user can override in Advanced. Everything else is automatic.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// None picks a block size for the job.
    pub block_size: Option<u64>,
    pub noerror: bool,
    /// All three are counted in blocks, like dd does.
    pub count: Option<u64>,
    pub skip: Option<u64>,
    pub seek: Option<u64>,
}

/// The choices in Advanced, after "Auto".
pub const BLOCK_SIZES: [u64; 5] = [512 << 10, 1 << 20, 4 << 20, 16 << 20, 64 << 20];

/// Direct I/O (iflag/oflag=direct) is a Linux-only flag in dd.
pub const DIRECT_IO_AVAILABLE: bool = cfg!(target_os = "linux");
/// Zeros are dd's /dev/zero on Linux and macOS; Windows has none, so DD-GUI's copier writes them.
pub const ZEROS_AVAILABLE: bool = true;

pub struct Plan<'a> {
    pub source: &'a Source,
    pub target: &'a Target,
    pub opts: &'a Options,
    /// Every drive we know of, to tell which one a file is stored on.
    pub drives: &'a [Drive],
}

impl Plan<'_> {
    pub fn block_size(&self) -> u64 {
        // Big blocks make wipes cheaper; 4 MiB suits USB sticks, SD cards and disks alike.
        self.opts.block_size.unwrap_or(if matches!(self.source, Source::Zeros) { 16 << 20 } else { 4 << 20 })
    }

    /// Writing straight to the drive keeps the progress honest (nothing piles up in RAM).
    pub fn direct_out(&self) -> bool {
        DIRECT_IO_AVAILABLE && matches!(self.target, Target::Drive(_))
    }

    /// Reading a drive without the page cache doesn't evict everything else from RAM.
    fn direct_in(&self) -> bool {
        DIRECT_IO_AVAILABLE && matches!(self.source, Source::Drive(_))
    }

    /// Backups skip writing runs of zeros, which saves space on the target disk.
    fn sparse(&self) -> bool {
        matches!((self.source, self.target), (Source::Drive(_), Target::File(_)))
    }

    /// DD-GUI's own copier does this job: compressed images and smart images need unpacking.
    pub fn restores_image(&self) -> bool {
        matches!(
            (self.source, self.target),
            (Source::File { info, .. }, Target::Drive(_)) if info.format != ImageFormat::Raw || info.smart.is_some()
        )
    }

    /// dd's operands, as (key, value) pairs in the order they're shown.
    pub fn operands(&self) -> Vec<(&'static str, String)> {
        let o = self.opts;
        let bs = self.block_size();
        let mut ops = Vec::new();
        if !self.restores_image() {
            match self.source {
                Source::File { path, .. } => ops.push(("if", path_str(path))),
                Source::Drive(d) => ops.push(("if", d.io_path.clone())),
                Source::Zeros => ops.push(("if", "/dev/zero".to_owned())),
                Source::None => {}
            }
        }
        match self.target {
            Target::File(path) => ops.push(("of", path_str(path))),
            Target::Drive(d) => ops.push(("of", d.io_path.clone())),
            Target::None => {}
        }
        ops.push(("bs", fmt::block_size(bs)));

        let mut iflag = Vec::new();
        match (o.count, self.source, self.target) {
            (Some(count), ..) => ops.push(("count", count.to_string())),
            // /dev/zero never ends; stop exactly at the end of the drive instead of
            // running into "No space left on device".
            (None, Source::Zeros, Target::Drive(d)) => {
                let room = d.size.saturating_sub(o.seek.unwrap_or(0).saturating_mul(bs));
                ops.push(("count", room.to_string()));
                iflag.push("count_bytes");
            }
            _ => {}
        }
        if let Some(skip) = o.skip.filter(|&n| n > 0) {
            ops.push(("skip", skip.to_string()));
        }
        if let Some(seek) = o.seek.filter(|&n| n > 0) {
            ops.push(("seek", seek.to_string()));
        }
        if self.direct_in() {
            iflag.push("direct");
        }
        if !iflag.is_empty() {
            ops.push(("iflag", iflag.join(",")));
        }
        if self.direct_out() {
            ops.push(("oflag", "direct".to_owned()));
        }

        let mut conv = Vec::new();
        // Drives get flushed after dd instead (see `sync_after`).
        if !matches!(self.target, Target::Drive(_)) {
            conv.push("fsync");
        }
        if self.sparse() {
            conv.push("sparse");
        }
        if o.noerror {
            conv.extend(["noerror", "sync"]);
        }
        // Windows can't truncate or create a physical drive, so tell dd not to try.
        if cfg!(windows) && matches!(self.target, Target::Drive(_)) {
            conv.extend(["notrunc", "nocreat"]);
        }
        if self.pads_last_block() {
            conv.push("sync");
        }
        if !conv.is_empty() {
            ops.push(("conv", conv.join(",")));
        }
        ops.push(("status", "progress".to_owned()));
        ops
    }

    /// Raw drives on macOS and Windows only take whole sectors, so a last partial
    /// block gets padded with zeros (dd pads it to the full block size).
    fn pads_last_block(&self) -> bool {
        (cfg!(windows) || cfg!(target_os = "macos"))
            && matches!(self.target, Target::Drive(_))
            && !self.opts.noerror
            && self.total_bytes().is_some_and(|t| t % 4096 != 0)
    }

    /// The drive to flush once dd is done; DD-GUI's worker does it, like `dd … && sync`.
    ///
    /// Two reasons not to leave it to `conv=fsync`: uutils dd writes through an
    /// unaligned buffer whenever any `conv=` is given, and O_DIRECT then quietly
    /// falls back to cached writes, making the progress run ahead of the drive.
    /// And on macOS, fsync on a raw disk may fail where the worker's flush falls back
    /// to asking the drive to empty its cache.
    pub fn sync_after(&self) -> Option<String> {
        match self.target {
            Target::Drive(d) => Some(d.io_path.clone()),
            _ => None,
        }
    }

    pub fn args(&self) -> Vec<String> {
        self.operands().into_iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// The command as shell words: `("", word)` for commands, `("_", word)` for
    /// plain arguments, `(key, value)` for dd operands. For image restores this is
    /// the equivalent pipeline (`xz -dc image | dd of=…`).
    pub fn words(&self) -> Vec<(&'static str, String)> {
        let mut words = Vec::new();
        if let (true, Source::File { path, info }, Target::Drive(d)) = (self.restores_image(), self.source, self.target) {
            let unpack = match info.format {
                // Virtual disks: qemu-img converts them straight onto the drive.
                ImageFormat::Dmg | ImageFormat::Vhd | ImageFormat::Vhdx | ImageFormat::Vmdk | ImageFormat::Qcow2 => {
                    words.push(("", "qemu-img convert -O raw".to_owned()));
                    words.push(("_", path_str(path)));
                    words.push(("_", d.io_path.clone()));
                    words.push(("", "&& sync".to_owned()));
                    return words;
                }
                ImageFormat::Xz => "xz -dc",
                ImageFormat::Zstd => "zstd -dc",
                ImageFormat::Zip => "unzip -p",
                ImageFormat::Bzip2 => "bzip2 -dc",
                ImageFormat::Lz4 => "lz4 -dc",
                ImageFormat::Lzma => "xz --format=lzma -dc",
                ImageFormat::SevenZ => "7z x -so",
                ImageFormat::Tar => "tar -xOf",
                ImageFormat::Gzip | ImageFormat::Raw => "gzip -dc",
            };
            words.push(("", unpack.to_owned()));
            words.push(("_", path_str(path)));
            words.push(("", "|".to_owned()));
        }
        words.push(("", "dd".to_owned()));
        words.extend(self.operands());
        if self.sync_after().is_some() {
            words.push(("", "&& sync".to_owned()));
        }
        words
    }

    /// The command as you'd type it in a shell.
    pub fn command_line(&self) -> String {
        self.words()
            .into_iter()
            .map(|(k, v)| match k {
                "" => v,
                "_" => shell_quote(&v),
                k => format!("{k}={}", shell_quote(&v)),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Plain words for what DD-GUI picked, for the Advanced dialog.
    pub fn auto_summary(&self) -> String {
        let mut parts = vec![format!("{} blocks", fmt::block_size(self.block_size()))];
        if self.direct_in() || self.direct_out() {
            parts.push("direct I/O on drives".into());
        }
        parts.push("flush to disk at the end".into());
        if self.sparse() {
            parts.push("sparse image (zeros take no space)".into());
        }
        if self.restores_image() {
            parts.push("unpacked on the fly".into());
        }
        let mut text = parts.join(" · ");
        text[..1].make_ascii_uppercase();
        text
    }

    /// Bytes the target needs to hold, when known.
    fn source_size(&self) -> Option<u64> {
        match self.source {
            // Copied as-is unless it's being written to a drive.
            Source::File { info, .. } if !self.restores_image() => Some(info.size),
            Source::File { info, .. } => match info.smart {
                Some(smart) => Some(smart.disk_size),
                None if info.format == ImageFormat::Raw => Some(info.size),
                None => info.uncompressed,
            },
            Source::Drive(d) => Some(d.size),
            Source::Zeros | Source::None => None,
        }
    }

    fn target_room(&self) -> Option<u64> {
        match self.target {
            Target::Drive(d) => Some(d.size.saturating_sub(self.opts.seek.unwrap_or(0).saturating_mul(self.block_size()))),
            _ => None,
        }
    }

    /// How many bytes dd will copy, when that's known up front.
    pub fn total_bytes(&self) -> Option<u64> {
        let bs = self.block_size();
        let limit = self.opts.count.map(|c| c.saturating_mul(bs));
        let available = match self.source {
            Source::Zeros => self.target_room(),
            _ => self.source_size().map(|s| s.saturating_sub(self.opts.skip.unwrap_or(0).saturating_mul(bs))),
        };
        match (available, limit) {
            (Some(a), Some(l)) => Some(a.min(l)),
            (a, l) => a.or(l),
        }
    }

    /// Anything that stops the job from starting, in plain words.
    pub fn problem(&self) -> Option<String> {
        match (self.source, self.target) {
            (Source::None, _) => return Some("Choose what to copy from.".into()),
            (_, Target::None) => return Some("Choose where to write.".into()),
            (Source::Drive(a), Target::Drive(b)) if a.path == b.path => {
                return Some("Source and target are the same drive.".into());
            }
            (Source::File { path: a, .. }, Target::File(b)) if same_file(a, b) => {
                return Some("Source and target are the same file.".into());
            }
            (Source::File { path, .. }, Target::Drive(d)) if lives_on(path, d, self.drives) => {
                return Some(format!("The source file is stored on {}, the drive you're writing to.", d.name));
            }
            (Source::Drive(d), Target::File(path)) if lives_on(path, d, self.drives) => {
                return Some(format!("The image would be saved onto {}, the drive being read.", d.name));
            }
            (Source::Zeros, Target::File(_)) if self.opts.count.is_none() => {
                return Some("Set a block count in Advanced to limit the file size.".into());
            }
            _ => {}
        }
        if let Target::Drive(d) = self.target {
            if d.system {
                return Some(format!("{} holds your operating system and can't be overwritten.", d.name));
            }
            if d.read_only {
                return Some(format!("{} is read-only.", d.name));
            }
        }
        let o = self.opts;
        if self.restores_image() && (o.count.is_some() || o.skip.is_some() || o.seek.is_some()) {
            return Some("Count, skip and seek don't apply when writing compressed or DD-GUI images. Clear them in Advanced.".into());
        }
        if self.pads_last_block()
            && let (Some(total), Some(room)) = (self.total_bytes(), self.target_room())
            && total.div_ceil(self.block_size()).saturating_mul(self.block_size()) > room
        {
            return Some("The image ends mid-block and won't fit once padded. Pick a smaller block size in Advanced.".into());
        }
        if let (Some(total), Some(room)) = (self.total_bytes(), self.target_room())
            && total > room
        {
            return Some(format!(
                "The source ({}) doesn't fit on the target drive ({}).",
                fmt::bytes(total),
                fmt::bytes(room)
            ));
        }
        None
    }

    /// Worth knowing, but doesn't stop the job.
    pub fn note(&self) -> Option<String> {
        let Source::File { info, .. } = self.source else { return None };
        let virtual_disk =
            matches!(info.format, ImageFormat::Dmg | ImageFormat::Vhd | ImageFormat::Vhdx | ImageFormat::Vmdk | ImageFormat::Qcow2);
        match (info.smart, info.format, self.target) {
            (Some(_), _, Target::Drive(_)) => Some("DD-GUI smart image: only the space in use gets written, so this is quick.".into()),
            (None, ImageFormat::Raw, _) => None,
            (_, _, Target::Drive(_)) if virtual_disk => Some("Virtual disk: DD-GUI turns it into a plain disk while writing.".into()),
            (_, _, Target::Drive(_)) => Some("Compressed image: DD-GUI unpacks it while writing.".into()),
            (_, _, _) => Some("Packed image: it gets copied as-is. Write it to a drive to unpack it.".into()),
        }
    }

    /// Reading or writing a drive needs admin rights (on Windows the whole app has them).
    pub fn needs_elevation(&self) -> bool {
        if cfg!(windows) || running_as_root() {
            return false;
        }
        matches!(self.source, Source::Drive(_)) || matches!(self.target, Target::Drive(_))
    }

    pub fn action(&self) -> &'static str {
        match (self.source, self.target) {
            (Source::File { .. }, Target::Drive(_)) => "Write to drive",
            (Source::Drive(_), Target::File(_)) => "Create image",
            (Source::Drive(_), Target::Drive(_)) => "Clone drive",
            (Source::Zeros, Target::Drive(_)) => "Wipe drive",
            (Source::File { .. }, Target::File(_)) => "Copy file",
            (Source::Zeros, Target::File(_)) => "Write zeros",
            _ => "Start",
        }
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Is `path` stored on a file system mounted from drive `d`? The drive with the
/// longest mount point that contains the path wins, so "/" only counts when
/// nothing more specific does.
fn lives_on(path: &Path, d: &Drive, drives: &[Drive]) -> bool {
    let dir = if path.exists() { path.to_path_buf() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
    let Ok(path) = dir.canonicalize() else { return false };
    let path = plain_path(&path);
    let best = drives
        .iter()
        .chain(std::iter::once(d))
        .flat_map(|drive| drive.mountpoints.iter().map(move |m| (m, drive)))
        .filter(|(mount, _)| path_starts_with(&path, mount))
        .max_by_key(|(mount, _)| mount.len());
    best.is_some_and(|(_, drive)| drive.path == d.path)
}

/// Windows' canonical paths start with `\\?\`, which its mount points don't.
fn plain_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
}

fn path_starts_with(path: &str, mount: &str) -> bool {
    if cfg!(windows) {
        let (path, mount) = (path.to_lowercase(), mount.to_lowercase());
        Path::new(&path).starts_with(Path::new(&mount))
    } else {
        Path::new(path).starts_with(Path::new(mount))
    }
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

fn shell_quote(s: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+,:=@%".contains(c);
    if !s.is_empty() && s.chars().all(plain) {
        s.to_owned()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SmartInfo;

    fn raw(size: u64) -> ImageInfo {
        ImageInfo { format: ImageFormat::Raw, size, uncompressed: Some(size), smart: None }
    }

    fn usb(size: u64) -> Drive {
        Drive {
            path: "/dev/sdb".into(),
            io_path: "/dev/sdb".into(),
            name: "USB stick".into(),
            size,
            bus: "USB".into(),
            kind: crate::drives::DriveKind::Usb,
            removable: true,
            system: false,
            read_only: false,
            mountpoints: vec![],
            volumes: vec![],
            unmount: vec![],
            lock: vec![],
        }
    }

    #[test]
    fn image_to_drive() {
        let source = Source::File { path: "/home/me/My Images/os.iso".into(), info: raw(5_000_000_000) };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.problem().is_none());
        assert_eq!(plan.total_bytes(), Some(5_000_000_000));
        let expected = if DIRECT_IO_AVAILABLE {
            "dd if='/home/me/My Images/os.iso' of=/dev/sdb bs=4M oflag=direct status=progress && sync"
        } else {
            "dd if='/home/me/My Images/os.iso' of=/dev/sdb bs=4M status=progress && sync"
        };
        assert_eq!(plan.command_line(), expected);
        assert!(plan.sync_after().is_some());
    }

    #[test]
    fn backups_are_sparse_and_flushed() {
        let source = Source::Drive(usb(8_000_000_000));
        let target = Target::File("/tmp/backup.img".into());
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        let line = plan.command_line();
        assert!(line.contains("conv=fsync,sparse"), "{line}");
        assert_eq!(line.contains("iflag=direct"), DIRECT_IO_AVAILABLE, "{line}");
    }

    #[test]
    fn compressed_images_are_unpacked() {
        let info = ImageInfo { format: ImageFormat::Xz, size: 900_000_000, uncompressed: Some(4_000_000_000), smart: None };
        let source = Source::File { path: "/tmp/raspios.img.xz".into(), info };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.restores_image());
        assert!(plan.problem().is_none());
        assert!(plan.command_line().starts_with("xz -dc /tmp/raspios.img.xz | dd of=/dev/sdb bs=4M"));
        assert!(plan.note().is_some());
    }

    #[test]
    fn smart_images_need_room_for_the_whole_drive() {
        let smart = SmartInfo { disk_size: 64_000_000_000, used: 9_000_000_000 };
        let info = ImageInfo { format: ImageFormat::Gzip, size: 5_000_000_000, uncompressed: Some(64_000_000_000), smart: Some(smart) };
        let source = Source::File { path: "/tmp/backup.img.gz".into(), info };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.problem().unwrap().contains("doesn't fit"));
    }

    #[test]
    fn restores_refuse_count_skip_seek() {
        let info = ImageInfo { format: ImageFormat::Xz, size: 900_000_000, uncompressed: None, smart: None };
        let source = Source::File { path: "/tmp/a.img.xz".into(), info };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options { seek: Some(2), ..Options::default() };
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.problem().unwrap().contains("don't apply"));
    }

    #[test]
    fn compressed_file_copied_as_is_counts_file_size() {
        let info = ImageInfo { format: ImageFormat::Xz, size: 900_000_000, uncompressed: Some(4_000_000_000), smart: None };
        let source = Source::File { path: "/tmp/a.img.xz".into(), info };
        let target = Target::File("/tmp/b.img.xz".into());
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert_eq!(plan.total_bytes(), Some(900_000_000));
    }

    #[test]
    fn huge_seek_does_not_overflow() {
        let source = Source::File { path: "/tmp/a.img".into(), info: raw(1 << 20) };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options { seek: Some(99_999_999_999_999), ..Options::default() };
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.problem().unwrap().contains("doesn't fit"));
    }

    #[test]
    fn root_mount_counts_when_nothing_more_specific_does() {
        let home = std::env::temp_dir();
        let mut system = usb(256_000_000_000);
        system.path = "/dev/nvme0n1".into();
        system.mountpoints = vec!["/".into()];
        let file = home.join("backup.img");
        assert!(lives_on(&file, &system, std::slice::from_ref(&system)));
        let mut tmp_drive = usb(8_000_000_000);
        tmp_drive.mountpoints = vec![home.canonicalize().unwrap().to_string_lossy().into_owned()];
        assert!(!lives_on(&file, &system, &[system.clone(), tmp_drive]));
    }

    #[test]
    fn too_small() {
        let source = Source::File { path: "/tmp/big.img".into(), info: raw(64_000_000_000) };
        let target = Target::Drive(usb(32_000_000_000));
        let o = Options::default();
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert!(plan.problem().unwrap().contains("doesn't fit"));
    }

    #[test]
    fn zeros_stop_at_end_of_drive() {
        let target = Target::Drive(usb(8_000_000_000));
        let o = Options::default();
        let plan = Plan { source: &Source::Zeros, target: &target, opts: &o, drives: &[] };
        let args = plan.args();
        assert!(args.contains(&"count=8000000000".to_owned()));
        assert!(args.contains(&"iflag=count_bytes".to_owned()));
        assert!(args.contains(&"bs=16M".to_owned()));
        assert_eq!(plan.total_bytes(), Some(8_000_000_000));
    }

    #[test]
    fn count_and_skip_limit_total() {
        let source = Source::File { path: "/tmp/a.img".into(), info: raw(100 << 20) };
        let target = Target::File("/tmp/b.img".into());
        // 100 MiB file, skipping 5 blocks of 4 MiB leaves 80 MiB.
        let o = Options { count: Some(10), skip: Some(5), ..Options::default() };
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert_eq!(plan.total_bytes(), Some(40 << 20));
        let o = Options { count: Some(100), skip: Some(5), ..Options::default() };
        let plan = Plan { source: &source, target: &target, opts: &o, drives: &[] };
        assert_eq!(plan.total_bytes(), Some(80 << 20));
    }
}
