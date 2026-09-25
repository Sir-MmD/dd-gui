//! APFS: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// An APFS container superblock.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 32, b"NXSB")
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("APFS isn't read yet".into()))
}
