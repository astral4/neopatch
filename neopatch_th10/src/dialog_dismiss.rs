//! Logic for auto-dismissing th10's window-mode startup dialog.
//!
//! `fcn.00438ad0` calls `DialogBoxParamA` with template `0xCB` and the dialog proc
//! at `0x0043a3a0` when `[0x491d78]` bit 8 ("ask every time", loaded from `th10.cfg`) is set.
//! The proc's `EndDialog` value lands in `[0x491d65]`, which the game later passes
//! as `Windowed` to `CreateDevice`. We NOP the dialog-creation gate so the call always fires,
//! then short-circuit the IAT hook to return the value the original proc would have
//! for our INI mode, without creating any window.

use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const TH10_DIALOG_TEMPLATE_ID: usize = 0xCB;
const TH10_DIALOG_PROC_VA: usize = 0x0043_a3a0;

/// `EndDialog` returns the dialog proc uses: Fullscreen radio (id `0xC9`) routes through
/// `0x43a418` -> `EndDialog(6)`; Window radio (id `0xCB`) routes through
/// `0x43a3cc` -> `EndDialog(7)`. Downstream `setne cl` against `6` writes
/// `0` for fullscreen, `1` for windowed to `[0x491d65]`.
const DIALOG_RET_FULLSCREEN: isize = 6;
const DIALOG_RET_WINDOWED: isize = 7;

/// "force dialog gate open": NOPs `je 0x438b9e`, the gate that skips
/// `DialogBoxParamA` when `[0x491d78]` bit 8 is clear. Without it,
/// our hook only fires when `custom.exe` is in "Ask every time" mode.
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
        for patch in DIALOG_PATCHES {
            patch.apply();
        }
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
