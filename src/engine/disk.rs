//! Raw I/O on drives and image files: aligned buffers, direct I/O, whole sectors.
//!
//! Drives are opened unbuffered wherever the OS allows it (O_DIRECT on Linux, the raw
//! /dev/rdiskN on macOS, FILE_FLAG_NO_BUFFERING and FILE_FLAG_WRITE_THROUGH on Windows),
//! so the progress follows the drive rather than a cache in RAM. Such drives only take
//! whole sectors at sector offsets, from aligned memory. `Disk` turns every read and
//! write into that: reads fetch the whole sectors around what was asked for, and a
//! write that covers part of a sector reads that sector, patches it and writes it back.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

/// Buffers start on this boundary, and chunk sizes are multiples of it.
pub const ALIGN: usize = 4096;

pub fn round_up(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

/// A zeroed buffer that starts on a 4096-byte boundary, as direct I/O wants.
pub struct AlignedBuf {
    vec: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        let vec = vec![0u8; len + ALIGN];
        let start = (ALIGN - vec.as_ptr() as usize % ALIGN) % ALIGN;
        Self { vec, start, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.vec[self.start..self.start + self.len]
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.vec[self.start..self.start + self.len]
    }
}

/// Is `path` a drive rather than a file? (Block and character devices, `\\.\PhysicalDriveN`.)
#[cfg(unix)]
pub fn is_drive(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .is_ok_and(|m| m.file_type().is_block_device() || m.file_type().is_char_device())
}

#[cfg(windows)]
pub fn is_drive(path: &Path) -> bool {
    path.to_string_lossy().starts_with(r"\\.\")
}

#[cfg(not(any(unix, windows)))]
pub fn is_drive(_path: &Path) -> bool {
    false
}

/// A drive or an image file, read and written at absolute offsets.
pub struct Disk {
    file: File,
    pub path: PathBuf,
    /// The drive's size, or the file's length when it was opened.
    pub size: u64,
    pub is_drive: bool,
    /// Transfers have to be whole sectors of this many bytes, at sector offsets and
    /// from aligned memory (direct I/O, raw devices). 1 when anything goes.
    sector: u64,
    /// Tests: fail transfers that such a drive would refuse.
    #[cfg(test)]
    strict: bool,
}

impl Disk {
    /// Opens a drive or an image file for reading.
    pub fn open_read(path: &Path) -> io::Result<Disk> {
        Self::open(path, false)
    }

    /// Opens a drive for writing. Never creates or truncates anything.
    pub fn open_drive(path: &Path) -> io::Result<Disk> {
        Self::open(path, true)
    }

    /// Creates an image file, or empties an existing one.
    pub fn create(path: &Path) -> io::Result<Disk> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self::new(file, path, 0, false, 1))
    }

    fn new(file: File, path: &Path, size: u64, is_drive: bool, sector: u64) -> Disk {
        Disk {
            file,
            path: path.into(),
            size,
            is_drive,
            sector,
            #[cfg(test)]
            strict: false,
        }
    }

    fn open(path: &Path, write: bool) -> io::Result<Disk> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(write);
        if !is_drive(path) {
            let file = opts.open(path)?;
            let size = file.metadata()?.len();
            return Ok(Self::new(file, path, size, false, 1));
        }
        let (file, size, sector) = platform::open_drive(path, &opts, write)?;
        Ok(Self::new(file, path, size, true, sector.max(1)))
    }

    /// Tests: a file that acts like a drive whose sectors hold `sector` bytes, and which
    /// refuses anything but whole sectors from aligned memory (as O_DIRECT does).
    #[cfg(all(test, unix))]
    pub fn strict(path: &Path, sector: u64) -> io::Result<Disk> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        let mut disk = Self::new(file, path, size, true, sector);
        disk.strict = true;
        Ok(disk)
    }

    /// The boundary that reads and writes are best aligned to: a sector, and at least 4 KiB.
    pub fn align(&self) -> u64 {
        self.sector.max(ALIGN as u64)
    }

    /// Reads `len` bytes at `off` into the start of `buf`.
    pub fn read_at(&self, off: u64, buf: &mut AlignedBuf, len: usize) -> io::Result<()> {
        let s = self.sector;
        if off.is_multiple_of(s) && (len as u64).is_multiple_of(s) {
            return self.pread(&mut buf[..len], off);
        }
        // Whole sectors straight into `buf`, as far as it has room for them.
        let mut body = 0;
        if off.is_multiple_of(s) {
            let whole = round_up(len as u64, s) as usize;
            if whole <= buf.len() {
                return self.pread(&mut buf[..whole], off);
            }
            body = len - len % s as usize;
            self.pread(&mut buf[..body], off)?;
        }
        // The rest (a partial last sector, or everything when `off` is inside a sector):
        // the sectors around it, through a buffer of their own.
        let from = off + body as u64;
        let rest = len - body;
        let start = from - from % s;
        let mut whole = AlignedBuf::new((round_up(from + rest as u64, s) - start) as usize);
        self.pread(&mut whole, start)?;
        let at = (from - start) as usize;
        buf[body..len].copy_from_slice(&whole[at..at + rest]);
        Ok(())
    }

    /// Writes `data`, which starts on an aligned address (in an `AlignedBuf`), at `off`.
    pub fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        let s = self.sector;
        let len = data.len() as u64;
        if off.is_multiple_of(s) && len.is_multiple_of(s) {
            return self.pwrite(data, off);
        }
        // Whole sectors go straight to the drive (rare: normally only the very end of an
        // image is a partial sector).
        let mut body = 0;
        if off.is_multiple_of(s) {
            body = (len - len % s) as usize;
            self.pwrite(&data[..body], off)?;
        }
        // Partial sectors: read what they hold, patch in the new bytes, write them back.
        let from = off + body as u64;
        let rest = &data[body..];
        let start = from - from % s;
        let span = (round_up(from + rest.len() as u64, s) - start) as usize;
        let s = s as usize;
        let mut whole = AlignedBuf::new(span);
        let head = (from - start) as usize;
        let end = head + rest.len();
        if head > 0 || (end % s != 0 && span == s) {
            self.pread(&mut whole[..s], start)?;
        }
        if end % s != 0 && span > s {
            self.pread(&mut whole[span - s..], start + (span - s) as u64)?;
        }
        whole[head..end].copy_from_slice(rest);
        self.pwrite(&whole, start)
    }

    /// Flushes everything written to the hardware.
    pub fn sync(&self) -> io::Result<()> {
        crate::worker::sync(&self.file)
    }

    /// Sets an image file's length (for a smart image that ends in free space).
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    /// For sequential writes to an image file.
    pub fn file(&self) -> &File {
        &self.file
    }

    fn pread(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.check(off, buf)?;
        read_exact_at(&self.file, buf, off)
    }

    fn pwrite(&self, data: &[u8], off: u64) -> io::Result<()> {
        self.check(off, data)?;
        write_all_at(&self.file, data, off)
    }

    /// Tests: what a strict drive refuses (EINVAL, as Linux says for misaligned O_DIRECT).
    #[cfg(test)]
    fn check(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        let s = self.sector;
        let aligned = off.is_multiple_of(s)
            && (buf.len() as u64).is_multiple_of(s)
            && (buf.as_ptr() as u64).is_multiple_of(s.min(ALIGN as u64));
        if self.strict && !aligned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "misaligned transfer: {} bytes at {off} from {:p} (sectors of {s} bytes)",
                    buf.len(),
                    buf.as_ptr()
                ),
            ));
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn check(&self, _off: u64, _buf: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match platform::pread(file, buf, off) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected end of data",
                ));
            }
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_at(file: &File, mut data: &[u8], mut off: u64) -> io::Result<()> {
    while !data.is_empty() {
        match platform::pwrite(file, data, off) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "no space left")),
            Ok(n) => {
                data = &data[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `Read + Seek` over a disk, for the analysis, which reads small pieces anywhere. It
/// only ever asks the disk for whole, aligned blocks (64 KiB to 1 MiB of them) and
/// serves the small reads from those.
pub struct DiskReader<'a> {
    disk: &'a Disk,
    pos: u64,
    buf: AlignedBuf,
    /// Where `buf` came from, and how much of it is filled.
    start: u64,
    filled: usize,
}

impl<'a> DiskReader<'a> {
    pub fn new(disk: &'a Disk) -> Self {
        Self {
            disk,
            pos: 0,
            buf: AlignedBuf::new(round_up(1 << 20, disk.align()) as usize),
            start: 0,
            filled: 0,
        }
    }
}

impl Read for DiskReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let size = self.disk.size;
        if out.is_empty() || self.pos >= size {
            return Ok(0);
        }
        if !(self.start..self.start + self.filled as u64).contains(&self.pos) {
            let align = self.disk.align();
            let start = self.pos - self.pos % align;
            // Small reads fetch 64 KiB around them, big ones up to 1 MiB at a time. Only
            // the end of the drive may cut that short (drives end on a whole sector).
            let want = round_up(self.pos + out.len() as u64 - start, align)
                .clamp(64 << 10, self.buf.len() as u64)
                .min(size - start) as usize;
            self.filled = 0;
            self.disk.read_at(start, &mut self.buf, want)?;
            (self.start, self.filled) = (start, want);
        }
        let at = (self.pos - self.start) as usize;
        let n = out.len().min(self.filled - at);
        out[..n].copy_from_slice(&self.buf[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for DiskReader<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.disk.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;

    /// `_IOR(0x12, 114, size_t)` from <linux/fs.h>, which the libc crate lacks.
    const BLKGETSIZE64: u64 = {
        let read: u64 = if cfg!(any(
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "mips",
            target_arch = "mips32r6",
            target_arch = "mips64",
            target_arch = "mips64r6",
            target_arch = "sparc",
            target_arch = "sparc64"
        )) {
            2 << 29
        } else {
            2 << 30
        };
        read | (size_of::<usize>() as u64) << 16 | 0x12 << 8 | 114
    };

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.read_at(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.write_at(data, off)
    }

    /// Opens with O_DIRECT (so progress tracks the drive, not the page cache), or
    /// without it where that isn't supported. O_DIRECT takes whole logical sectors.
    pub fn open_drive(
        path: &Path,
        opts: &OpenOptions,
        _write: bool,
    ) -> io::Result<(File, u64, u64)> {
        let (mut file, direct) = match opts.clone().custom_flags(libc::O_DIRECT).open(path) {
            Ok(file) => (file, true),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => (opts.open(path)?, false),
            Err(e) => return Err(e),
        };
        let fd = file.as_raw_fd();
        let mut bytes: u64 = 0;
        // SAFETY: BLKGETSIZE64 writes one u64.
        let size = if unsafe { libc::ioctl(fd, BLKGETSIZE64 as _, &mut bytes) } == 0 {
            bytes
        } else {
            // Not a block device (a character device, say).
            file.seek(SeekFrom::End(0))?
        };
        let mut sector: libc::c_int = 0;
        // SAFETY: BLKSSZGET writes one int.
        let known =
            unsafe { libc::ioctl(fd, libc::BLKSSZGET as _, &mut sector) } == 0 && sector > 0;
        let sector = match (direct, known) {
            (false, _) => 1,
            (true, true) => sector as u64,
            (true, false) => 512,
        };
        Ok((file, size, sector))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;

    // _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t) from <sys/disk.h>.
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.read_at(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.write_at(data, off)
    }

    /// Raw devices (/dev/rdiskN) only take whole blocks, and can't seek to their end:
    /// the size is the block count times the block size.
    pub fn open_drive(
        path: &Path,
        opts: &OpenOptions,
        _write: bool,
    ) -> io::Result<(File, u64, u64)> {
        let file = opts.open(path)?;
        let fd = file.as_raw_fd();
        let (mut block, mut count) = (0u32, 0u64);
        // SAFETY: these ioctls write one u32 and one u64.
        if unsafe { libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut block) } != 0
            || unsafe { libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut count) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if block == 0 {
            return Err(io::Error::other(format!(
                "{} reports sectors of 0 bytes",
                path.display()
            )));
        }
        // No caching for /dev/diskN either (the raw /dev/rdiskN never caches).
        // SAFETY: fcntl on our own descriptor.
        unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
        Ok((file, count * block as u64, block as u64))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DISK_GEOMETRY, DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    };

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.seek_read(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.seek_write(data, off)
    }

    /// Asks the driver for a `T`, into a buffer of `room` bytes (at least a `T`).
    fn ioctl<T: Copy>(file: &File, code: u32, room: usize) -> io::Result<T> {
        // u64s keep the buffer aligned for any of these structs.
        let mut out = vec![0u64; room.max(size_of::<T>()).div_ceil(8)];
        let mut returned = 0u32;
        // SAFETY: the buffer is as big as we say, and big enough for a T.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                code,
                std::ptr::null(),
                0,
                out.as_mut_ptr().cast(),
                (out.len() * 8) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        } != 0;
        if !ok {
            return Err(io::Error::last_os_error());
        }
        if (returned as usize) < size_of::<T>() {
            return Err(io::Error::other(
                "the drive's driver gave an incomplete answer",
            ));
        }
        // SAFETY: the driver filled in a T at the start of the buffer.
        Ok(unsafe { out.as_ptr().cast::<T>().read_unaligned() })
    }

    /// The logical sector size, which unbuffered I/O has to use.
    fn sector_size(file: &File) -> Option<u32> {
        // The _EX answer carries partition and detection data after the geometry; room for it.
        let size = ioctl::<DISK_GEOMETRY_EX>(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, 256)
            .map(|g| g.Geometry.BytesPerSector)
            .or_else(|_| {
                ioctl::<DISK_GEOMETRY>(file, IOCTL_DISK_GET_DRIVE_GEOMETRY, 0)
                    .map(|g| g.BytesPerSector)
            })
            .ok()?;
        (size >= 512 && size.is_power_of_two()).then_some(size)
    }

    /// `\\.\PhysicalDriveN`: whole sectors only. Opened unbuffered (writes straight
    /// through too), so the progress is what reached the drive. std opens it with
    /// OPEN_EXISTING and shares it for reading and writing; the volumes on it are locked
    /// by the plumbing.
    pub fn open_drive(
        path: &Path,
        opts: &OpenOptions,
        write: bool,
    ) -> io::Result<(File, u64, u64)> {
        let flags = FILE_FLAG_NO_BUFFERING | if write { FILE_FLAG_WRITE_THROUGH } else { 0 };
        let file = opts.clone().custom_flags(flags).open(path)?;
        let length: GET_LENGTH_INFORMATION = ioctl(&file, IOCTL_DISK_GET_LENGTH_INFO, 0)?;
        let sector = sector_size(&file).unwrap_or(512);
        Ok((file, length.Length as u64, sector as u64))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;
    use std::os::unix::fs::FileExt;

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.read_at(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.write_at(data, off)
    }

    pub fn open_drive(
        path: &Path,
        opts: &OpenOptions,
        _write: bool,
    ) -> io::Result<(File, u64, u64)> {
        let mut file = opts.open(path)?;
        let size = file.seek(SeekFrom::End(0))?;
        Ok((file, size, 1))
    }
}
