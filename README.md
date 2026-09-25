<img src="assets/png/DD-GUI-128.png" width="64" alt="DD-GUI icon">

# DD-GUI

A simple, good-looking front end for `dd`, shipped as one self-contained file.
Flash an image to a USB stick, back up a drive, clone one drive to another, or wipe
one, and see exactly which `dd` command runs.

![Choosing an image and a drive](docs/setup.png)
![Choosing how to copy a drive](docs/copy-mode.png)
![Copying](docs/progress.png)

## What it does

- **Flash images to drives.** ISO and IMG files go through the bundled `dd`.
  Compressed images (`.gz`, `.xz`, `.zst`, `.zip`) are unpacked on the fly.
- **Back up or clone a drive, two ways.** DD-GUI reads the drive first, then asks:
  - **Smart copy** copies only the space in use, e.g. 9 GB of a 64 GB stick. Backups
    become a compressed `.img.gz` that DD-GUI writes back quickly.
  - **Sector by sector** is plain `dd`: every byte, including free space and deleted
    files. It works with any file system or encryption, and the image is as big as the drive.
- **Wipe a drive** with zeros.
- **dd options are picked for you.**
  - Block size.
  - Direct I/O on Linux, so progress follows the drive, not RAM.
  - A final flush.
  - Sparse output for backups.

  Advanced settings let you override the block size, count, skip and seek, and turn on rescue
  mode (`conv=noerror,sync`).
- **Safe by default.**
  - The drive your OS runs from can't be written to, and internal drives stay hidden until you ask.
  - Drives that are too small are flagged.
  - Writing onto the drive that holds the image is refused.
  - Every erase asks first.
  - The chosen drive is checked again right before writing, in case sticks were swapped.
- **One file.** `dd` from [uutils coreutils](https://github.com/uutils/coreutils) (MIT,
  GNU-compatible) is compiled in: `DD-GUI dd if=… of=…` works like `dd` from a terminal.

## Smart copy

DD-GUI reads the partition table (MBR or GPT) and the allocation maps of FAT12/16/32,
exFAT, NTFS and ext2/3/4, and copies only the blocks those file systems use.

Anything it can't read is copied in full: LUKS, LVM, btrfs, XFS, APFS, swap, unknown data.
So are file systems that look unsafe to trust: not cleanly unmounted, with a journal to replay,
or failing a checksum. The partition tables, boot areas and the first and last MiB of the drive
are always copied.

A smart image is a standard gzip file:
- `gunzip` (or 7-Zip, or `zcat backup.img.gz | dd of=/dev/sdX`) gives the full raw disk image,
  with free space as zeros.
- DD-GUI also stores a map of the used space in the gzip header, so writing the image back
  with DD-GUI skips the free space.

## Build

```sh
cargo build --release
./target/release/DD-GUI                  # or: DD-GUI some-image.iso
```

The Linux binary is about 22 MB and only needs libc, fontconfig and freetype. X11,
Wayland and OpenGL are loaded at runtime if they're present, and a software renderer
is built in. `.github/workflows/build.yml` builds for Linux, Windows (the exe with its
icon) and macOS (a `DD-GUI.app`). `packaging/README.md` covers the desktop entry and icons.

## How it works

```
DD-GUI (window, runs as you)
  └─ pkexec DD-GUI copy --ask … | DD-GUI dd …      (with admin rights, only when a drive is involved)
       ├─ reads the drive's layout and waits for your choice (copies from a drive)
       ├─ unmounts the drives
       ├─ smart copy, image unpacking, or the bundled dd
       └─ reports progress back; Cancel works, and it stops if the window goes away
```

| | Drives listed with | Admin rights via | Before writing |
|---|---|---|---|
| Linux | `lsblk` | `pkexec` (polkit) | unmounts the partitions |
| macOS | `diskutil` | the system password prompt | `diskutil unmountDisk`, uses `/dev/rdiskN` |
| Windows | PowerShell `Get-Disk` | the app asks for admin at start | locks and dismounts the volumes |

Without polkit, set `DD_GUI_ELEVATE` to another tool, e.g. `DD_GUI_ELEVATE="sudo -A"` or `doas`.

## Status

- **Linux:** tested end to end on practice drives (loop devices) with real file systems.
  Covered: flashing, compressed images, smart and sector-by-sector backups, smart restores
  and clones, wiping, cancelling, and failures. Results were checked with fsck and file checksums.
- **Windows and macOS:** the code compiles for both, but hasn't been run on real machines yet.
- **Drives stay unmounted.** A backup unmounts the source drive and doesn't mount it again
  afterwards; replug it or mount it yourself.

### A quirk in uutils dd

uutils `dd` routes writes through an unaligned buffer whenever any `conv=` option is
given. With `oflag=direct`, those writes quietly fall back to the page cache. So for drives,
DD-GUI leaves out `conv=fsync` and flushes the drive itself after dd finishes. That's why
the preview reads `dd … && sync`.

## Licenses

- DD-GUI bundles:
  - uutils coreutils (MIT);
  - Inter and JetBrains Mono (SIL Open Font License, see `ui/fonts/`);
  - Lucide icons (ISC, see `ui/icons/LICENSE-lucide.txt`);
  - pure-Rust flate2, miniz_oxide, lzma-rust2, ruzstd and zip for images.
- The UI uses [Slint](https://slint.dev). Its royalty-free license asks for the attribution
  shown in the About dialog. Alternatively you can distribute under GPLv3.
