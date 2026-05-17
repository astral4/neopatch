//! Logic for auto-dismissing th15's startup dialog.
//!
//! We let the dialog's message pump continue running because the loader thread deadlocks otherwise.
//! This is done by IAT-hooking `CreateDialogParamA`,
//! overriding the dialog's selections from our config, and then
//! using `PostMessage` to send an OK click and set the pump's exit-flag bit.

use crate::config::{CONFIG, DisplayMode};
use crate::game_addr::GameAddr;
use crate::iat_hook;
use crate::patches::Patch;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, CheckRadioButton,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{BN_CLICKED, DLGPROC, PostMessageA, WM_COMMAND};

const TH15_DIALOG_TEMPLATE_ID: usize = 0xCB;
const TH15_DIALOG_PROC_VA: usize = 0x0047_3DE0;

// Dialog control IDs:
// - 0xCA "don't show again" checkbox
// - 0xCB fullscreen checkbox
// - 0xCC unused
// - 0xCD/CE/CF resolution radios (640x480/960x720/1280x960)
//
// The OK handler computes `[0x4e79c3] = res_radio_index + (0xCB checked ? 0 : 3)`,
// so we have to set both the resolution radio and the fullscreen checkbox.
const FULLSCREEN_CHECKBOX_ID: i32 = 0xCB;
const RES_RADIO_FIRST_ID: i32 = 0xCD;
const RES_RADIO_LAST_ID: i32 = 0xCF;

const OK_BUTTON_ID: u32 = 0xD0;

/// The pump exit predicate at `0x4716A2` is `test [0x4e6d1c], 0x80001`.
/// We set bit 19 ("Enter accept") to terminate the pump after the posted OK click is dispatched.
const EXIT_FLAG: GameAddr<u32> = unsafe { GameAddr::new(0x004E_6D1C) };
const EXIT_FLAG_BIT: u32 = 0x0008_0000;

/// "force resolution dialog" makes the dialog-creation gate unconditional
/// so our IAT hook fires on every launch; otherwise, th15.cfg's "don't show again" bit
/// suppresses the dialog after the first run.
/// "force dialog hidden" keeps the explicit `ShowWindow` from rendering the dialog.
/// The template lacks `WS_VISIBLE`, so `SW_HIDE` is a no-op on an already-hidden window.
/// The OK handler still runs and writes `[0x4e79c3]` invisibly.
const DIALOG_PATCHES: &[Patch] = &[
    Patch::new(0x0047_15f2, &[0x75], &[0xeb], "force resolution dialog"),
    Patch::new(0x0047_1620, &[0x05], &[0x00], "force dialog hidden"),
];

iat_hook! {
    REAL_CREATE_DIALOG_PARAM_A / real_create_dialog_param_a : c"CreateDialogParamA"
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
        for patch in DIALOG_PATCHES {
            patch.apply();
        }
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
            "dialog_dismiss: CreateDialogParamA template={template_id:#x} proc={proc_va:#x} -> hwnd={:?}",
            hwnd,
        );

        if hwnd.is_null() {
            return hwnd;
        }
        if template_id != TH15_DIALOG_TEMPLATE_ID || proc_va != TH15_DIALOG_PROC_VA {
            return hwnd;
        }

        let cfg = CONFIG.get().unwrap();

        let res_radio_id = RES_RADIO_FIRST_ID + i32::from(cfg.display.resolution.index());
        let fullscreen = matches!(cfg.display.mode, DisplayMode::Fullscreen);

        // Restrict the radio range to 0xCD..0xCF. Otherwise, `CheckRadioButton`'s
        // "clear all others in range" would otherwise hit the checkboxes at 0xCA/CB/CC.
        let radio_ret = CheckRadioButton(hwnd, RES_RADIO_FIRST_ID, RES_RADIO_LAST_ID, res_radio_id);
        let fs_state = if fullscreen {
            BST_CHECKED
        } else {
            BST_UNCHECKED
        };
        let dlg_btn_ret = CheckDlgButton(hwnd, FULLSCREEN_CHECKBOX_ID, fs_state);
        let wparam = ((BN_CLICKED << 16) | OK_BUTTON_ID) as WPARAM;
        let pm_ok = PostMessageA(hwnd, WM_COMMAND, wparam, 0);
        // Post first, then set the exit bit. th15's pump at `0x471633` dispatches
        // queued messages before re-testing `[0x4e6d1c]` at `0x471698`, so the OK
        // handler's resolution write at `[0x4e79c3]` runs on the same iteration our
        // bit terminates the loop.
        let prev = EXIT_FLAG.read();
        let next = prev | EXIT_FLAG_BIT;
        EXIT_FLAG.write(next);
        info!(
            "dialog_dismiss: th15 dialog auto-dismissed; resolution={res} mode={mode} res_radio={res_radio_id:#x} fullscreen={fullscreen} CheckRadioButton={radio_ret} CheckDlgButton(0xCB,{fs_state})={dlg_btn_ret} PostMessage(WM_COMMAND,IDOK)={pm_ok} [0x4e6d1c]:{prev:#010x}->{next:#010x}",
            res = cfg.display.resolution,
            mode = cfg.display.mode,
        );
        hwnd
    }
}
