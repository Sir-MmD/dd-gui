//! UDF: recognised, and copied in full until its allocation map is read.

use super::util::{Bad, Part, Res, Usage, at};

/// A UDF volume recognition sequence (NSR02 or NSR03) from 32 KiB on.
pub(crate) fn detect(head: &[u8]) -> bool {
    (0..8).any(|k| at(head, 0x8001 + k * 2048, b"NSR02") || at(head, 0x8001 + k * 2048, b"NSR03"))
}

pub(crate) fn analyze(_p: &mut Part, _head: &[u8], _label: &mut Option<String>) -> Res<Usage> {
    Err(Bad("UDF isn't read yet".into()))
}
