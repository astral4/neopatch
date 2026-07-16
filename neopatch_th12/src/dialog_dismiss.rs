//! Logic for auto-dismissing th12's window-mode startup dialog.

use neopatch_core::config::{CONFIG, DisplayMode};
use neopatch_core::game_addr::GameAddr;
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const DIALOG_TEMPLATE_ID: usize = 0xcb;
const DIALOG_PROC_VA: usize = 0x0045_18c0;
const MODE_FULLSCREEN: u8 = 0;
const MODE_WINDOWED: u8 = 1;
const DISPLAY_MODE_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_eacd) };
const DIALOG_RET: isize = 6;

const DIALOG_BOX_CALL_VA: usize = 0x0044_f676;
const DIALOG_BOX_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x04, 0x82, 0x49, 0x00];

const DIALOG_PATCHES: &[Patch] = &[Patch::new(
    0x0044_f667,
    &[0x74, 0x13],
    &[0x90, 0x90],
    "force dialog gate open",
)];

iat_hook! {
    REAL_DIALOG_BOX_PARAM_A / real_dialog_box_param_a : "DialogBoxParamA"
        as fn(
            hinst: HMODULE,
            template: *const c_char,
            parent: HWND,
            proc: DLGPROC,
            init_param: LPARAM,
        ) -> isize;
}

unsafe extern "system" fn hook_dialog_box_param_a(
    hinst: HMODULE,
    template: *const c_char,
    parent: HWND,
    proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    let template_id = template.addr();
    let proc_va = proc.map_or(0usize, |f| f as usize);

    if template_id != DIALOG_TEMPLATE_ID || proc_va != DIALOG_PROC_VA {
        info!(
            kind = "dialog_box_param_a_passthrough",
            template = format_args!("{template_id:#x}"),
            proc = format_args!("{proc_va:#x}"),
        );
        return unsafe { real_dialog_box_param_a(hinst, template, parent, proc, init_param) };
    }

    let mode = CONFIG.get().unwrap().display.mode;
    let mode_byte = match mode {
        DisplayMode::Windowed => MODE_WINDOWED,
        DisplayMode::Fullscreen => MODE_FULLSCREEN,
    };
    let mode_byte_prev = DISPLAY_MODE_BYTE.read();
    DISPLAY_MODE_BYTE.write(mode_byte);

    info!(
        kind = "dialog_short_circuited",
        template = format_args!("{template_id:#x}"),
        proc = format_args!("{proc_va:#x}"),
        mode = %mode,
        display_mode_prev = mode_byte_prev,
        display_mode_next = mode_byte,
        retval = DIALOG_RET,
    );
    DIALOG_RET
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_DIALOG_BOX_PARAM_A.install_with_call_site(
            host,
            hook_dialog_box_param_a,
            DIALOG_BOX_CALL_VA,
            &DIALOG_BOX_CALL_BYTES,
        );
        Patch::apply_all(DIALOG_PATCHES);
    }
}
