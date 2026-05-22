//! Direct reads of game state for th11.exe v1.00a.

use neopatch_core::d3d9::ReplayMode;
use neopatch_core::game_addr::GameAddr;
use std::ptr::read_volatile;

/// `CReplayManager**`; null outside the replay menu.
const REPLAY_MGR_INSTANCE_PTR: GameAddr<*const CReplayManager> =
    unsafe { GameAddr::new(0x004a_8eb8) };
const REPLAY_MODE_VIEWER: i32 = 1;

const INPUT_STATE: GameAddr<u32> = unsafe { GameAddr::new(0x004c_92a8) };
const INPUT_SHOOT: u32 = 0x1;
const INPUT_FOCUS: u32 = 0x8;
const INPUT_SKIP: u32 = 0x200;

// We do this instead of a straightforward `[u8; 16]` + offset
// so `&raw const` produces a properly aligned `*const i32` for free.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CReplayManager {
    _gap: [u8; 16],
    mode: i32,
}

/// Probe registered with [`neopatch_core::d3d9::set_replay_mode_fn`].
/// Reads the replay-manager pointer and input bitfield to classify the current pacing intent.
pub(crate) fn replay_mode() -> ReplayMode {
    let mgr = REPLAY_MGR_INSTANCE_PTR.read();
    if mgr.is_null() {
        return ReplayMode::Normal;
    }
    // `mgr.mode` is pointer-derived, not a fixed game address: we just read the pointer
    // from `REPLAY_MGR_INSTANCE_PTR`. So, it stays as a direct volatile read.
    let mode = unsafe { read_volatile(&raw const (*mgr).mode) };
    if mode != REPLAY_MODE_VIEWER {
        return ReplayMode::Normal;
    }
    let input = INPUT_STATE.read();
    if input & INPUT_FOCUS != 0 {
        ReplayMode::Slow
    } else if input & (INPUT_SHOOT | INPUT_SKIP) != 0 {
        ReplayMode::Skip
    } else {
        ReplayMode::Normal
    }
}
