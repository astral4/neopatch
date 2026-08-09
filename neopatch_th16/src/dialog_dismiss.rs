//! Logic for auto-dismissing th16's startup dialog.

use crate::CONFIG;
use neopatch_core::config::{CONFIG as CORE_CONFIG, DisplayMode};
use neopatch_core::iat_hook;
use neopatch_core::patches::PatchSite;
use std::ffi::c_char;
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, CheckRadioButton,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{BN_CLICKED, DLGPROC, PostMessageA, WM_COMMAND};

const DIALOG_TEMPLATE_ID: usize = 0xcb;
const DIALOG_PROC_VA: usize = 0x0045_c110;
const RES_RADIO_FIRST_ID: i32 = 0xcd;
const RES_RADIO_LAST_ID: i32 = 0xcf;
const FULLSCREEN_CHECKBOX_ID: i32 = 0xcb;
const OK_BUTTON_ID: u32 = 0xd0;

const CREATE_DIALOG_CALL_VA: usize = 0x0045_9b13;
const CREATE_DIALOG_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x20, 0xb2, 0x48, 0x00];

pub(crate) const DIALOG_PATCHES: &[PatchSite] = &[
    PatchSite::replace(
        0x0045_9ae9,
        &[0x75, 0x16],
        &[0xeb, 0x16],
        "force resolution dialog",
    ),
    PatchSite::replace(
        0x0045_9b19,
        &[0x6a, 0x05],
        &[0x6a, 0x00],
        "force dialog hidden",
    ),
    PatchSite::call(
        CREATE_DIALOG_CALL_VA,
        &CREATE_DIALOG_CALL_BYTES,
        hook_create_dialog_param_a as *mut (),
        "CreateDialogParamA call-site rewrite",
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

unsafe extern "system" fn hook_create_dialog_param_a(
    hinst: HMODULE,
    template: *const c_char,
    parent: HWND,
    proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    let hwnd = unsafe { real_create_dialog_param_a(hinst, template, parent, proc, init_param) };

    let template_id = template.addr();
    let proc_va = proc.map_or(0usize, |f| (f as *const ()).addr());
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

    let mode = CORE_CONFIG.get().unwrap().display.mode;
    let resolution = CONFIG.get().unwrap().resolution;

    let res_radio_id = RES_RADIO_FIRST_ID + i32::from(resolution.index());
    let fullscreen_state = match mode {
        DisplayMode::Windowed => BST_UNCHECKED,
        DisplayMode::Fullscreen => BST_CHECKED,
    };
    let wparam = ((BN_CLICKED << 16) | OK_BUTTON_ID) as WPARAM;

    let radio_ret =
        unsafe { CheckRadioButton(hwnd, RES_RADIO_FIRST_ID, RES_RADIO_LAST_ID, res_radio_id) };
    let dlg_btn_ret = unsafe { CheckDlgButton(hwnd, FULLSCREEN_CHECKBOX_ID, fullscreen_state) };
    let pm_ok = unsafe { PostMessageA(hwnd, WM_COMMAND, wparam, 0) };

    info!(
        kind = "dialog_auto_dismissed",
        mode = %mode,
        resolution = %resolution,
        res_radio = format_args!("{res_radio_id:#x}"),
        check_radio_button = radio_ret,
        check_dlg_button = dlg_btn_ret,
        post_message_ok = pm_ok,
    );
    hwnd
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe { REAL_CREATE_DIALOG_PARAM_A.install(host, hook_create_dialog_param_a) };
}
