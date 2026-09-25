//! ISO 9660: the volume is one piece, as long as its primary volume descriptor says. Past it
//! the partition is free, except what a hybrid image's own partition table (in the system
//! area, the first 32 KiB) says lies there: partitions appended to the image.

use super::util::{Bad, Part, Res, Usage, at, be16_at, be32_at, ensure, u16_at, u32_at, u64_at};

/// An ISO 9660 volume descriptor at 32 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0x8001, b"CD001")
}

/// Volume descriptors looked at, at most, for the set terminator.
const MAX_DESCRIPTORS: u64 = 64;

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let pvd = head.get(0x8000..0x8800).ok_or_else(|| Bad("partition too small for ISO 9660".into()))?;
    ensure(pvd[0] == 1 && at(pvd, 1, b"CD001") && pvd[6] == 1, "no primary volume descriptor")?;
    // Both-endian fields: both halves must agree.
    let blocks = u32_at(pvd, 80);
    let block = u16_at(pvd, 128);
    ensure(blocks == be32_at(pvd, 84) && block == be16_at(pvd, 130), "inconsistent volume descriptor")?;
    ensure(matches!(block, 512 | 1024 | 2048), "bad logical block size")?;
    let end = u64::from(blocks) * u64::from(block);
    ensure(end >= 0x8800 && end <= p.size, "volume larger than its partition, or too small")?;
    // The descriptors, 2 KiB each from 32 KiB on, end with a terminator (type 255).
    let mut terminated = false;
    for i in 1..MAX_DESCRIPTORS {
        let pos = 0x8000 + i * 2048;
        let mut d = [0u8; 7];
        if pos + 2048 > end {
            break;
        }
        p.read_at(pos, &mut d)?;
        ensure(at(&d, 1, b"CD001"), "volume descriptor set not terminated")?;
        if d[0] == 255 {
            terminated = true;
            break;
        }
    }
    ensure(terminated, "volume descriptor set not terminated")?;
    let name = String::from_utf8_lossy(&pvd[40..72]).trim_end_matches([' ', '\0']).trim().to_owned();
    *label = (!name.is_empty()).then_some(name);
    let keep = end.max(hybrid_end(head)).min(p.size);
    let mut used = p.ranges();
    used.add(0, keep);
    Ok(Usage { used, end: p.size })
}

/// Where the partitions of a hybrid image end, per the MBR or GPT in its system area (0
/// without one). Partitions appended to an ISO image (EFI boot images, persistence) lie past
/// the volume.
fn hybrid_end(head: &[u8]) -> u64 {
    let mut end = 0u64;
    if at(head, 510, &[0x55, 0xAA]) {
        for i in 0..4 {
            let e = 0x1BE + 16 * i;
            let (start, sectors) = (u64::from(u32_at(head, e + 8)), u64::from(u32_at(head, e + 12)));
            if head[e + 4] != 0 && sectors > 0 {
                end = end.max((start + sectors) * 512);
            }
        }
    }
    // A GPT: its backup header is at the end of what it describes.
    for sector in [512usize, 2048, 4096] {
        if at(head, sector, b"EFI PART") {
            let (alternate, last) = (u64_at(head, sector + 32), u64_at(head, sector + 48));
            let ss = sector as u64;
            end = end.max(alternate.max(last).saturating_add(1).saturating_mul(ss));
        }
    }
    end
}
