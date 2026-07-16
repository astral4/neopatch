//! Per-game replay-state probe.
//!
//! Each game-specific crate declares a [`ReplayStateLayout`] with the values for that game.

use crate::d3d9::ReplayMode;
use crate::thread::MainToken;
use std::ptr::{read_volatile, with_exposed_provenance};

/// Layout of a game's replay-state globals. All addresses are absolute and must be 4-byte aligned.
#[derive(Clone, Copy)]
pub struct ReplayStateLayout {
    /// Address of the `CReplayManager**`. This is a pointer slot that holds the address of the manager instance,
    /// or null outside the replay menu.
    pub mgr_ptr_addr: usize,
    /// Byte offset of the `mode: i32` field within the manager instance.
    pub mgr_mode_offset: usize,
    /// Mode value indicating "viewer" (user is in replay playback).
    pub viewer_mode: i32,
    /// Address for the game's input bitfield (u32).
    pub input_addr: InputAddr,
    /// Bit set when "shoot" input is held.
    pub input_shoot_bit: u32,
    /// Bit set when "focus" input is held.
    pub input_focus_bit: u32,
    /// Bit set when "skip" input is held.
    pub input_skip_bit: u32,
}

impl ReplayStateLayout {
    /// Checks that this layout satisfies invariants for correctness.
    /// This should be invoked from a `const _: () = ...` block at each site declaring a `ReplayStateLayout`.
    ///
    /// # Panics
    /// Panics if:
    /// - `mgr_ptr_addr` or `input_addr` is 0
    /// - `mgr_ptr_addr`, `mgr_mode_offset`, or `input_addr` isn't a multiple of 4
    pub const fn validate(&self) {
        assert!(
            self.mgr_ptr_addr != 0,
            "ReplayStateLayout::mgr_ptr_addr must be nonzero",
        );
        assert!(
            self.mgr_ptr_addr.is_multiple_of(4),
            "ReplayStateLayout::mgr_ptr_addr must be 4-byte aligned",
        );
        assert!(
            self.mgr_mode_offset.is_multiple_of(4),
            "ReplayStateLayout::mgr_mode_offset must be 4-byte aligned",
        );
        let input_addr = match self.input_addr {
            InputAddr::Direct(addr) | InputAddr::Indirect(addr) => addr,
        };
        assert!(
            input_addr != 0,
            "ReplayStateLayout::input_addr must be nonzero",
        );
        assert!(
            input_addr.is_multiple_of(4),
            "ReplayStateLayout::input_addr must be 4-byte aligned",
        );
    }
}

/// Input bitfield addressing method.
#[derive(Clone, Copy)]
pub enum InputAddr {
    /// Address of the game's input bitfield.
    Direct(usize),
    /// Address of a pointer to the game's input bitfield.
    Indirect(usize),
}

/// Classifies the current pacing intent for a game with replay-speed control.
/// Returns `Normal` outside the replay menu, when not in viewer mode, or when no relevant input is held.
#[must_use]
pub fn read_replay_mode(
    _tok: &MainToken,
    layout: ReplayStateLayout,
    skip_held: impl FnOnce() -> bool,
) -> ReplayMode {
    let mgr: *const u8 = unsafe { read_volatile(with_exposed_provenance(layout.mgr_ptr_addr)) };
    if mgr.is_null() {
        return ReplayMode::Normal;
    }
    // SAFETY: `mode_ptr` is `mgr + mgr_mode_offset`; both are 4-byte aligned.
    let mode_ptr: *const i32 = mgr.wrapping_add(layout.mgr_mode_offset).cast();
    let mode = unsafe { read_volatile(mode_ptr) };
    if mode != layout.viewer_mode {
        return ReplayMode::Normal;
    }
    let Some(input) = read_input_bits(layout) else {
        return ReplayMode::Normal;
    };
    if input & layout.input_focus_bit != 0 {
        ReplayMode::Slow
    } else if input & (layout.input_shoot_bit | layout.input_skip_bit) != 0 || skip_held() {
        ReplayMode::Skip
    } else {
        ReplayMode::Normal
    }
}

/// Reads the held-input bitfield named by `layout`. Returns `None` if the input object isn't live yet.
fn read_input_bits(layout: ReplayStateLayout) -> Option<u32> {
    let bits_ptr = match layout.input_addr {
        InputAddr::Direct(addr) => with_exposed_provenance(addr),
        InputAddr::Indirect(addr) => {
            let obj: *const u8 = unsafe { read_volatile(with_exposed_provenance(addr)) };
            if obj.is_null() {
                return None;
            }
            obj.cast()
        }
    };
    let bits = unsafe { read_volatile(bits_ptr) };
    Some(bits)
}
