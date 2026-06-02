//! Per-game replay-state probe.
//!
//! Each game-specific crate declares a `ReplayStateLayout` with the values for that game.

use crate::d3d9::ReplayMode;
use crate::pacer::{qpc, read_qpc_freq};
use crate::thread::{MainCell, MainToken};
use std::ptr::{read_volatile, with_exposed_provenance};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};

/// Layout of a game's replay-state globals.
/// All addresses are absolute and must be 4-byte aligned.
#[derive(Clone, Copy)]
pub struct ReplayStateLayout {
    /// Address of the `CReplayManager**`. This is a pointer slot that holds
    /// the address of the manager instance, or null outside the replay menu.
    pub mgr_ptr_addr: usize,
    /// Byte offset of the `mode: i32` field within the manager instance.
    pub mgr_mode_offset: usize,
    /// Address of the game's input bitfield (u32), or, when `input_indirect`, of a pointer to it.
    pub input_addr: usize,
    /// If `true`, `input_addr` holds a pointer to the input object and the bitfield
    /// is its first dword; if `false`, `input_addr` is the bitfield itself.
    pub input_indirect: bool,
    /// Mode value indicating "viewer" (user is in replay playback).
    pub viewer_mode: i32,
    /// Bit set when "shoot" input is held.
    pub input_shoot_bit: u32,
    /// Bit set when "focus" input is held.
    pub input_focus_bit: u32,
    /// Bit set when "skip" input is held.
    pub input_skip_bit: u32,
    /// Also enter `Skip` while `VK_CONTROL` is held. A held modifier (e.g. Ctrl) latches to
    /// a continuous signal under WineD3D + X11, whereas a held non-modifier (e.g. Z) arrives as
    /// discrete pulses and fast-forwards erratically. Set this for games that don't already
    /// wire Ctrl into a `skip` bit, to restore reliable hold-to-fast-forward behavior.
    pub skip_on_ctrl: bool,
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
pub fn read_replay_mode(tok: &MainToken, layout: ReplayStateLayout) -> ReplayMode {
    let mgr: *const u8 = unsafe { read_volatile(with_exposed_provenance(layout.mgr_ptr_addr)) };
    if mgr.is_null() {
        return ReplayMode::Normal;
    }
    // SAFETY: `mode_addr` is `mgr + mgr_mode_offset`; both are 4-byte aligned.
    let mode_addr = mgr.addr().wrapping_add(layout.mgr_mode_offset);
    let mode: i32 = unsafe { read_volatile(with_exposed_provenance(mode_addr)) };
    if mode != layout.viewer_mode {
        return ReplayMode::Normal;
    }
    let Some(input) = read_input_bits(layout) else {
        return ReplayMode::Normal;
    };
    if input & layout.input_focus_bit != 0 {
        ReplayMode::Slow
    } else if input & (layout.input_shoot_bit | layout.input_skip_bit) != 0
        || (layout.skip_on_ctrl && ctrl_held(tok))
    {
        ReplayMode::Skip
    } else {
        ReplayMode::Normal
    }
}

/// Reads the held-input bitfield named by `layout`. Returns `None` if the input object isn't live yet.
fn read_input_bits(layout: ReplayStateLayout) -> Option<u32> {
    let bits_addr = if layout.input_indirect {
        let obj: *const u8 = unsafe { read_volatile(with_exposed_provenance(layout.input_addr)) };
        if obj.is_null() {
            return None;
        }
        obj.addr()
    } else {
        layout.input_addr
    };
    let bits: u32 = unsafe { read_volatile(with_exposed_provenance(bits_addr)) };
    Some(bits)
}

// We throttle `GetAsyncKeyState` polling because a high poll rate races the game's own
// keyboard delivery under WineD3D + X11 and drops its input events.
const CTRL_POLL_THROTTLE_MS: i64 = 25;

#[derive(Clone, Copy)]
struct CtrlPoll {
    last_qpc: i64,
    held: bool,
    freq: i64,
}

static CTRL_POLL: MainCell<CtrlPoll> = MainCell::new(CtrlPoll {
    last_qpc: 0,
    held: false,
    freq: 0,
});

fn ctrl_held(tok: &MainToken) -> bool {
    let mut s = CTRL_POLL.get(tok);
    if s.freq == 0 {
        s.freq = read_qpc_freq();
    }
    let now = qpc();
    let throttled = s.last_qpc != 0
        && s.freq != 0
        && now >= s.last_qpc
        && (now - s.last_qpc) * 1_000 / s.freq < CTRL_POLL_THROTTLE_MS;
    if !throttled {
        s.held = unsafe { GetAsyncKeyState(i32::from(VK_CONTROL)).cast_unsigned() & 0x8000 != 0 };
        s.last_qpc = now;
    }
    CTRL_POLL.set(tok, s);
    s.held
}
