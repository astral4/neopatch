//! Logic for auto-dismissing th17's startup dialog.
//!
//! We let the dialog's message pump continue running because th17 folds it into the main loop,
//! so the game tick advances while the dialog is up. This is done by IAT-hooking
//! `CreateDialogParamA`, overriding the dialog's selections from our config, and using
//! `PostMessage` to send an OK click; th17's OK handler (`fcn.00463400`) tears the dialog down
//! itself, so no pump exit-flag is needed.

use crate::config::CONFIG;
use neopatch_core::config::{self as core_config, DisplayMode};
use neopatch_core::iat_hook;
use neopatch_core::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, CheckRadioButton,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{BN_CLICKED, DLGPROC, PostMessageA, WM_COMMAND};

const DIALOG_TEMPLATE_ID: usize = 0xCB;
const DIALOG_PROC_VA: usize = 0x0046_3400;

// Dialog control IDs (confirmed in the th17 dialog proc):
// - 0xCA "don't show again" checkbox
// - 0xCB fullscreen checkbox
// - 0xCD/CE/CF render-size radios (640x480 / 960x720 / 1280x960)
// - 0xD0 OK button
const FULLSCREEN_CHECKBOX_ID: i32 = 0xCB;
const RES_RADIO_FIRST_ID: i32 = 0xCD;
const RES_RADIO_LAST_ID: i32 = 0xCF;
const OK_BUTTON_ID: u32 = 0xD0;

/// "force resolution dialog": `jne 0x460b9f` -> `jmp 0x460b9f` makes the dialog path
/// unconditional, so our hook fires every launch instead of being gated by th17.cfg's
/// persisted "show this dialog" bit or an Alt-held-at-launch check.
///
/// "force dialog hidden": `push 5` (SW_SHOW) -> `push 0` (SW_HIDE) on the dialog's
/// `ShowWindow`. The OK handler still runs and applies the selection invisibly.
const DIALOG_PATCHES: &[Patch] = &[
    Patch::new(
        0x0046_0b87,
        &[0x75, 0x16],
        &[0xeb, 0x16],
        "force resolution dialog",
    ),
    Patch::new(
        0x0046_0bb4,
        &[0x6a, 0x05],
        &[0x6a, 0x00],
        "force dialog hidden",
    ),
];

iat_hook! {
    REAL_CREATE_DIALOG_PARAM_A / real_create_dialog_param_a : "CreateDialogParamA"
        as fn(
            hinst: HMODULE,
            template: *const c_char,
            parent: HWND,
            proc: DLGPROC,
            init_param: LPARAM,
        ) -> HWND;
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_CREATE_DIALOG_PARAM_A.install(host, hook_create_dialog_param_a);
        Patch::apply_all(DIALOG_PATCHES);
    }
}

unsafe extern "system" fn hook_create_dialog_param_a(
    hinst: HMODULE,
    template: *const c_char,
    parent: HWND,
    proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    unsafe {
        let hwnd = real_create_dialog_param_a(hinst, template, parent, proc, init_param);

        let template_id = template as usize;
        let proc_va = proc.map_or(0usize, |f| f as usize);
        info!(
            kind = "create_dialog_param_a",
            template = format_args!("{template_id:#x}"),
            proc = format_args!("{proc_va:#x}"),
            hwnd = format_args!("{hwnd:?}"),
        );

        if hwnd.is_null() {
            return hwnd;
        }
        if template_id != DIALOG_TEMPLATE_ID || proc_va != DIALOG_PROC_VA {
            return hwnd;
        }

        let th17_cfg = CONFIG.get().unwrap();
        let core_cfg = core_config::CONFIG.get().unwrap();

        let res_radio_id = RES_RADIO_FIRST_ID + i32::from(th17_cfg.resolution.index());
        let fullscreen = matches!(core_cfg.display.mode, DisplayMode::Fullscreen);

        // The range is restricted to 0xCD..0xCF so `CheckRadioButton`'s
        // "clear others in range" doesn't hit the checkboxes at 0xCA/CB.
        let radio_ret = CheckRadioButton(hwnd, RES_RADIO_FIRST_ID, RES_RADIO_LAST_ID, res_radio_id);
        let fs_state = if fullscreen {
            BST_CHECKED
        } else {
            BST_UNCHECKED
        };
        let dlg_btn_ret = CheckDlgButton(hwnd, FULLSCREEN_CHECKBOX_ID, fs_state);
        // Drives the dialog proc's own OK handler (reads the control state, then self-destructs).
        let wparam = ((BN_CLICKED << 16) | OK_BUTTON_ID) as WPARAM;
        let pm_ok = PostMessageA(hwnd, WM_COMMAND, wparam, 0);
        info!(
            kind = "dialog_auto_dismissed",
            resolution = %th17_cfg.resolution,
            mode = %core_cfg.display.mode,
            res_radio = format_args!("{res_radio_id:#x}"),
            fullscreen,
            fs_state,
            check_radio_button = radio_ret,
            check_dlg_button = dlg_btn_ret,
            post_message_ok = pm_ok,
        );
        hwnd
    }
}
