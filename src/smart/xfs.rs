//! XFS: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// An XFS superblock at the start.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0, b"XFSB")
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("XFS isn't read yet".into()))
}
