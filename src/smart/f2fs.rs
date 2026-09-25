//! F2FS: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, u32_at};

/// An F2FS superblock at 1 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    u32_at(head, 1024) == 0xF2F5_2010
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("F2FS isn't read yet".into()))
}
