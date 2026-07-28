//! Direct reads of game state for th17.exe v1.00b.

use neopatch_core::replay::{HeldKeys, InputAddr, ReplayStateLayout};
use neopatch_core::thread::MainToken;

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004b_77d8,
    mgr_mode_offset: 12,
    viewer_mode: 1,
    input_addr: InputAddr::Direct(0x004b_3230),
    input_shoot_bit: 0x1,
    input_focus_bit: 0x8,
    input_skip_bit: 0x200,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_keys(tok: &MainToken) -> Option<HeldKeys> {
    REPLAY_STATE.read_keys(tok)
}
