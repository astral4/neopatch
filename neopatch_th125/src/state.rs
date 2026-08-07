//! Direct reads of game state for th125.exe v1.00a.

use neopatch_core::replay::{HeldKeys, InputAddr, ReplayStateLayout};
use neopatch_core::thread::MainToken;

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004b_68cc,
    mgr_mode_offset: 16,
    viewer_mode: 1,
    input_addr: InputAddr::Direct(0x004d_8da0),
    input_shoot_bit: 0x1,
    input_focus_bit: 0x4,
    input_skip_bit: 0x80,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_keys(tok: &MainToken) -> Option<HeldKeys> {
    REPLAY_STATE.read_keys(tok)
}
