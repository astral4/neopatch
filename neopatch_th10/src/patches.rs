//! Static byte patches for th10.exe v1.00a.

use neopatch_core::patches::Patch;

/// "Sleep-path branch nop": NOPs the `jne 0x439527` inside `fcn.00439390`
/// (`CWindowManager::Update`). The branch target contains the game's `Sleep`
/// at `0x0043952B`, so killing the branch keeps the loop from reaching it.
///
/// "frame limiter unconditional skip": flips a post-FPU (x87) `jne` to `jmp` so the loop
/// always lands past the deadline check. Together with the above, this disengages
/// the game's pacer in favor of ours.
pub(crate) const PATCHES: &[Patch] = &[
    Patch::new(
        0x0043_93b7,
        &[0x0f, 0x85, 0x6a, 0x01, 0x00, 0x00],
        &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90],
        "Sleep-path branch nop",
    ),
    Patch::new(
        0x0043_93c5,
        &[0x75, 0x22],
        &[0xeb, 0x22],
        "frame limiter unconditional skip",
    ),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}
