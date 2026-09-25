//! btrfs: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// A btrfs superblock at 64 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0x10040, b"_BHRfS_M")
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("btrfs isn't read yet".into()))
}
