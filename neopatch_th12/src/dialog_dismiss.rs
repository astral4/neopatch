//! Logic for auto-dismissing th12's window-mode startup dialog.
//!
//! `main` at `0x0044f660..0x0044f67c` tests an "Alt held at launch" stack-local bit before
//! falling through to `DialogBoxParamA` at `0x0044f676` (template `0xCB`, proc at `0x004518c0`).
//! We NOP the `je` at `0x0044f667` so the call site is reached unconditionally, then IAT-hook
//! `DialogBoxParamA` to short-circuit without creating any window.
//!
//! Like th11's dialog, th12's dialog proc unconditionally calls `EndDialog(hwnd, 6)`
//! on the IDOK branch. The selection is a side-effect write in `WM_COMMAND IDOK`.
//! Short-circuiting the IAT skips that write, so we perform it ourselves.

use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::game_addr::GameAddr;
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const TH12_DIALOG_TEMPLATE_ID: usize = 0xCB;
const TH12_DIALOG_PROC_VA: usize = 0x0045_18c0;

/// Game's display-mode byte. Read after the dialog returns to gate the
/// fullscreen vs. windowed path; written by the dialog proc's `WM_COMMAND IDOK` branch.
const DISPLAY_MODE_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_eacd) };
const MODE_FULLSCREEN: u8 = 0;
const MODE_WINDOWED: u8 = 1;

/// `EndDialog` value returned by th12's dialog proc on the IDOK branch.
/// `main` doesn't branch on this; the dialog's effect lives entirely in
/// the side-write to `[0x004ceacd]`.
const DIALOG_RET: isize = 6;

/// "force dialog gate open": NOPs the `je 0x44f67c` at `0x0044f667` that skips
/// `DialogBoxParamA` when the Alt-held stack-local bit was clear. Without this,
/// our hook only fires when the user holds Alt at launch.
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

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_DIALOG_BOX_PARAM_A.install(host, hook_dialog_box_param_a);
        Patch::apply_all(DIALOG_PATCHES);
    }
}

unsafe extern "system" fn hook_dialog_box_param_a(
    hinst: HMODULE,
    template: *const c_char,
    parent: HWND,
    proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    let template_id = template as usize;
    let proc_va = proc.map_or(0usize, |f| f as usize);

    if template_id != TH12_DIALOG_TEMPLATE_ID || proc_va != TH12_DIALOG_PROC_VA {
        info!(
            kind = "dialog_box_param_a_passthrough",
            template = format_args!("{template_id:#x}"),
            proc = format_args!("{proc_va:#x}"),
        );
        return unsafe { real_dialog_box_param_a(hinst, template, parent, proc, init_param) };
    }

    let core_cfg = core_config::CONFIG.get().unwrap();
    let mode = core_cfg.display.mode;
    let mode_byte = match mode {
        DisplayMode::Windowed => MODE_WINDOWED,
        DisplayMode::Fullscreen => MODE_FULLSCREEN,
    };
    let prev = DISPLAY_MODE_BYTE.read();
    DISPLAY_MODE_BYTE.write(mode_byte);

    info!(
        kind = "dialog_short_circuited",
        template = format_args!("{template_id:#x}"),
        proc = format_args!("{proc_va:#x}"),
        mode = %mode,
        display_mode_prev = prev,
        display_mode_next = mode_byte,
        retval = DIALOG_RET,
    );
    DIALOG_RET
}
