//! Direct reads of game state for 東方紅魔郷.exe v1.02h.

use crate::patches::G_SUPERVISOR;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::replay::HeldKeys;
use neopatch_core::thread::MainToken;
use std::sync::atomic::{AtomicU16, Ordering};

const SUPERVISOR_REQUESTED_STATE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_SUPERVISOR + 0x18c) };
const SUPERVISOR_SETTLED_STATE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_SUPERVISOR + 0x188) };
const STATE_GAME: i32 = 2;
const IS_IN_REPLAY_VA: GameAddr<i32> = unsafe { GameAddr::new(0x0069_bca0 + 0x1c) };
/// The counter behind the game's auto-focus for pad mappings with shoot and focus on the same button (the game's default mapping).
/// While the shared button is held, the counter climbs up to 16; at [`AUTO_FOCUS_THRESHOLD`] and above, `Controller::GetInput` ORs `FOCUS`
/// into the mask it returns even though the player only pressed shoot.
const FOCUS_BUTTON_CONFLICT_COUNTER_VA: GameAddr<u16> = unsafe { GameAddr::new(0x0069_d8f4) };
const AUTO_FOCUS_THRESHOLD: u16 = 8;

const INPUT_SHOOT: u32 = 0x1;
const INPUT_FOCUS: u32 = 0x4;
const INPUT_SKIP: u32 = 0x100;

/// The held-key bitmask most recently returned by the game's own `Controller::GetInput` call.
static OBSERVED_INPUT: AtomicU16 = AtomicU16::new(0);

/// Records one frame's raw hardware input.
pub(crate) fn record_input(input: u16) {
    OBSERVED_INPUT.store(input, Ordering::Relaxed);
}

pub(crate) fn replay_keys(_tok: &MainToken) -> Option<HeldKeys> {
    let in_stage = SUPERVISOR_REQUESTED_STATE_VA.read() == STATE_GAME
        || SUPERVISOR_SETTLED_STATE_VA.read() == STATE_GAME;

    if !in_stage || IS_IN_REPLAY_VA.read() == 0 {
        return None;
    }

    let mut input = u32::from(OBSERVED_INPUT.load(Ordering::Relaxed));
    if input & INPUT_SHOOT != 0 && FOCUS_BUTTON_CONFLICT_COUNTER_VA.read() >= AUTO_FOCUS_THRESHOLD {
        input &= !INPUT_FOCUS;
    }

    Some(HeldKeys {
        shoot: input & INPUT_SHOOT != 0,
        focus: input & INPUT_FOCUS != 0,
        skip: input & INPUT_SKIP != 0,
        ctrl: false,
    })
}
