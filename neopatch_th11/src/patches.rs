//! Static byte patches for th11.exe v1.00a.

use neopatch_core::patches::Patch;

/// "UpdateFast skip": flips `jne 0x44645e` to `jmp +0x43`, landing past the `Sleep(1)`
/// at `0x00446458` and the FPU (x87) catch-up loop (`0x0044645e..0x00446497`) inside
/// `CWindowManager::UpdateFast`. The catch-up loop iterates the game step without rendering
/// when behind schedule. We let the D3D9 `Present` pacer in core be the sole timing source,
/// so neither the yield nor the multi-step matters.
///
/// "fast input latency #1/#2": flips two cond jumps so the per-frame driver dispatch
/// at `0x004458a7` is always reached. Skips the alternative `fcn.00446080` ("slow")
/// and `fcn.00446650` ("normal") paths in favor of `fcn.00446420` (`CWindowManager::UpdateFast`).
/// OILP also does this under "Force fast input latency mode."
///
/// "replay speed control skip": skips the game's own Ctrl-key fast-forward
/// branch at `0x00436d5f`. Without it the game's internal speed control fights
/// our pacer's replay-skip / replay-slow modes (see `state::replay_mode`).
pub(crate) const PATCHES: &[Patch] = &[
    Patch::new(0x0044_6454, &[0x75, 0x08], &[0xeb, 0x43], "UpdateFast skip"),
    Patch::new(0x0044_5877, &[0x74], &[0xeb], "fast input latency #1"),
    Patch::new(0x0044_588b, &[0x75], &[0xeb], "fast input latency #2"),
    Patch::new(0x0043_6d5f, &[0x74], &[0xeb], "replay speed control skip"),
];

pub(crate) unsafe fn apply_basic() {
    unsafe { Patch::apply_all(PATCHES) };
}
