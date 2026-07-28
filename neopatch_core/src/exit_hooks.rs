//! Logging and passthrough hooks for game exit logic.

use crate::ansi::{codepage, to_wide};
use crate::iat_hook;
use crate::log::flush;
use crate::untrusted::Untrusted;
use std::ffi::c_void;
use std::num::NonZero;
use std::ptr::null;
use std::slice::from_mut as slice_from_mut;
use tracing::info;
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::LPTHREAD_START_ROUTINE;
use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
use windows_sys::core::{PCSTR, PCWSTR};

const MAX_MSGBOX_LEN: usize = 4096;

iat_hook! {
    REAL_EXIT_PROCESS / real_exit_process : "ExitProcess"
        as fn(exit_code: u32) -> !;
}
iat_hook! {
    REAL_TERMINATE_PROCESS / real_terminate_process : "TerminateProcess"
        as fn(process: HANDLE, exit_code: u32) -> i32;
}
iat_hook! {
    REAL_MESSAGE_BOX_A / real_message_box_a : "MessageBoxA"
        as fn(parent: HWND, text: PCSTR, caption: PCSTR, flags: u32) -> i32;
}
iat_hook! {
    REAL_MESSAGE_BOX_W / real_message_box_w : "MessageBoxW"
        as fn(parent: HWND, text: PCWSTR, caption: PCWSTR, flags: u32) -> i32;
}
iat_hook! {
    REAL_CREATE_THREAD / real_create_thread : "CreateThread"
        as fn(
            sec: *const SECURITY_ATTRIBUTES,
            stack: usize,
            start: LPTHREAD_START_ROUTINE,
            param: *const c_void,
            flags: u32,
            tid_out: *mut u32,
        ) -> HANDLE;
}

/// IAT-hooks the process-lifetime imports we wrap for diagnostics
/// (`ExitProcess`, `TerminateProcess`, `MessageBox{A,W}`, `CreateThread`) against `host`'s import table.
/// `RaiseException` is deliberately not hooked since those are already seen by the vectored handler in `crash.rs`.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_EXIT_PROCESS.install(host, hook_exit_process);
        REAL_TERMINATE_PROCESS.install(host, hook_terminate_process);
        REAL_MESSAGE_BOX_A.install(host, hook_message_box_a);
        REAL_MESSAGE_BOX_W.install(host, hook_message_box_w);
        REAL_CREATE_THREAD.install(host, hook_create_thread);
    }
}

unsafe extern "system" fn hook_exit_process(exit_code: u32) -> ! {
    info!(
        kind = "exit_process_intercepted",
        exit_code = format_args!("{exit_code:#010x}"),
    );
    // We drain the `BufWriter` before the OS tears down the process. Otherwise, the destructor and shutdown tail of the log are lost.
    flush();
    unsafe { real_exit_process(exit_code) }
}

unsafe extern "system" fn hook_terminate_process(process: HANDLE, exit_code: u32) -> i32 {
    info!(
        kind = "terminate_process_intercepted",
        process = format_args!("{process:?}"),
        exit_code = format_args!("{exit_code:#010x}"),
    );
    flush();
    unsafe { real_terminate_process(process, exit_code) }
}

unsafe extern "system" fn hook_message_box_a(
    parent: HWND,
    text: PCSTR,
    caption: PCSTR,
    flags: u32,
) -> i32 {
    if let Some(ret) = try_show_wide(parent, text, caption, flags) {
        return ret;
    }

    let text_str = pcstr_to_string(Untrusted::from_raw(text));
    let caption_str = pcstr_to_string(Untrusted::from_raw(caption));
    info!(
        kind = "message_box_a_intercepted",
        flags = format_args!("{flags:#x}"),
        caption = ?caption_str,
        text = ?text_str,
    );
    unsafe { real_message_box_a(parent, text, caption, flags) }
}

/// Shows the dialog through `MessageBoxW` when a side needs the game code page. With a code page registered,
/// the system's own A-to-W conversion would mangle game-encoded (e.g. Shift-JIS) text on a non-Japanese locale,
/// so we convert it ourselves. This deliberately bypasses any other DLL's chained `MessageBoxA` hook for such dialogs,
/// since forwarding would mean converting back through the system code page, which is the lossy step being avoided.
///
/// Returns `None` without showing anything when the conversion doesn't apply. This occurs when there is no game code page,
/// every side is ASCII or null, or there is a side whose non-ASCII bytes fail to convert (showing a half-converted dialog would be worse).
fn try_show_wide(parent: HWND, text: PCSTR, caption: PCSTR, flags: u32) -> Option<i32> {
    let cp = codepage()?;
    let mut scratch = [0u8; MAX_MSGBOX_LEN];
    if !has_non_ascii(text, &mut scratch) && !has_non_ascii(caption, &mut scratch) {
        return None;
    }

    let text_w = widen(cp, text, &mut scratch).ok()?;
    let caption_w = widen(cp, caption, &mut scratch).ok()?;

    info!(
        kind = "message_box_a_converted",
        flags = format_args!("{flags:#x}"),
        caption = %wide_log_string(caption_w.as_deref()),
        text = %wide_log_string(text_w.as_deref()),
    );

    Some(unsafe {
        MessageBoxW(
            parent,
            opt_pcwstr(text_w.as_deref()),
            opt_pcwstr(caption_w.as_deref()),
            flags,
        )
    })
}

fn has_non_ascii(p: PCSTR, scratch: &mut [u8]) -> bool {
    !p.is_null()
        && !Untrusted::from_raw(p)
            .safe_read_until(scratch, 0)
            .is_ascii()
}

/// Widens one `MessageBoxA` string through the caller's `scratch`. Returns `Ok(None)` for a null pointer, ASCII bytes widened 1:1,
/// and anything else converted through `codepage`. Returns `Err` if conversion fails.
fn widen(codepage: NonZero<u32>, p: PCSTR, scratch: &mut [u8]) -> Result<Option<Vec<u16>>, ()> {
    if p.is_null() {
        return Ok(None);
    }

    let bytes = Untrusted::from_raw(p).safe_read_until(scratch, 0);
    if bytes.is_ascii() {
        return Ok(Some(
            bytes.iter().map(|&b| u16::from(b)).chain([0]).collect(),
        ));
    }

    to_wide(codepage, bytes, false).ok_or(()).map(Some)
}

fn opt_pcwstr(side: Option<&[u16]>) -> PCWSTR {
    side.map_or_else(null, <[u16]>::as_ptr)
}

fn wide_log_string(side: Option<&[u16]>) -> String {
    side.map_or_else(
        || String::from("<null>"),
        |wide| String::from_utf16_lossy(wide.strip_suffix(&[0]).unwrap_or(wide)),
    )
}

unsafe extern "system" fn hook_message_box_w(
    parent: HWND,
    text: PCWSTR,
    caption: PCWSTR,
    flags: u32,
) -> i32 {
    let text_str = pcwstr_to_string(Untrusted::from_raw(text));
    let caption_str = pcwstr_to_string(Untrusted::from_raw(caption));
    info!(
        kind = "message_box_w_intercepted",
        flags = format_args!("{flags:#x}"),
        caption = ?caption_str,
        text = ?text_str,
    );
    unsafe { real_message_box_w(parent, text, caption, flags) }
}

unsafe extern "system" fn hook_create_thread(
    sec: *const SECURITY_ATTRIBUTES,
    stack: usize,
    start: LPTHREAD_START_ROUTINE,
    param: *const c_void,
    flags: u32,
    tid_out: *mut u32,
) -> HANDLE {
    let h = unsafe { real_create_thread(sec, stack, start, param, flags, tid_out) };
    let start_va = start.map_or(0, |f| (f as *const ()).addr());
    let tid_out = Untrusted::from_raw(tid_out.cast_const());
    let mut tid: u32 = 0;
    if !tid_out.is_null() {
        tid_out.safe_read(slice_from_mut(&mut tid));
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

fn pcstr_to_string(p: Untrusted<u8>) -> String {
    if p.is_null() {
        return String::from("<null>");
    }
    let mut buf = [0u8; 4096];
    String::from_utf8_lossy(p.safe_read_until(&mut buf, 0)).into_owned()
}

fn pcwstr_to_string(p: Untrusted<u16>) -> String {
    if p.is_null() {
        return String::from("<null>");
    }
    let mut buf = [0u16; 4096];
    String::from_utf16_lossy(p.safe_read_until(&mut buf, 0))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_MSGBOX_LEN, has_non_ascii as has_non_ascii_scratch, opt_pcwstr, widen as widen_scratch,
    };
    use crate::ansi::CP_SHIFT_JIS;
    use std::ptr::null;
    use windows_sys::core::PCSTR;

    const TEXT_SHIFT_JIS: &[u8] = &[0x93, 0x8c, 0x95, 0xfb, 0x00];
    const TEXT_WIDE: &[u16] = &[0x6771, 0x65b9, 0];

    fn has_non_ascii(p: PCSTR) -> bool {
        has_non_ascii_scratch(p, &mut [0u8; MAX_MSGBOX_LEN])
    }

    fn widen(p: PCSTR) -> Result<Option<Vec<u16>>, ()> {
        widen_scratch(CP_SHIFT_JIS, p, &mut [0u8; MAX_MSGBOX_LEN])
    }

    #[test]
    fn null_side_passthrough() {
        assert!(!has_non_ascii(null()));
        let side = widen(null()).unwrap();
        assert!(side.is_none());
        assert!(opt_pcwstr(side.as_deref()).is_null());
    }

    #[test]
    fn ascii_side_not_on_w_path() {
        for s in [c"oops", c""] {
            assert!(!has_non_ascii(s.as_ptr().cast()), "{s:?}");
            let side = widen(s.as_ptr().cast()).unwrap();
            assert!(!opt_pcwstr(side.as_deref()).is_null(), "{s:?}");
        }
    }

    #[test]
    fn ascii_widening_one_to_one() {
        let side = widen(c"oops".as_ptr().cast()).unwrap();
        assert_eq!(
            side.as_deref(),
            Some([0x6f, 0x6f, 0x70, 0x73, 0].as_slice())
        );
    }

    #[test]
    fn empty_string_widening() {
        let side = widen(c"".as_ptr().cast()).unwrap();
        assert_eq!(side.as_deref(), Some([0u16].as_slice()));
    }

    #[test]
    fn shift_jis_conversion() {
        assert!(has_non_ascii(TEXT_SHIFT_JIS.as_ptr()));
        let side = widen(TEXT_SHIFT_JIS.as_ptr()).unwrap();
        assert_eq!(side.as_deref(), Some(TEXT_WIDE));
    }
}
