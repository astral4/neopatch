//! Logic for auto-dismissing th13's window-mode startup dialog.
//!
//! `main` at `0x0045c389..0x0045c3a7` tests an "Alt held at launch" stack-local bit before
//! falling through to `DialogBoxParamA` at `0x0045c3a1` (template `0xCB`, proc at `0x0045e910`).
//! We NOP the Alt-key `je` at `0x0045c390` so the call site is reached on every launch,
//! then IAT-hook `DialogBoxParamA` to short-circuit without creating any window.
//!
//! On the IDOK branch, the real dialog proc returns 6, writes 0..3 to `[0x004dc88f]`
//! (fullscreen 640x480 / windowed 640x480 / windowed 960x720 / windowed 1280x960),
//! and clears bits 0x60 of `[0x004df0e0]`. We replicate both writes. Window size
//! is overwritten by our configuration, so 1 vs. 2/3 doesn't matter.
//! 0 vs 1 picks fullscreen vs. windowed.

use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::game_addr::GameAddr;
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::DLGPROC;

const TH13_DIALOG_TEMPLATE_ID: usize = 0xCB;
const TH13_DIALOG_PROC_VA: usize = 0x0045_e910;

/// Read by `fcn.0045da40` (window setup) to gate fullscreen vs. windowed;
/// written by the dialog proc on IDOK. 0 = fullscreen, 1/2/3 = windowed at preset sizes.
const DISPLAY_MODE_BYTE: GameAddr<u8> = unsafe { GameAddr::new(0x004d_c88f) };
const MODE_FULLSCREEN: u8 = 0;
const MODE_WINDOWED: u8 = 1;

/// Bits 0x60 gate the post-dialog cleanup/exit at `main+0x1e7`
/// (`test byte [..], 0x60; jne 0x45c1d4`). The dialog proc's IDOK clears them.
const LAUNCH_FLAGS: GameAddr<u32> = unsafe { GameAddr::new(0x004d_f0e0) };
const LAUNCH_FLAGS_DIALOG_OK_MASK: u32 = 0xffff_ff9f;

/// `EndDialog` value returned by the IDOK branch. `main` doesn't branch on it.
const DIALOG_RET: isize = 6;

/// NOPs the Alt-key `je 0x45c3a7` at `0x0045c390` so `DialogBoxParamA`
/// fires every launch instead of only when Alt is held.
const DIALOG_PATCHES: &[Patch] = &[Patch::new(
    0x0045_c390,
    &[0x74, 0x15],
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

    if template_id != TH13_DIALOG_TEMPLATE_ID || proc_va != TH13_DIALOG_PROC_VA {
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
    let flags_prev = LAUNCH_FLAGS.read();
    LAUNCH_FLAGS.write(flags_prev & LAUNCH_FLAGS_DIALOG_OK_MASK);

    info!(
        kind = "dialog_short_circuited",
        template = format_args!("{template_id:#x}"),
        proc = format_args!("{proc_va:#x}"),
        mode = %mode,
        display_mode_prev = prev,
        display_mode_next = mode_byte,
        launch_flags_prev = format_args!("{flags_prev:#010x}"),
        launch_flags_next = format_args!("{:#010x}", flags_prev & LAUNCH_FLAGS_DIALOG_OK_MASK),
        retval = DIALOG_RET,
    );
    DIALOG_RET
}
