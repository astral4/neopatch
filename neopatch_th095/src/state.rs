//! Direct reads of game state for th095.exe v1.02a.

use neopatch_core::replay::{HeldKeys, InputAddr, ReplayStateLayout};
use neopatch_core::thread::MainToken;

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004c_4e74,
    mgr_mode_offset: 0,
    viewer_mode: 1,
    input_addr: InputAddr::Direct(0x004b_e218),
    input_shoot_bit: 0x2,
    input_focus_bit: 0x4,
    input_skip_bit: 0x100,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_keys(tok: &MainToken) -> Option<HeldKeys> {
    REPLAY_STATE.read_keys(tok)
}
