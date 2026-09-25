//! Linux swap: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// A swap signature at the end of the first page (4 to 64 KiB pages).
pub(crate) fn detect(head: &[u8]) -> bool {
    [4096, 8192, 16384, 32768, 65536].iter().any(|&p| at(head, p - 10, b"SWAPSPACE2") || at(head, p - 10, b"SWAP-SPACE"))
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("Linux swap isn't read yet".into()))
}
