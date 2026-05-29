//! Direct reads of game state for th16.exe v1.00a.

use neopatch_core::MainToken;
use neopatch_core::d3d9::ReplayMode;
use neopatch_core::replay::{ReplayStateLayout, read_replay_mode};

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004a_6f08,
    mgr_mode_offset: 12,
    input_addr: 0x004a_50b0,
    viewer_mode: 1,
    input_shoot_bit: 0x1,
    input_focus_bit: 0x8,
    input_skip_bit: 0x200,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_mode(tok: &MainToken) -> ReplayMode {
    read_replay_mode(tok, REPLAY_STATE)
}
