//! Direct reads of game state for th20.exe v1.00a.

use neopatch_core::MainToken;
use neopatch_core::d3d9::{ReplayMode, set_replay_mode_fn};
use neopatch_core::replay::{InputAddr, ReplayStateLayout, read_replay_mode};
use std::ptr::{read_volatile, with_exposed_provenance};
use std::sync::OnceLock;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;

const INPUT_MGR_PTR_VA: usize = 0x005b_8898;
const INPUT_BITS_PTR_VA: usize = 0x005b_889c;
const KBD_VK_CONTROL_OFFSET: usize = const {
    const KEYBOARD_DEVICE_OFFSET: usize = 0x20;
    const DEVICE_SNAPSHOT_OFFSET: usize = 0x2d0;
    KEYBOARD_DEVICE_OFFSET + DEVICE_SNAPSHOT_OFFSET + VK_CONTROL as usize
};
const REPLAY_MGR_PTR_VA: usize = 0x005c_60fc;
const REPLAY_MGR_MODE_OFFSET: usize = 0x10;
const VIEWER_MODE: i32 = 1;

// The slide is a multiple of 0x10000, so it preserves the alignment of these addresses.
const _: () = {
    assert!(REPLAY_MGR_PTR_VA.is_multiple_of(4));
    assert!(REPLAY_MGR_MODE_OFFSET.is_multiple_of(4));
    assert!(INPUT_MGR_PTR_VA.is_multiple_of(4));
    assert!(INPUT_BITS_PTR_VA.is_multiple_of(4));
};

static REPLAY_STATE: OnceLock<Th20ReplayState> = OnceLock::new();

#[derive(Clone, Copy)]
struct Th20ReplayState {
    layout: ReplayStateLayout,
    // Slide-adjusted address of the `InputInf` manager pointer slot.
    input_mgr_ptr_addr: usize,
}

pub(crate) fn install(slide: usize) {
    let state = Th20ReplayState {
        layout: ReplayStateLayout {
            mgr_ptr_addr: REPLAY_MGR_PTR_VA.wrapping_add(slide),
            mgr_mode_offset: REPLAY_MGR_MODE_OFFSET,
            viewer_mode: VIEWER_MODE,
            input_addr: InputAddr::Indirect(INPUT_BITS_PTR_VA.wrapping_add(slide)),
            input_shoot_bit: 0x1,
            input_focus_bit: 0x8,
            input_skip_bit: 0x200,
        },
        input_mgr_ptr_addr: INPUT_MGR_PTR_VA.wrapping_add(slide),
    };
    state.layout.validate();
    let _ = REPLAY_STATE.set(state);
    set_replay_mode_fn(replay_mode);
}

fn replay_mode(tok: &MainToken) -> ReplayMode {
    let state = *REPLAY_STATE
        .get()
        .expect("state::install must run before the first replay_mode call");
    read_replay_mode(tok, state.layout, || ctrl_held(state.input_mgr_ptr_addr))
}

fn ctrl_held(input_mgr_ptr_addr: usize) -> bool {
    let mgr: *const u8 = unsafe { read_volatile(with_exposed_provenance(input_mgr_ptr_addr)) };
    if mgr.is_null() {
        return false;
    }
    let byte_ptr = mgr.wrapping_add(KBD_VK_CONTROL_OFFSET);
    let byte = unsafe { read_volatile(byte_ptr) };
    byte & 0x80 != 0
}
