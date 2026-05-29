//! Logic for auto-dismissing th11's window-mode startup dialog.
//!
//! `main` at `0x00445610..0x00445626` gates `DialogBoxParamA` on either the
//! `[0x4c3480] & 0x100` checkbox or holding Alt at launch. We NOP the Alt-key skip at
//! `0x00445617` so the call site is reached on every launch, then IAT-hook `DialogBoxParamA`
//! to short-circuit without creating any window.
//!
//! th11's dialog proc returns `EndDialog(hwnd, 6)` on IDOK regardless of selection;
//! the selection is a side-write to `[0x4c3465]`. We replicate it.

use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::game_addr::GameAddr;
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const DIALOG_TEMPLATE_ID: usize = 0xCB;
const DIALOG_PROC_VA: usize = 0x0044_7910;

/// Read at `fcn.00446d30` to gate fullscreen vs. windowed; written by the dialog proc on IDOK.
const DISPLAY_MODE_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004c_3465) };
const MODE_FULLSCREEN: u8 = 0;
const MODE_WINDOWED: u8 = 1;

/// `EndDialog` value returned by the IDOK branch. `main` doesn't branch on it.
const DIALOG_RET: isize = 6;

/// NOPs the Alt-key `je 0x44562c` at `0x00445617` so `DialogBoxParamA`
/// fires every launch instead of only when Alt is held.
const DIALOG_PATCHES: &[Patch] = &[Patch::new(
    0x0044_5617,
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

    if template_id != DIALOG_TEMPLATE_ID || proc_va != DIALOG_PROC_VA {
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
