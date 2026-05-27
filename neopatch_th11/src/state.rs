//! Direct reads of game state for th11.exe v1.00a.

use neopatch_core::MainToken;
use neopatch_core::d3d9::ReplayMode;
use neopatch_core::replay::{ReplayStateLayout, read_replay_mode};

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004a_8eb8,
    mgr_mode_offset: 16,
    input_addr: 0x004c_92a8,
    viewer_mode: 1,
    input_shoot_bit: 0x1,
    input_focus_bit: 0x8,
    input_skip_bit: 0x200,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_mode(tok: &MainToken) -> ReplayMode {
    read_replay_mode(tok, REPLAY_STATE)
}
