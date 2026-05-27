//! Direct reads of game state for th10.exe v1.00a.

use neopatch_core::MainToken;
use neopatch_core::d3d9::ReplayMode;
use neopatch_core::replay::{ReplayStateLayout, read_replay_mode};

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x0047_7838,
    mgr_mode_offset: 16,
    input_addr: 0x0047_4e30,
    viewer_mode: 1,
    input_shoot_bit: 0x1,
    input_focus_bit: 0x4,
    input_skip_bit: 0x100,
};
const _: () = REPLAY_STATE.validate();

pub(crate) fn replay_mode(tok: &MainToken) -> ReplayMode {
    read_replay_mode(tok, REPLAY_STATE)
}
