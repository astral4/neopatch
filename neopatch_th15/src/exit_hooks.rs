//! Logging and passthrough hooks for game exit logic.
//! Includes handlers for `ExitProcess`, `TerminateProcess`, `MessageBoxA`/`MessageBoxW`,
//! `RaiseException`, and `CreateThread` (for thread provenance).

use crate::crash::{safe_read, safe_read_until};
use crate::iat_hook;
use crate::log::flush;
use std::ffi::c_void;
use std::process::abort;
use std::slice::from_mut as slice_from_mut;
use tracing::info;
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::LPTHREAD_START_ROUTINE;
use windows_sys::core::{PCSTR, PCWSTR};

iat_hook! {
    REAL_EXIT_PROCESS / real_exit_process : c"ExitProcess"
        as fn(exit_code: u32) -> ();
}
iat_hook! {
    REAL_TERMINATE_PROCESS / real_terminate_process : c"TerminateProcess"
        as fn(process: HANDLE, exit_code: u32) -> i32;
}
iat_hook! {
    REAL_MESSAGE_BOX_A / real_message_box_a : c"MessageBoxA"
        as fn(parent: HWND, text: PCSTR, caption: PCSTR, flags: u32) -> i32;
}
iat_hook! {
    REAL_MESSAGE_BOX_W / real_message_box_w : c"MessageBoxW"
        as fn(parent: HWND, text: PCWSTR, caption: PCWSTR, flags: u32) -> i32;
}
iat_hook! {
    REAL_RAISE_EXCEPTION / real_raise_exception : c"RaiseException"
        as fn(code: u32, flags: u32, nargs: u32, args: *const u32) -> ();
}
iat_hook! {
    REAL_CREATE_THREAD / real_create_thread : c"CreateThread"
        as fn(
            sec: *const SECURITY_ATTRIBUTES,
            stack: usize,
            start: LPTHREAD_START_ROUTINE,
            param: *const c_void,
            flags: u32,
            tid_out: *mut u32,
        ) -> HANDLE;
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_EXIT_PROCESS.install(host, hook_exit_process);
        REAL_TERMINATE_PROCESS.install(host, hook_terminate_process);
        REAL_MESSAGE_BOX_A.install(host, hook_message_box_a);
        REAL_MESSAGE_BOX_W.install(host, hook_message_box_w);
        REAL_RAISE_EXCEPTION.install(host, hook_raise_exception);
        REAL_CREATE_THREAD.install(host, hook_create_thread);
    }
}

unsafe extern "system" fn hook_exit_process(exit_code: u32) {
    unsafe {
        info!(
            kind = "exit_process_intercepted",
            exit_code = format_args!("{exit_code:#010x}"),
        );
        // Drain the `BufWriter` before the OS tears down the process.
        // Otherwise, the destructor and shutdown tail of the log are lost.
        flush();
        real_exit_process(exit_code);
        abort();
    }
}

unsafe extern "system" fn hook_terminate_process(process: HANDLE, exit_code: u32) -> i32 {
    unsafe {
        info!(
            kind = "terminate_process_intercepted",
            process = format_args!("{process:?}"),
            exit_code = format_args!("{exit_code:#010x}"),
        );
        flush();
        real_terminate_process(process, exit_code)
    }
}

unsafe extern "system" fn hook_message_box_a(
    parent: HWND,
    text: PCSTR,
    caption: PCSTR,
    flags: u32,
) -> i32 {
    unsafe {
        let text_str = pcstr_to_string(text);
        let caption_str = pcstr_to_string(caption);
        info!(
            kind = "message_box_a_intercepted",
            flags = format_args!("{flags:#x}"),
            caption = ?caption_str,
            text = ?text_str,
        );
        real_message_box_a(parent, text, caption, flags)
    }
}

unsafe extern "system" fn hook_message_box_w(
    parent: HWND,
    text: PCWSTR,
    caption: PCWSTR,
    flags: u32,
) -> i32 {
    unsafe {
        let text_str = pcwstr_to_string(text);
        let caption_str = pcwstr_to_string(caption);
        info!(
            kind = "message_box_w_intercepted",
            flags = format_args!("{flags:#x}"),
            caption = ?caption_str,
            text = ?text_str,
        );
        real_message_box_w(parent, text, caption, flags)
    }
}

unsafe extern "system" fn hook_raise_exception(
    code: u32,
    flags: u32,
    nargs: u32,
    args: *const u32,
) {
    unsafe {
        info!(
            kind = "raise_exception_intercepted",
            code = format_args!("{code:#010x}"),
            flags = format_args!("{flags:#x}"),
            nargs,
        );
        real_raise_exception(code, flags, nargs, args);
    }
}

unsafe extern "system" fn hook_create_thread(
    sec: *const SECURITY_ATTRIBUTES,
    stack: usize,
    start: LPTHREAD_START_ROUTINE,
    param: *const c_void,
    flags: u32,
    tid_out: *mut u32,
) -> HANDLE {
    unsafe {
        let h = real_create_thread(sec, stack, start, param, flags, tid_out);
        let start_va = start.map_or(0, |f| f as usize);
        // `tid_out` is caller-controlled; we route through `safe_read` rather than
        // a direct deref so an invalid (but non-null) pointer logs `tid=0`
        // instead of AV'ing inside the hook.
        let mut tid: u32 = 0;
        if !tid_out.is_null() {
            safe_read(tid_out, slice_from_mut(&mut tid));
        }
        info!(
            kind = "thread_spawned",
            tid,
            start = format_args!("{start_va:#010x}"),
            param = format_args!("{param:p}"),
            handle = format_args!("{h:?}"),
        );
        h
    }
}

fn pcstr_to_string(p: PCSTR) -> String {
    if p.is_null() {
        return String::from("<null>");
    }
    let mut buf = [0u8; 4096];
    String::from_utf8_lossy(safe_read_until(p, &mut buf, 0)).into_owned()
}

fn pcwstr_to_string(p: PCWSTR) -> String {
    if p.is_null() {
        return String::from("<null>");
    }
    let mut buf = [0u16; 4096];
    String::from_utf16_lossy(safe_read_until(p, &mut buf, 0))
}
