//! ISO 9660: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// An ISO 9660 volume descriptor at 32 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    at(head, 0x8001, b"CD001")
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("ISO 9660 isn't read yet".into()))
}
