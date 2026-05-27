//! Per-game replay-state probe.
//!
//! Each game-specific crate declares a `ReplayStateLayout` with the values for that game.

use crate::d3d9::ReplayMode;
use crate::thread::MainToken;
use std::ptr::{read_volatile, with_exposed_provenance};

/// Layout of a game's replay-state globals.
/// All addresses are absolute and must be 4-byte aligned.
#[derive(Clone, Copy)]
pub struct ReplayStateLayout {
    /// Address of the `CReplayManager**`. This is a pointer slot that holds
    /// the address of the manager instance, or null outside the replay menu.
    pub mgr_ptr_addr: usize,
    /// Byte offset of the `mode: i32` field within the manager instance.
    pub mgr_mode_offset: usize,
    /// Address of the game's input bitfield (u32).
    pub input_addr: usize,
    /// Mode value indicating "viewer" (user is in replay playback).
    pub viewer_mode: i32,
    /// Bit set when "shoot" input is held.
    pub input_shoot_bit: u32,
    /// Bit set when "focus" input is held.
    pub input_focus_bit: u32,
    /// Bit set when "skip" input is held.
    pub input_skip_bit: u32,
}

impl ReplayStateLayout {
    /// Checks that this layout satisfies invariants for correctness. This should be invoked
    /// from a `const _: () = ...` block at each site declaring a `ReplayStateLayout`.
    ///
    /// # Panics
    /// Panics if `mgr_ptr_addr`, `mgr_mode_offset`, or `input_addr` isn't a multiple of 4.
    pub const fn validate(&self) {
        assert!(
            self.mgr_ptr_addr.is_multiple_of(4),
            "ReplayStateLayout::mgr_ptr_addr must be 4-byte aligned",
        );
        assert!(
            self.mgr_mode_offset.is_multiple_of(4),
            "ReplayStateLayout::mgr_mode_offset must be 4-byte aligned",
        );
        assert!(
            self.input_addr.is_multiple_of(4),
            "ReplayStateLayout::input_addr must be 4-byte aligned",
        );
    }
}

/// Classifies the current pacing intent for a game with replay-speed control.
/// Returns `Normal` outside the replay menu, when not in viewer mode,
/// or when no relevant input is held.
#[must_use]
pub fn read_replay_mode(_tok: &MainToken, layout: ReplayStateLayout) -> ReplayMode {
    let mgr: *const u8 =
        unsafe { read_volatile(with_exposed_provenance::<*const u8>(layout.mgr_ptr_addr)) };
    if mgr.is_null() {
        return ReplayMode::Normal;
    }
    // SAFETY: `mode_addr` is `mgr + mgr_mode_offset`; both are 4-byte aligned.
    let mode_addr = mgr.addr().wrapping_add(layout.mgr_mode_offset);
    let mode: i32 = unsafe { read_volatile(with_exposed_provenance::<i32>(mode_addr)) };
    if mode != layout.viewer_mode {
        return ReplayMode::Normal;
    }
    let input: u32 = unsafe { read_volatile(with_exposed_provenance::<u32>(layout.input_addr)) };
    if input & layout.input_focus_bit != 0 {
        ReplayMode::Slow
    } else if input & (layout.input_shoot_bit | layout.input_skip_bit) != 0 {
        ReplayMode::Skip
    } else {
        ReplayMode::Normal
    }
}
