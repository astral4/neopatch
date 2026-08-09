//! Signal handling for graceful termination.

use crate::log::flush;
use crate::window::main_hwnd;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, GetFileType, WriteFile};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, GetStdHandle, STD_ERROR_HANDLE, SetConsoleCtrlHandler,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows_sys::core::BOOL;

const HANDLED: BOOL = 1;
const UNHANDLED: BOOL = 0;

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
    if !matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        return UNHANDLED;
    }

    if QUIT_REQUESTED.load(Ordering::Relaxed) {
        info!(kind = "console_ctrl_quit_already_requested", ctrl_type);
        console_hint(
            b"neopatch: quit already requested; close this console window to force-kill\n",
        );
        return HANDLED;
    }

    let hwnd = main_hwnd();
    if hwnd.is_null() {
        info!(kind = "console_ctrl_no_window", ctrl_type);
        flush();
        return UNHANDLED;
    }

    if unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } == 0 {
        info!(kind = "console_ctrl_post_failed", ctrl_type);
        flush();
        return UNHANDLED;
    }

    QUIT_REQUESTED.store(true, Ordering::Relaxed);
    info!(kind = "console_ctrl_close_posted", ctrl_type);
    HANDLED
}

/// Sends a best-effort note to the terminal the game was launched from.
fn console_hint(msg: &[u8]) {
    let h = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return;
    }
    if unsafe { GetFileType(h) } != FILE_TYPE_CHAR {
        return;
    }
    let Ok(len) = u32::try_from(msg.len()) else {
        return;
    };
    let mut written = 0u32;
    unsafe { WriteFile(h, msg.as_ptr(), len, &raw mut written, null_mut()) };
}

/// Installs the console control handler. This should be called after logging is initialized.
pub fn install() {
    if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
        warn!(kind = "console_ctrl_handler_install_failed");
    } else {
        info!(kind = "console_ctrl_handler_installed");
    }
}
