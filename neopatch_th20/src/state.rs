//! Direct reads of game state for th20.exe v1.00a.

use neopatch_core::MainToken;
use neopatch_core::d3d9::{ReplayMode, set_replay_mode_fn};
use neopatch_core::replay::{InputAddr, ReplayStateLayout, read_replay_mode};
use std::sync::OnceLock;

const MANAGER_PTR_VA: usize = 0x005c_60fc;
const MANAGER_MODE_OFFSET: usize = 0x10;
const VIEWER_MODE: i32 = 1;
const INPUT_PTR_VA: usize = 0x005b_889c;

// The slide is a multiple of 0x10000, so it preserves the alignment of these addresses.
const _: () = {
    assert!(MANAGER_PTR_VA.is_multiple_of(4));
    assert!(MANAGER_MODE_OFFSET.is_multiple_of(4));
    assert!(INPUT_PTR_VA.is_multiple_of(4));
};

static REPLAY_STATE: OnceLock<ReplayStateLayout> = OnceLock::new();

pub(crate) fn install(slide: usize) {
    let layout = ReplayStateLayout {
        mgr_ptr_addr: MANAGER_PTR_VA.wrapping_add(slide),
        mgr_mode_offset: MANAGER_MODE_OFFSET,
        viewer_mode: VIEWER_MODE,
        input_addr: InputAddr::Indirect(INPUT_PTR_VA.wrapping_add(slide)),
        input_shoot_bit: 0x1,
        input_focus_bit: 0x8,
        input_skip_bit: 0x200,
        skip_on_ctrl: true,
    };
    layout.validate();
    let _ = REPLAY_STATE.set(layout);
    set_replay_mode_fn(replay_mode);
}

fn replay_mode(tok: &MainToken) -> ReplayMode {
    let layout = *REPLAY_STATE
        .get()
        .expect("state::install must run before the first replay_mode call");
    read_replay_mode(tok, layout)
}
