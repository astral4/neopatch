//! Logic for auto-dismissing th15's startup dialog.

use crate::CONFIG;
use neopatch_core::config::{CONFIG as CORE_CONFIG, DisplayMode};
use neopatch_core::game_addr::GameAddr;
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
const DIALOG_PROC_VA: usize = 0x0047_3de0;
const RES_RADIO_FIRST_ID: i32 = 0xcd;
const RES_RADIO_LAST_ID: i32 = 0xcf;
const FULLSCREEN_CHECKBOX_ID: i32 = 0xcb;
const OK_BUTTON_ID: u32 = 0xd0;
const EXIT_FLAG_VA: GameAddr<u32> = unsafe { GameAddr::new(0x004e_6d1c) };
const EXIT_FLAG_BIT: u32 = 0x0008_0000;

const CREATE_DIALOG_CALL_VA: usize = 0x0047_1619;
const CREATE_DIALOG_CALL_BYTES: [u8; 6] = [0xff, 0x15, 0x30, 0xe2, 0x4b, 0x00];

pub(crate) const DIALOG_PATCHES: &[PatchSite] = &[
    PatchSite::replace(0x0047_15f2, &[0x75], &[0xeb], "force resolution dialog"),
    PatchSite::replace(0x0047_1620, &[0x05], &[0x00], "force dialog hidden"),
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
    let prev = EXIT_FLAG_VA.read();
    let next = prev | EXIT_FLAG_BIT;
    EXIT_FLAG_VA.write(next);

    info!(
        kind = "dialog_auto_dismissed",
        mode = %mode,
        resolution = %resolution,
        res_radio = format_args!("{res_radio_id:#x}"),
        check_radio_button = radio_ret,
        check_dlg_button = dlg_btn_ret,
        post_message_ok = pm_ok,
        exit_flag_prev = format_args!("{prev:#010x}"),
        exit_flag_next = format_args!("{next:#010x}"),
    );
    hwnd
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe { REAL_CREATE_DIALOG_PARAM_A.install(host, hook_create_dialog_param_a) };
}
