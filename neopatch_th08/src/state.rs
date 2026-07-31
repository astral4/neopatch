//! Direct reads of game state for th08.exe v1.00d.

use crate::patches::G_ENGINE;
use neopatch_core::game_addr::GameAddr;
use neopatch_core::replay::HeldKeys;
use neopatch_core::thread::MainToken;
use std::sync::atomic::{AtomicU16, Ordering};

const ENGINE_SETTLED_STATE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_ENGINE + 0x158) };
const ENGINE_REQUESTED_STATE_VA: GameAddr<i32> = unsafe { GameAddr::new(G_ENGINE + 0x15c) };
const STATE_GAME: i32 = 2;
const G_GAME: usize = 0x0160_f508;
/// Bit 3 is set for the whole replay playback session. (Stage teardown at `0x43be2c` checks it to skip score saving,
/// and `CEngine::OnUpdate`'s input path switches to merging live hardware input into the replayed mask while it's set.)
const GAME_FLAGS_VA: GameAddr<u32> = unsafe { GameAddr::new(G_GAME + 0x3dbac) };
const GAME_FLAG_REPLAY_MASK: u32 = 1 << 3;
/// The counter behind the game's auto-focus for pad mappings with shoot and focus on the same button (the game's default mapping).
/// While the shared button is held, the counter climbs up to 20; at [`AUTO_FOCUS_THRESHOLD`] and above, `GetInput` ORs `FOCUS`
/// into the mask it returns even though the player only pressed shoot.
const FOCUS_BUTTON_CONFLICT_COUNTER_VA: GameAddr<u32> = unsafe { GameAddr::new(0x0164_d51c) };
const AUTO_FOCUS_THRESHOLD: u32 = 10;

const INPUT_SHOOT: u32 = 0x1;
const INPUT_FOCUS: u32 = 0x4;
const INPUT_SKIP: u32 = 0x100;

/// The held-key bitmask most recently returned by the game's own `GetInput` call.
static OBSERVED_INPUT: AtomicU16 = AtomicU16::new(0);

/// Records one frame's raw hardware input.
pub(crate) fn record_input(input: u16) {
    OBSERVED_INPUT.store(input, Ordering::Relaxed);
}

pub(crate) fn replay_keys(_tok: &MainToken) -> Option<HeldKeys> {
    let in_stage = ENGINE_REQUESTED_STATE_VA.read() == STATE_GAME
        || ENGINE_SETTLED_STATE_VA.read() == STATE_GAME;

    if !in_stage || GAME_FLAGS_VA.read() & GAME_FLAG_REPLAY_MASK == 0 {
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
