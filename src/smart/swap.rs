//! Linux swap: only its first page matters (the header with the label, the UUID and the list
//! of bad pages). What's swapped out is worthless once the system that wrote it is gone, so
//! the rest is free. Unless the area holds a hibernation image, which is copied in full.

use super::util::{Bad, Part, Res, Usage, at, be32_at, ensure, u32_at};

/// Page sizes the header comes in: its signature ends the first page.
const PAGES: [usize; 5] = [4096, 8192, 16384, 32768, 65536];
/// Swap signatures: version 1, and the obsolete version 0.
const SWAP: [&[u8]; 2] = [b"SWAPSPACE2", b"SWAP-SPACE"];
/// Signatures hibernation writes over the swap signature (swsusp, uswsusp, TuxOnIce).
const SUSPEND: [&[u8]; 4] = [b"S1SUSPEND", b"S2SUSPEND", b"ULSUSPEND", b"LINHIB0001"];
/// TuxOnIce's own header, at the very start.
const TUXONICE: &[u8] = b"\xed\xc3\x02\xe9\x98\x56\xe5\x0c";

/// A swap signature at the end of the first page (4 to 64 KiB pages), or a hibernation image
/// that took its place.
pub(crate) fn detect(head: &[u8]) -> bool {
    signature(head).is_some() || at(head, 0, TUXONICE)
}

/// The page size, and the signature at its end.
fn signature(head: &[u8]) -> Option<(usize, &'static [u8])> {
    PAGES.iter().find_map(|&page| SWAP.iter().chain(&SUSPEND).find(|m| at(head, page - 10, m)).map(|m| (page, *m)))
}

pub(crate) fn analyze(p: &mut Part, head: &[u8], label: &mut Option<String>) -> Res<Usage> {
    let Some((page, magic)) = signature(head) else {
        return Err(Bad("holds a hibernation image (TuxOnIce)".into()));
    };
    ensure(!SUSPEND.contains(&magic) && !at(head, 0, TUXONICE), "holds a hibernation image")?;
    let page = page as u64;
    ensure(page <= p.size, "partition smaller than a swap header")?;
    if magic == b"SWAPSPACE2" {
        // Fields in the byte order of the system that wrote them.
        let big = match (u32_at(head, 1024), be32_at(head, 1024)) {
            (1, _) => false,
            (_, 1) => true,
            _ => return Err(Bad("unknown swap header version".into())),
        };
        let field = |at| {
            if big { be32_at(head, at) } else { u32_at(head, at) }
        };
        let last = u64::from(field(1028));
        let bad = u64::from(field(1032));
        // Checked the way the kernel does before it swaps to it.
        ensure(bad <= (page - 1024 - 512 - 10) / 4, "too many bad pages")?;
        let area = last.checked_add(1).and_then(|n| n.checked_mul(page));
        ensure(last > 0 && area.is_some_and(|len| len <= p.size), "swap area larger than its partition")?;
        for i in 0..bad as usize {
            let bad_page = u64::from(field(1536 + 4 * i));
            ensure(bad_page > 0 && bad_page <= last, "bad page outside the swap area")?;
        }
        let name = &head[1052..1068];
        let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(16)]).trim().to_owned();
        *label = (!name.is_empty()).then_some(name);
    }
    let mut used = p.ranges();
    used.add(0, page);
    Ok(Usage { used, end: p.size })
}
