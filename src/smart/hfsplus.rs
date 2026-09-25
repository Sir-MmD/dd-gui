//! HFS+ and HFSX: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at, be16_at};

/// An HFS+ or HFSX volume header at 1 KiB.
pub(crate) fn detect(head: &[u8]) -> bool {
    (at(head, 1024, b"H+") && be16_at(head, 1026) == 4) || (at(head, 1024, b"HX") && be16_at(head, 1026) == 5)
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("HFS+ and HFSX isn't read yet".into()))
}
