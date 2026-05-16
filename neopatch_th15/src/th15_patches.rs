//! Static byte patches and hooks for th15.exe v1.00b.

use crate::patches::{Patch, patch_bytes_verified};

/// "UpdateFast skip": unconditional `jmp +0x4A` past the game's `Sleep`,
/// spin, and deadline-advance. Without it, the game's own pacer
/// holds the inter-Present interval at >=33ms in slow replay.
///
/// "fast input latency #1/#2": flip the cond jumps to `EB`, forcing the input preamble
/// to "fast" mode. OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skip the game's own replay-speed control
/// so it doesn't fight our pacer.
///
/// See `dialog_dismiss.rs` for dialog-flow byte patches.
pub(crate) const PATCHES: &[Patch] = &[
    Patch {
        addr: 0x0047_27de,
        bytes: &[0xeb, 0x4a],
        name: "UpdateFast skip",
    },
    Patch {
        addr: 0x0047_1a86,
        bytes: &[0xeb],
        name: "fast input latency #1",
    },
    Patch {
        addr: 0x0047_1a9b,
        bytes: &[0xeb],
        name: "fast input latency #2",
    },
    Patch {
        addr: 0x0045_ced2,
        bytes: &[0xeb, 0x1d],
        name: "replay speed control skip",
    },
];

pub(crate) unsafe fn apply_basic() {
    unsafe {
        for patch in PATCHES {
            patch_bytes_verified(patch.addr, patch.bytes, patch.name);
        }
    }
}
