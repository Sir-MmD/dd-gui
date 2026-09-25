//! Human-friendly numbers. Sizes use SI units, like drive labels and dd itself.

pub fn bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = "B";
    for next in ["kB", "MB", "GB", "TB", "PB"] {
        if value < 999.5 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{} {unit}", three_digits(value))
}

pub fn speed(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

pub fn duration(secs: f64) -> String {
    if secs > 0.0 && secs < 0.5 {
        return "<1s".to_owned();
    }
    let secs = secs.max(0.0).round() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs / 60 % 60),
    }
}

/// A file system's name as people know it, from the blkid-style names the smart analysis
/// and lsblk use: "vfat" → "FAT", "crypto_LUKS" → "LUKS". Unknown names pass through.
pub fn fs_name(name: &str) -> String {
    let known = match name {
        "vfat" | "msdos" | "fat" => "FAT",
        "exfat" => "exFAT",
        "ntfs" | "ntfs3" => "NTFS",
        "refs" | "ReFS" => "ReFS",
        "btrfs" => "Btrfs",
        "xfs" => "XFS",
        "f2fs" => "F2FS",
        "LVM2_member" | "lvm2" => "LVM",
        "swap" => "swap",
        "swsuspend" => "hibernation",
        "iso9660" => "ISO 9660",
        "udf" => "UDF",
        "apfs" => "APFS",
        "hfsplus" => "HFS+",
        "hfs" => "HFS",
        "crypto_LUKS" => "LUKS",
        "BitLocker" => "BitLocker",
        "squashfs" | "squashfs3" => "SquashFS",
        "linux_raid_member" => "Linux RAID",
        "zfs_member" => "ZFS",
        "jfs" => "JFS",
        "reiserfs" => "ReiserFS",
        "reiser4" => "Reiser4",
        "nilfs2" => "NILFS2",
        "erofs" => "EROFS",
        "cramfs" => "cramfs",
        "romfs" => "romfs",
        "minix" => "Minix",
        "ufs" => "UFS",
        "vxfs" => "VxFS",
        "gfs" => "GFS",
        "gfs2" => "GFS2",
        "ocfs2" => "OCFS2",
        "VMFS" | "vmfs" | "VMFS_volume_member" => "VMFS",
        "bcache" => "bcache",
        "bcachefs" => "bcachefs",
        "oracleasm" => "Oracle ASM",
        "hpfs" => "HPFS",
        "ubifs" => "UBIFS",
        "ubi" => "UBI",
        "bfs" => "BFS",
        "zonefs" => "zonefs",
        "LVM1_member" => "LVM1",
        "DM_integrity" => "dm-integrity",
        "DM_verity_hash" => "dm-verity",
        "DM_snapshot_cow" => "LVM snapshot",
        "vdo" => "VDO",
        "stratis" => "Stratis",
        "ceph_bluestore" => "Ceph BlueStore",
        _ => return name.to_owned(),
    };
    known.to_owned()
}

/// dd's notation: 4194304 → "4M".
pub fn block_size(n: u64) -> String {
    for (unit, size) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if n >= size && n.is_multiple_of(size) {
            return format!("{}{unit}", n / size);
        }
    }
    n.to_string()
}

fn three_digits(v: f64) -> String {
    if v < 9.995 {
        format!("{v:.2}")
    } else if v < 99.95 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(5_910_000_000), "5.91 GB");
        assert_eq!(bytes(31_914_983_424), "31.9 GB");
        assert_eq!(bytes(256_060_514_304), "256 GB");
        assert_eq!(bytes(999_999), "1.00 MB");
        assert_eq!(block_size(4 << 20), "4M");
        assert_eq!(block_size(512 << 10), "512K");
        assert_eq!(block_size(1000), "1000");
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(78.0), "1m 18s");
        assert_eq!(duration(3900.0), "1h 05m");
    }

    #[test]
    fn file_systems() {
        assert_eq!(fs_name("vfat"), "FAT");
        assert_eq!(fs_name("crypto_LUKS"), "LUKS");
        assert_eq!(fs_name("hfsplus"), "HFS+");
        assert_eq!(fs_name("ext4"), "ext4");
        assert_eq!(fs_name("something_new"), "something_new");
    }
}
