//! Direct reads of game state for th15.exe v1.00b.

use std::ptr::{read_volatile, with_exposed_provenance};
use tracing::info;

/// `CReplayManager**`; null outside the replay menu.
const REPLAY_MGR_INSTANCE_PTR_ADDR: usize = 0x004e_9bc4;
const REPLAY_MODE_VIEWER: i32 = 1;

const INPUT_STATE_ADDR: usize = 0x004e_6d10;
const INPUT_SHOOT: u32 = 0x1;
const INPUT_FOCUS: u32 = 0x8;
const INPUT_SKIP: u32 = 0x200;

// We do this instead of a straightforward `[u8; 12]` + offset
// so `&raw const` produces a properly aligned `*const i32` for free.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CReplayManager {
    _gap: [u8; 12],
    mode: i32,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayMode {
    Normal = 0,
    Skip = 1,
    Slow = 2,
}

#[inline]
pub(crate) unsafe fn replay_mode() -> ReplayMode {
    unsafe {
        let mgr = read_volatile(with_exposed_provenance::<*const CReplayManager>(
            REPLAY_MGR_INSTANCE_PTR_ADDR,
        ));
        if mgr.is_null() {
            return ReplayMode::Normal;
        }
        let mode = read_volatile(&raw const (*mgr).mode);
        if mode != REPLAY_MODE_VIEWER {
            return ReplayMode::Normal;
        }
        let input = read_volatile(with_exposed_provenance::<u32>(INPUT_STATE_ADDR));
        if input & INPUT_FOCUS != 0 {
            ReplayMode::Slow
        } else if input & (INPUT_SHOOT | INPUT_SKIP) != 0 {
            ReplayMode::Skip
        } else {
            ReplayMode::Normal
        }
    }
}

/// One log line per populated slot in th15's anim-preload manager.
/// The manager is at `*[0x503c18]` and its 30-slot table is at offset `0x187f4d8`.
/// Each slot pointing at a 316-byte anim object whose `+0x128`
/// is the spin counter polled by `preloadAnim`.
pub(crate) unsafe fn log_anim_counters() {
    const CTX_PTR_ADDR: usize = 0x0050_3C18;
    const SLOT_TABLE_OFFSET: usize = 0x0187_F4D8;
    const COUNTER_OFFSET: usize = 0x128;
    const SLOT_COUNT: usize = 30;

    // SAFETY: `0x00503c18` is in th15.exe's `.data`, mapped readable
    // for the process lifetime. All three reads are `u32`-aligned.
    let ctx = unsafe { read_volatile(with_exposed_provenance::<u32>(CTX_PTR_ADDR)) };
    if ctx == 0 {
        info!("  anim_ctx ([0x503c18]) = NULL, manager not initialized yet",);
        return;
    }

    let mut populated = 0u32;
    for idx in 0..SLOT_COUNT {
        let slot_addr = (ctx as usize)
            .wrapping_add(SLOT_TABLE_OFFSET)
            .wrapping_add(idx * 4);
        let anim = unsafe { read_volatile(with_exposed_provenance::<u32>(slot_addr)) };
        if anim == 0 {
            continue;
        }
        let counter_addr = (anim as usize).wrapping_add(COUNTER_OFFSET);
        let counter = unsafe { read_volatile(with_exposed_provenance::<u32>(counter_addr)) };
        info!("  anim[{idx:2}] = {anim:#010x}  [+0x128] = {counter}",);
        populated += 1;
    }
    if populated == 0 {
        info!("  anim_ctx = {ctx:#010x}, all 30 slots empty",);
    }
}
