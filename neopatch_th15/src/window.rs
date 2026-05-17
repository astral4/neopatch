//! Window setup and hooking.

use crate::config::{DisplayMode, WindowCfg, WindowFrame};
use crate::iat_hook;
use crate::untrusted::Untrusted;
use std::ffi::c_void;
use std::num::NonZero;
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;
use windows_sys::Win32::Foundation::{HMODULE, HWND, RECT};
use windows_sys::Win32::Globalization::MultiByteToWideChar;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, GWL_EXSTYLE, GWL_STYLE, HMENU, HWND_TOPMOST, SWP_FRAMECHANGED,
    SWP_NOOWNERZORDER, SWP_SHOWWINDOW, SetWindowLongA, SetWindowPos, SetWindowTextW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_POPUP, WS_SYSMENU, WS_VISIBLE,
};

iat_hook! {
    REAL_CREATEWINDOWEXA / real_create_window_ex_a : c"CreateWindowExA"
        as fn(
            dw_ex_style: u32,
            lp_class_name: *const u8,
            lp_window_name: *const u8,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            h_wnd_parent: HWND,
            h_menu: HMENU,
            h_instance: HMODULE,
            lp_param: *mut c_void,
        ) -> HWND;
}

static APPLIED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<State> = OnceLock::new();

/// Installation-time resolution of `WindowCfg`.
struct ResolvedWindowCfg {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    frame: WindowFrame,
    always_on_top: bool,
}

impl ResolvedWindowCfg {
    fn new(cfg: &WindowCfg, framebuffer: (u32, u32), mode: DisplayMode) -> Self {
        Self {
            x: cfg.x,
            y: cfg.y,
            width: cfg.width.map_or(framebuffer.0, NonZero::get),
            height: cfg.height.map_or(framebuffer.1, NonZero::get),
            frame: cfg.frame.unwrap_or(match mode {
                DisplayMode::Fullscreen => WindowFrame::Borderless,
                DisplayMode::Windowed => WindowFrame::Frameless,
            }),
            always_on_top: cfg.always_on_top,
        }
    }
}

struct State {
    framebuffer: (u32, u32),
    restyle: ResolvedWindowCfg,
}

pub(crate) unsafe fn install(
    host: HMODULE,
    restyle: &WindowCfg,
    framebuffer: (u32, u32),
    display_mode: DisplayMode,
) {
    unsafe {
        let state = State {
            framebuffer,
            restyle: ResolvedWindowCfg::new(restyle, framebuffer, display_mode),
        };
        _ = STATE.set(state);
        REAL_CREATEWINDOWEXA.install(host, hook_create_window_ex_a);
    }
}

unsafe extern "system" fn hook_create_window_ex_a(
    dw_ex_style: u32,
    lp_class_name: *const u8,
    lp_window_name: *const u8,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
    h_wnd_parent: HWND,
    h_menu: HMENU,
    h_instance: HMODULE,
    lp_param: *mut c_void,
) -> HWND {
    unsafe {
        // SAFETY: caller-controlled FFI pointers; tag as `Untrusted` for our inspection.
        // The raw `lp_class_name` / `lp_window_name` are forwarded verbatim to
        // `real_create_window_ex_a` below.
        let class_name = Untrusted::from_raw(lp_class_name);
        let window_name = Untrusted::from_raw(lp_window_name);

        // IME and sound-thread helpers also use this import, but we only want
        // the game's render window. th15 registers it under class "BASE" at `fcn.00472f50`.
        // We match by class name so we catch both the fullscreen (`WS_POPUP`)
        // and windowed (no `WS_POPUP`) branches.
        let is_main = !APPLIED.load(Ordering::Acquire)
            && h_wnd_parent.is_null()
            && class_name_matches(class_name, b"BASE");

        let (use_w, use_h) = if is_main && (dw_style & WS_POPUP) == 0 {
            let (bw, bh) = STATE.get().unwrap().framebuffer;
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: bw.cast_signed(),
                bottom: bh.cast_signed(),
            };
            AdjustWindowRectEx(&raw mut rc, dw_style, 0, dw_ex_style);
            (rc.right - rc.left, rc.bottom - rc.top)
        } else {
            (n_width, n_height)
        };
        // We log the configuration and recomputed dimensions
        // before the `CreateWindowExA` call in case there's a failure
        // or wrong-sized client area, so we can troubleshoot.
        if is_main {
            info!(
                kind = "create_window_call",
                dw_style = format_args!("{dw_style:#x}"),
                dw_ex_style = format_args!("{dw_ex_style:#x}"),
                x,
                y,
                width_in = n_width,
                height_in = n_height,
                width_out = use_w,
                height_out = use_h,
                recomputed = use_w != n_width || use_h != n_height,
            );
        }

        let hwnd = real_create_window_ex_a(
            dw_ex_style,
            lp_class_name,
            lp_window_name,
            dw_style,
            x,
            y,
            use_w,
            use_h,
            h_wnd_parent,
            h_menu,
            h_instance,
            lp_param,
        );

        if is_main
            && !hwnd.is_null()
            && APPLIED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            apply(hwnd, &STATE.get().unwrap().restyle, window_name);
        }
        hwnd
    }
}

// `CreateWindowExA`'s `lpClassName` is either a Win32 `ATOM` (16-bit integer
// in the pointer slot, < 0x10000) or a pointer to a null-terminated string.
// `ATOM` values land in the process's null-guard region of address space,
// so `safe_read` returns 0 bytes for them and the length check below
// rejects without a special case.
fn class_name_matches(p: Untrusted<u8>, expected: &[u8]) -> bool {
    let want_len = expected.len() + 1;
    let mut buf = [0u8; 32];
    if want_len > buf.len() {
        return false;
    }
    let n = p.safe_read(&mut buf[..want_len]);
    n == want_len && &buf[..expected.len()] == expected && buf[expected.len()] == 0
}

/// Reads the game's Shift-JIS title bytes, transcodes through `CP_SHIFT_JIS` to UTF-16,
/// and appends a version identifier for this project.
///
/// This is independent of locale because we use the literal Shift-JIS code page,
/// not the system ANSI code page.
fn build_extended_title(original: Untrusted<u8>) -> Vec<u16> {
    const CP_SHIFT_JIS: u32 = 932;
    const BUF_LEN: usize = 512;
    let mut buf = [0u8; BUF_LEN];
    let sjis = original.safe_read_until(&mut buf, 0);

    let mut wide = vec![0u16; sjis.len()];
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let in_len = sjis.len() as i32;
    let written = unsafe {
        MultiByteToWideChar(
            CP_SHIFT_JIS,
            0,
            sjis.as_ptr(),
            in_len,
            wide.as_mut_ptr(),
            in_len,
        )
    };
    wide.truncate(written.max(0).cast_unsigned() as usize);

    wide.extend(" + neopatch v".encode_utf16());
    wide.extend(env!("CARGO_PKG_VERSION").encode_utf16());
    wide.push(0);
    wide
}

unsafe fn apply(hwnd: HWND, cfg: &ResolvedWindowCfg, lp_window_name: Untrusted<u8>) {
    unsafe {
        // We do this before `SetWindowPos` so the `SWP_FRAMECHANGED`-driven
        // first paint of the title chrome gets the new UTF-16 title.
        let title = build_extended_title(lp_window_name);
        SetWindowTextW(hwnd, title.as_ptr());

        let style: WINDOW_STYLE = match cfg.frame {
            WindowFrame::Framed => {
                WS_OVERLAPPED
                    | WS_SYSMENU
                    | WS_VISIBLE
                    | WS_CAPTION
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX
            }
            WindowFrame::Frameless => {
                WS_OVERLAPPED | WS_SYSMENU | WS_VISIBLE | WS_MINIMIZEBOX | WS_MAXIMIZEBOX
            }
            WindowFrame::Borderless => WS_POPUP | WS_VISIBLE,
        };
        let ex_style: WINDOW_EX_STYLE = 0;
        SetWindowLongA(hwnd, GWL_STYLE, style.cast_signed());
        SetWindowLongA(hwnd, GWL_EXSTYLE, ex_style.cast_signed());

        let mut rc = RECT {
            left: 0,
            top: 0,
            right: cfg.width.cast_signed(),
            bottom: cfg.height.cast_signed(),
        };
        AdjustWindowRectEx(&raw mut rc, style, 0, ex_style);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;

        let after = if cfg.always_on_top {
            HWND_TOPMOST
        } else {
            null_mut()
        };
        SetWindowPos(
            hwnd,
            after,
            cfg.x,
            cfg.y,
            w,
            h,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW | SWP_NOOWNERZORDER,
        );
    }
}
