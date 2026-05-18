//! Direct reads of game state for th15.exe v1.00b.

use crate::game_addr::GameAddr;
use std::ffi::c_void;
use std::ptr::read_volatile;
use tracing::info;

/// `CReplayManager**`; null outside the replay menu.
const REPLAY_MGR_INSTANCE_PTR: GameAddr<*const CReplayManager> =
    unsafe { GameAddr::new(0x004e_9bc4) };
const REPLAY_MODE_VIEWER: i32 = 1;

const INPUT_STATE: GameAddr<u32> = unsafe { GameAddr::new(0x004e_6d10) };
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

/// One log line per populated slot in th15's anim-preload manager.
/// The manager is at `*[0x503c18]` and its 30-slot table is at offset `0x187f4d8`.
/// Each slot pointing at a 316-byte anim object whose `+0x128`
/// is the spin counter polled by `preloadAnim`.
///
/// # Safety
/// May only run when each slot in the anim table is either null or points to a
/// live anim object; a non-null but stale slot AVs at the `[+0x128]` read.
pub(crate) unsafe fn log_anim_counters() {
    // Pointer to the anim-preload manager.
    const ANIM_CTX_PTR: GameAddr<*const c_void> = unsafe { GameAddr::new(0x0050_3C18) };
    const SLOT_TABLE_OFFSET: usize = 0x0187_F4D8;
    const COUNTER_OFFSET: usize = 0x128;
    const SLOT_COUNT: usize = 30;

    let ctx = ANIM_CTX_PTR.read();
    if ctx.is_null() {
        info!("  anim_ctx ([0x503c18]) = NULL, manager not initialized yet",);
        return;
    }

    let mut populated = 0u32;
    for idx in 0..SLOT_COUNT {
        let slot_ptr = ctx
            .wrapping_byte_add(SLOT_TABLE_OFFSET)
            .wrapping_byte_add(idx * 4)
            .cast::<*const c_void>();
        let anim = unsafe { read_volatile(slot_ptr) };
        if anim.is_null() {
            continue;
        }
        let counter_ptr = anim.wrapping_byte_add(COUNTER_OFFSET).cast::<u32>();
        let counter = unsafe { read_volatile(counter_ptr) };
        info!(
            "  anim[{idx:2}] = {:#010x}  [+0x128] = {counter}",
            anim as usize,
        );
        populated += 1;
    }
    if populated == 0 {
        info!("  anim_ctx = {:#010x}, all 30 slots empty", ctx as usize);
    }
}
