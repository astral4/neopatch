//! Direct reads of game state for th10.exe v1.00a.

use neopatch_core::replay::{HeldKeys, InputAddr, ReplayStateLayout};
use neopatch_core::thread::MainToken;

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x0047_7838,
    mgr_mode_offset: 16,
    viewer_mode: 1,
    input_addr: InputAddr::Direct(0x0047_4e30),
    input_shoot_bit: 0x1,
    input_focus_bit: 0x4,
    input_skip_bit: 0x100,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_keys(tok: &MainToken) -> Option<HeldKeys> {
    REPLAY_STATE.read_keys(tok)
}
