//! LVM2 physical volumes: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// An LVM2 label in one of the first four sectors.
pub(crate) fn detect(head: &[u8]) -> bool {
    (0..4).any(|s| at(head, s * 512, b"LABELONE") && at(head, s * 512 + 24, b"LVM2 001"))
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("LVM2 physical volumes isn't read yet".into()))
}
