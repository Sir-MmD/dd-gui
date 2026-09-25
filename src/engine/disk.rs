//! Raw I/O on drives and image files: aligned buffers, direct I/O, whole sectors.

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
    /// Set when transfers have to be whole sectors at sector offsets: direct I/O on
    /// Linux, raw devices on macOS and Windows.
    sector: Option<u64>,
}

impl Disk {
    /// Opens a drive or an image file for reading.
    pub fn open_read(path: &Path) -> io::Result<Disk> {
        Self::open(path, OpenOptions::new().read(true))
    }

    /// Opens a drive for writing. Never creates or truncates anything.
    pub fn open_drive(path: &Path) -> io::Result<Disk> {
        Self::open(path, OpenOptions::new().read(true).write(true))
    }

    /// Creates an image file, or empties an existing one.
    pub fn create(path: &Path) -> io::Result<Disk> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Disk {
            file,
            path: path.into(),
            size: 0,
            is_drive: false,
            sector: None,
        })
    }

    fn open(path: &Path, opts: &OpenOptions) -> io::Result<Disk> {
        if !is_drive(path) {
            let file = opts.open(path)?;
            let size = file.metadata()?.len();
            return Ok(Disk {
                file,
                path: path.into(),
                size,
                is_drive: false,
                sector: None,
            });
        }
        let (file, size, sector) = platform::open_drive(path, opts)?;
        Ok(Disk {
            file,
            path: path.into(),
            size,
            is_drive: true,
            sector,
        })
    }

    /// Reads `len` bytes at `off` into the start of `buf`.
    pub fn read_at(&self, off: u64, buf: &mut AlignedBuf, len: usize) -> io::Result<()> {
        let Some(sector) = self.sector else {
            return read_exact_at(&self.file, &mut buf[..len], off);
        };
        if !off.is_multiple_of(sector) {
            // Rare: read the whole sectors around it.
            let start = off - off % sector;
            let span = (round_up(off + len as u64, sector) - start) as usize;
            let mut whole = AlignedBuf::new(span);
            self.read_at(start, &mut whole, span)?;
            let at = (off - start) as usize;
            buf[..len].copy_from_slice(&whole[at..at + len]);
            return Ok(());
        }
        // Whole sectors, but never past the end of the drive (or of `buf`).
        let want = round_up(len as u64, sector)
            .min(self.size.saturating_sub(off))
            .min(buf.len() as u64)
            .max(len as u64);
        read_exact_at(&self.file, &mut buf[..want as usize], off)
    }

    /// Writes `data`, which starts on an aligned address (in an `AlignedBuf`), at `off`.
    pub fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        let len = data.len() as u64;
        match self.sector {
            Some(sector) if !off.is_multiple_of(sector) || !len.is_multiple_of(sector) => {
                // A partial sector: normally just the very end of an image.
                let body = if off.is_multiple_of(sector) {
                    (len - len % sector) as usize
                } else {
                    0
                };
                write_all_at(&self.file, &data[..body], off)?;
                platform::write_partial(&self.file, off + body as u64, &data[body..], sector)
            }
            _ => write_all_at(&self.file, data, off),
        }
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

/// `Read + Seek` over a disk, for the analysis, which reads small pieces anywhere.
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
            buf: AlignedBuf::new(1 << 20),
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
            let start = self.pos - self.pos % ALIGN as u64;
            // Small reads fetch 64 KiB around them, big ones up to 1 MiB at a time.
            let want = round_up(self.pos + out.len() as u64 - start, ALIGN as u64)
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

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.read_at(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.write_at(data, off)
    }

    /// Opens with O_DIRECT (so progress tracks the drive, not the page cache), or
    /// without it where that isn't supported.
    pub fn open_drive(path: &Path, opts: &OpenOptions) -> io::Result<(File, u64, Option<u64>)> {
        let (mut file, direct) = match opts.clone().custom_flags(libc::O_DIRECT).open(path) {
            Ok(file) => (file, true),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => (opts.open(path)?, false),
            Err(e) => return Err(e),
        };
        let size = file.seek(SeekFrom::End(0))?;
        let mut sector: libc::c_int = 0;
        // SAFETY: BLKSSZGET writes one int.
        let ok = unsafe { libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &mut sector) } == 0;
        let sector = if ok && sector > 0 { sector as u64 } else { 512 };
        Ok((file, size, direct.then_some(sector)))
    }

    /// Like dd: the tail goes through the page cache, which fills in the rest of its sector.
    pub fn write_partial(file: &File, off: u64, data: &[u8], _sector: u64) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: fcntl on our own descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_DIRECT) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = write_all_at(file, data, off);
        // SAFETY: as above.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
        result
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

    /// Raw devices (/dev/rdiskN) only take whole blocks, and can't seek to their end.
    pub fn open_drive(path: &Path, opts: &OpenOptions) -> io::Result<(File, u64, Option<u64>)> {
        let file = opts.open(path)?;
        let (mut block, mut count) = (0u32, 0u64);
        // SAFETY: these ioctls write one u32 and one u64.
        let ok = unsafe {
            libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut block) == 0
                && libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKCOUNT, &mut count) == 0
        };
        if !ok || block == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((file, count * block as u64, Some(block as u64)))
    }

    pub fn write_partial(file: &File, off: u64, data: &[u8], sector: u64) -> io::Result<()> {
        read_modify_write(file, off, data, sector)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DISK_GEOMETRY, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
        IOCTL_DISK_GET_LENGTH_INFO,
    };

    pub fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
        file.seek_read(buf, off)
    }

    pub fn pwrite(file: &File, data: &[u8], off: u64) -> io::Result<usize> {
        file.seek_write(data, off)
    }

    fn ioctl<T: Default>(file: &File, code: u32) -> io::Result<T> {
        let mut out = T::default();
        let mut returned = 0u32;
        // SAFETY: `out` is a plain struct of the size we pass.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                code,
                std::ptr::null(),
                0,
                (&mut out as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        } != 0;
        if ok {
            Ok(out)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// `\\.\PhysicalDriveN`: whole sectors only. std opens it with OPEN_EXISTING and
    /// shares it for reading and writing; the volumes on it are locked by the plumbing.
    pub fn open_drive(path: &Path, opts: &OpenOptions) -> io::Result<(File, u64, Option<u64>)> {
        let file = opts.open(path)?;
        let length: GET_LENGTH_INFORMATION = ioctl(&file, IOCTL_DISK_GET_LENGTH_INFO)?;
        let geometry: DISK_GEOMETRY = ioctl(&file, IOCTL_DISK_GET_DRIVE_GEOMETRY)?;
        let sector = if geometry.BytesPerSector > 0 {
            geometry.BytesPerSector as u64
        } else {
            512
        };
        Ok((file, length.Length as u64, Some(sector)))
    }

    pub fn write_partial(file: &File, off: u64, data: &[u8], sector: u64) -> io::Result<()> {
        read_modify_write(file, off, data, sector)
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

    pub fn open_drive(path: &Path, opts: &OpenOptions) -> io::Result<(File, u64, Option<u64>)> {
        let mut file = opts.open(path)?;
        let size = file.seek(SeekFrom::End(0))?;
        Ok((file, size, None))
    }

    pub fn write_partial(file: &File, off: u64, data: &[u8], _sector: u64) -> io::Result<()> {
        write_all_at(file, data, off)
    }
}

/// Writes a partial sector on a device that only takes whole ones: reads the sectors
/// around it, patches them, and writes them back.
#[cfg(any(target_os = "macos", windows))]
fn read_modify_write(file: &File, off: u64, data: &[u8], sector: u64) -> io::Result<()> {
    let start = off - off % sector;
    let span = (round_up(off + data.len() as u64, sector) - start) as usize;
    let mut whole = AlignedBuf::new(span);
    read_exact_at(file, &mut whole, start)?;
    let at = (off - start) as usize;
    whole[at..at + data.len()].copy_from_slice(data);
    write_all_at(file, &whole, start)
}
