//! Direct reads of game state for th18.exe v1.00a.

use neopatch_core::iat_hook;
use neopatch_core::replay::{HeldKeys, InputAddr, ReplayStateLayout, set_probe};
use neopatch_core::thread::MainToken;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;

const REPLAY_STATE: ReplayStateLayout = ReplayStateLayout {
    mgr_ptr_addr: 0x004c_f418,
    mgr_mode_offset: 12,
    viewer_mode: 1,
    input_addr: InputAddr::Direct(0x004c_a210),
    input_shoot_bit: 0x1,
    input_focus_bit: 0x8,
    input_skip_bit: 0x200,
};
const _: () = REPLAY_STATE.validate();

iat_hook! {
    REAL_GET_KEYBOARD_STATE / real_get_keyboard_state : "GetKeyboardState"
        as fn(lp_key_state: *mut u8) -> i32;
}

static CTRL_HELD: AtomicBool = AtomicBool::new(false);

/// IAT-hooks `GetKeyboardState` against `host`'s import table and installs the replay key probe.
///
/// # Safety
/// `host` must be a loaded module handle.
pub(crate) unsafe fn install(host: HMODULE) {
    unsafe { REAL_GET_KEYBOARD_STATE.install(host, hook_get_keyboard_state) };
    set_probe(replay_keys);
}

fn replay_keys(tok: &MainToken) -> Option<HeldKeys> {
    let keys = REPLAY_STATE.read_keys(tok)?;
    Some(HeldKeys {
        ctrl: CTRL_HELD.load(Ordering::Relaxed),
        ..keys
    })
}

unsafe extern "system" fn hook_get_keyboard_state(lp_key_state: *mut u8) -> i32 {
    let ok = unsafe { real_get_keyboard_state(lp_key_state) };
    if ok != 0 && !lp_key_state.is_null() {
        let byte = unsafe { *lp_key_state.add(usize::from(VK_CONTROL)) };
        CTRL_HELD.store(byte & 0x80 != 0, Ordering::Relaxed);
    }
    ok
}
