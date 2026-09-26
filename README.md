<p align="center">
  <img src="assets/png/dd-gui-128.png" width="96" alt="">
</p>

<h1 align="center">DD-GUI</h1>

<p align="center">
  A desktop front end for <code>dd</code>: flash, back up, clone and wipe drives.<br>
  Linux, Windows and macOS. One file, nothing to install.
</p>

<p align="center">
  <a href="https://github.com/Sir-MmD/dd-gui/releases/latest">Download</a>
  &nbsp;·&nbsp;
  <a href="#building">Build from source</a>
</p>

<table>
  <tr>
    <td><img src="docs/setup.png" alt="Main window"></td>
    <td><img src="docs/copy-mode.png" alt="Choosing a copy mode"></td>
  </tr>
  <tr>
    <td><img src="docs/progress.png" alt="Copy in progress"></td>
    <td><img src="docs/done.png" alt="Finished backup"></td>
  </tr>
</table>

## Features

- Write disk images to USB drives and SD cards
- Back up and clone drives, either sector by sector or used space only
- Wipe drives
- `dd` is built in, and the exact command is shown before it runs
- The system drive is protected, and every erase needs confirmation

## Smart copy

Smart copy reads the file system's allocation data and copies only the blocks in use, so a
64 GB stick holding 9 GB of data makes a 9 GB backup. Backups are saved as `.img.zst`:
DD-GUI skips the free space when writing them back, and `zstd -d` turns them into a regular
raw image.

| Supported | |
|---|---|
| File systems | FAT, exFAT, NTFS, ext2/3/4, Btrfs, XFS, F2FS, swap, APFS, HFS+, ISO 9660, UDF |
| Partitioning | MBR, GPT, Apple partition map, BSD disklabel, LVM |

Other file systems, encrypted volumes and volumes that weren't cleanly unmounted are copied
in full.

## Supported images

| | |
|---|---|
| Disk images | ISO, IMG, DMG, VHD, VHDX, VMDK, QCOW2 |
| Compressed | gz, xz, zst, bz2, lz4, lzma, zip |
| Archives | 7z, tar |

## Download

| Platform | File | Requires |
|---|---|---|
| Linux x86_64 | `dd-gui-<version>-linux-x86_64.tar.gz` | glibc 2.28 or newer |
| Windows x86_64 | `dd-gui-<version>-windows-x86_64.zip` | Windows 10 or newer |
| macOS | | [Build from source](#building) |

Writing to a drive needs administrator rights:
- **Linux:** DD-GUI asks through polkit, or through the tool named in `DD_GUI_ELEVATE`, such as `sudo -A`.
- **macOS:** the system password prompt appears.
- **Windows:** DD-GUI asks when it starts.

On Linux, DD-GUI adds itself to the application menu on first start. Set
`DD_GUI_NO_DESKTOP_INTEGRATION=1` to prevent this.

## Building

Rust 1.93 or newer is required. The build scripts install anything else that's missing,
after asking, and put the result in `dist/`.

| Platform | Command | Output |
|---|---|---|
| Linux | `./build.sh` | `dist/dd-gui` |
| macOS | `./build.command` | `dist/DD-GUI.app` |
| Windows | `build.bat` | `dist\dd-gui.exe` |

Options:
- `--check` runs the tests.
- `--clean` rebuilds from scratch.
- `--noconfirm` skips the prompts.
- `--universal` (macOS only) builds for both Apple Silicon and Intel.

## Command line

```
dd-gui [IMAGE]          open the window, optionally with an image selected
dd-gui dd if=… of=…     run the bundled dd
dd-gui drives           list drives as JSON
```

## Credits

- [uutils coreutils](https://github.com/uutils/coreutils) for `dd` (MIT)
- [Zstandard](https://github.com/facebook/zstd) (BSD)
- [Slint](https://slint.dev) for the user interface (Slint Royalty-free License)
- [Inter](https://rsms.me/inter/) and [JetBrains Mono](https://www.jetbrains.com/lp/mono/) (SIL Open Font License)
- [Lucide](https://lucide.dev) icons (ISC)
