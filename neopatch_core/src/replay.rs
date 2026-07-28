//! Per-game replay-state probe.

use crate::config::CONFIG;
use crate::pacer::PacingPolicy;
use crate::thread::{MainCell, MainToken};
use std::ptr::{read_volatile, with_exposed_provenance};
use std::sync::OnceLock;

/// State of replay-relevant inputs.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
pub struct HeldKeys {
    /// The game's "shoot" input is held.
    pub shoot: bool,
    /// The game's "focus" input is held.
    pub focus: bool,
    /// The game's "skip" input is held.
    pub skip: bool,
    /// Ctrl is held. Games with an extra fast-forward source should set this; the rest should leave it `false`.
    pub ctrl: bool,
}

/// Probe registered by game-specific crates via [`set_probe`].
static PROBE: OnceLock<fn(&MainToken) -> Option<HeldKeys>> = OnceLock::new();

/// Registers the game-specific probe for replay-relevant held keys; first caller wins. The probe returns `None`
/// while replay playback is inactive. It is read lazily on each `Present`, so any registration before the first `Present` is in time.
pub fn set_probe(f: fn(&MainToken) -> Option<HeldKeys>) {
    let _ = PROBE.set(f);
}

/// Pacing intent during replay playback, rechecked each `Present` to decide whether to switch the pacer policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayMode {
    Normal,
    Skip,
    Slow,
}

/// Classifies the registered probe's observations: focus requests slow-motion and wins; shoot, skip, or Ctrl request fast-forward.
/// Returns [`ReplayMode::Normal`] when no probe is registered, replay playback is inactive, or no relevant key is held.
fn replay_mode(tok: &MainToken) -> ReplayMode {
    let Some(keys) = PROBE.get().and_then(|probe| probe(tok)) else {
        return ReplayMode::Normal;
    };
    if keys.focus {
        ReplayMode::Slow
    } else if keys.shoot || keys.skip || keys.ctrl {
        ReplayMode::Skip
    } else {
        ReplayMode::Normal
    }
}

// `Pacer::apply_policy` resets the pacing deadline, so a transition is surfaced only on mode change.
static MODE: MainCell<ReplayMode> = MainCell::new(ReplayMode::Normal);

/// Re-classifies pacing intent and reports transitions. Returns `Some` only when the mode changed since the last call,
/// with the pacer policy that mode calls for. The transition is latched immediately, so a returned policy must be applied to the pacer.
pub(crate) fn policy_change(tok: &MainToken) -> Option<(ReplayMode, PacingPolicy)> {
    let observed = replay_mode(tok);
    if MODE.get(tok) == observed {
        return None;
    }
    MODE.set(tok, observed);
    let cfg = CONFIG.get().unwrap();
    let policy = match observed {
        ReplayMode::Normal => PacingPolicy::LiveInput {
            target_fps: cfg.framerate.game_fps,
        },
        ReplayMode::Skip => PacingPolicy::InternalCadence {
            target_fps: cfg.framerate.replay_skip_fps,
        },
        ReplayMode::Slow => PacingPolicy::InternalCadence {
            target_fps: cfg.framerate.replay_slow_fps,
        },
    };
    Some((observed, policy))
}

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

    /// Reads the held keys named by this layout. Returns `None` outside the replay menu, when not
    /// in viewer mode, or before the input object is live. `ctrl` is always `false` here; games
    /// with an extra fast-forward source set it on the result.
    #[must_use]
    pub fn read_keys(&self, _tok: &MainToken) -> Option<HeldKeys> {
        let mgr: *const u8 = unsafe { read_volatile(with_exposed_provenance(self.mgr_ptr_addr)) };
        if mgr.is_null() {
            return None;
        }
        // SAFETY: `mode_ptr` is `mgr + mgr_mode_offset`; both are 4-byte aligned.
        let mode_ptr: *const i32 = mgr.wrapping_add(self.mgr_mode_offset).cast();
        let mode = unsafe { read_volatile(mode_ptr) };
        if mode != self.viewer_mode {
            return None;
        }
        let input = read_input_bits(self.input_addr)?;
        Some(HeldKeys {
            shoot: input & self.input_shoot_bit != 0,
            focus: input & self.input_focus_bit != 0,
            skip: input & self.input_skip_bit != 0,
            ctrl: false,
        })
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

/// Reads the held-input bitfield at `input_addr`. Returns `None` if the input object isn't live yet.
fn read_input_bits(input_addr: InputAddr) -> Option<u32> {
    let bits_ptr = match input_addr {
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
