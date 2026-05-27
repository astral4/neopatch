//! Logic for auto-dismissing th10's window-mode startup dialog.
//!
//! `fcn.00438ad0` calls `DialogBoxParamA` (template `0xCB`, proc `0x0043a3a0`) when
//! `[0x491d78] & 0x100` is set. The proc's `EndDialog` value lands in `[0x491d65]`,
//! which the game later passes as `Windowed` to `CreateDevice`. We NOP the gate so the call
//! always fires, then short-circuit the IAT to return the value the proc would have for our
//! own configuration without creating any window.

use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const TH10_DIALOG_TEMPLATE_ID: usize = 0xCB;
const TH10_DIALOG_PROC_VA: usize = 0x0043_a3a0;

/// `EndDialog` values: Fullscreen radio (`0xC9`) -> 6; Window radio (`0xCB`) -> 7.
/// Downstream `setne cl` against `6` writes 0 / 1 to `[0x491d65]`.
const DIALOG_RET_FULLSCREEN: isize = 6;
const DIALOG_RET_WINDOWED: isize = 7;

/// NOPs `je 0x438b9e` so `DialogBoxParamA` fires regardless of
/// `custom.exe`'s "Ask every time" checkbox.
const DIALOG_PATCHES: &[Patch] = &[Patch::new(
    0x0043_8b7b,
    &[0x74, 0x21],
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

    if template_id != TH10_DIALOG_TEMPLATE_ID || proc_va != TH10_DIALOG_PROC_VA {
        info!(
            kind = "dialog_box_param_a_passthrough",
            template = format_args!("{template_id:#x}"),
            proc = format_args!("{proc_va:#x}"),
        );
        return unsafe { real_dialog_box_param_a(hinst, template, parent, proc, init_param) };
    }

    let core_cfg = core_config::CONFIG.get().unwrap();
    let mode = core_cfg.display.mode;
    let retval = match mode {
        DisplayMode::Windowed => DIALOG_RET_WINDOWED,
        DisplayMode::Fullscreen => DIALOG_RET_FULLSCREEN,
    };
    info!(
        kind = "dialog_short_circuited",
        template = format_args!("{template_id:#x}"),
        proc = format_args!("{proc_va:#x}"),
        mode = %mode,
        retval,
    );
    retval
}
