//! Window setup and hooking.

use crate::config::{DisplayMode, WindowCfg, WindowFrame};
use crate::iat_hook;
use crate::untrusted::Untrusted;
use std::ffi::c_void;
use std::num::NonZero;
use std::ptr::null_mut;
use std::sync::OnceLock;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{HMODULE, HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, DefWindowProcW, GWL_EXSTYLE, GWL_STYLE, GetWindowLongA, HMENU,
    HWND_TOPMOST, InternalGetWindowText, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowLongA, SetWindowPos, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_SETTEXT, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP,
    WS_SYSMENU, WS_VISIBLE,
};

static STATE: OnceLock<State> = OnceLock::new();

/// Determines what [`install`] does with the game's render window.
#[derive(Clone, Copy)]
pub enum WindowPolicy {
    /// Rewrites geometry/style/title/Z-order for non-`WS_POPUP` window creations, and only title/Z-order for `WS_POPUP` creations.
    Restyle {
        /// The target size.
        framebuffer: (u32, u32),
        /// Determines the default frame used when `[window]` leaves it unset.
        display_mode: DisplayMode,
    },
    /// Rewrites only title/Z-order.
    DeferToGame,
}

iat_hook! {
    REAL_CREATEWINDOWEXA / real_create_window_ex_a : "CreateWindowExA"
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
iat_hook! {
    REAL_CREATEWINDOWEXW / real_create_window_ex_w : "CreateWindowExW"
        as fn(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
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

/// The resolved form of [`WindowPolicy`].
enum State {
    Restyle {
        framebuffer: (u32, u32),
        restyle: ResolvedWindowCfg,
    },
    DeferToGame {
        always_on_top: bool,
    },
}

/// Picks the IAT slot [`install`] hooks. th10-th18 use Ansi; th20 uses Wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowApi {
    Ansi,
    Wide,
}

/// Caches the resolved [`WindowCfg`] and IAT-hooks `CreateWindowExA` / `CreateWindowExW` based on `api`.
/// The hook acts on the game's main render window (class `"BASE"`) based on [`WindowPolicy`].
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE, restyle: &WindowCfg, policy: WindowPolicy, api: WindowApi) {
    unsafe {
        let state = match policy {
            WindowPolicy::Restyle {
                framebuffer,
                display_mode,
            } => State::Restyle {
                framebuffer,
                restyle: ResolvedWindowCfg::new(restyle, framebuffer, display_mode),
            },
            WindowPolicy::DeferToGame => State::DeferToGame {
                always_on_top: restyle.always_on_top,
            },
        };
        _ = STATE.set(state);
        match api {
            WindowApi::Ansi => {
                REAL_CREATEWINDOWEXA.install(host, hook_create_window_ex_a);
            }
            WindowApi::Wide => {
                REAL_CREATEWINDOWEXW.install(host, hook_create_window_ex_w);
            }
        }
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
        // IME and sound-thread helpers also use this import, but we only want the game's render window.
        // We match by class name "BASE" to catch both fullscreen (`WS_POPUP`) and windowed (no `WS_POPUP`) branches.
        let is_main = h_wnd_parent.is_null()
            && Untrusted::from_raw(lp_class_name).matches_nul_terminated(b"BASE");
        let (use_w, use_h) =
            prep_main_window(is_main, dw_ex_style, dw_style, x, y, n_width, n_height);
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
        finish_main_window(hwnd, is_main, || {
            build_extended_title_from_sjis(Untrusted::from_raw(lp_window_name))
        });
        hwnd
    }
}

unsafe extern "system" fn hook_create_window_ex_w(
    dw_ex_style: u32,
    lp_class_name: *const u16,
    lp_window_name: *const u16,
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
    const BASE_CLASS_W: [u16; 4] = [b'B' as u16, b'A' as u16, b'S' as u16, b'E' as u16];

    unsafe {
        let is_main = h_wnd_parent.is_null()
            && Untrusted::from_raw(lp_class_name).matches_nul_terminated(&BASE_CLASS_W);
        let (use_w, use_h) =
            prep_main_window(is_main, dw_ex_style, dw_style, x, y, n_width, n_height);
        let hwnd = real_create_window_ex_w(
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
        finish_main_window(hwnd, is_main, || {
            build_extended_title_from_wide(Untrusted::from_raw(lp_window_name))
        });
        hwnd
    }
}

/// Geometry for the main render window. Returns the `(width, height)` to pass to `CreateWindowEx*`.
/// This is the framebuffer size under [`State::Restyle`] adjusted for the frame, else the game's requested size.
fn prep_main_window(
    is_main: bool,
    dw_ex_style: u32,
    dw_style: u32,
    x: i32,
    y: i32,
    n_width: i32,
    n_height: i32,
) -> (i32, i32) {
    let (use_w, use_h) = if let (true, State::Restyle { framebuffer, .. }) =
        (is_main, STATE.get().unwrap())
        && (dw_style & WS_POPUP) == 0
    {
        let (bw, bh) = *framebuffer;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: bw.cast_signed(),
            bottom: bh.cast_signed(),
        };
        unsafe { AdjustWindowRectEx(&raw mut rc, dw_style, 0, dw_ex_style) };
        (rc.right - rc.left, rc.bottom - rc.top)
    } else {
        (n_width, n_height)
    };
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
    (use_w, use_h)
}

// Post-creation handling for the main render window.
// This runs on first creation and again on every recreation, with each creation styled independently.
fn finish_main_window(hwnd: HWND, is_main: bool, build_title: impl FnOnce() -> Vec<u16>) {
    if !is_main {
        return;
    }
    if hwnd.is_null() {
        warn!(kind = "main_window_create_failed");
        return;
    }

    let title = build_title();
    match STATE.get().unwrap() {
        State::Restyle { restyle, .. } => unsafe {
            if GetWindowLongA(hwnd, GWL_STYLE).cast_unsigned() & WS_POPUP == 0 {
                apply(hwnd, restyle, &title);
            } else {
                apply_deferred(hwnd, restyle.always_on_top, &title);
            }
        },
        State::DeferToGame { always_on_top } => unsafe {
            apply_deferred(hwnd, *always_on_top, &title);
        },
    }
}

/// Sets the window title losslessly, bypassing the ANSI message thunk.
///
/// `SetWindowTextW` delivers `WM_SETTEXT` through the target window's procedure.
/// For an ANSI window, the text is converted from UTF-16 to ANSI for the game's procedure and back for storage.
/// Both go through the system ANSI code page, which mangles Japanese text on non-Japanese locales.
unsafe fn set_window_text_lossless(hwnd: HWND, title: &[u16]) {
    unsafe {
        // We assume the games' window procedures ignore `WM_SETTEXT`.
        DefWindowProcW(
            hwnd,
            WM_SETTEXT,
            0,
            title.as_ptr().expose_provenance().cast_signed(),
        );
        // `InternalGetWindowText` reads the internal Unicode buffer directly — the same source the
        // title bar and taskbar render from — bypassing the lossy `WM_GETTEXT` ANSI thunk.
        let mut stored = [0u16; 128];
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let n = InternalGetWindowText(hwnd, stored.as_mut_ptr(), stored.len() as i32);
        info!(
            kind = "window_title_set",
            title = %String::from_utf16_lossy(&stored[..n.max(0).cast_unsigned() as usize]),
        );
    }
}

/// [`apply`] without geometry/style modifications.
unsafe fn apply_deferred(hwnd: HWND, always_on_top: bool, title: &[u16]) {
    unsafe {
        set_window_text_lossless(hwnd, title);
        if always_on_top {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }
}

/// Reads the game's Shift-JIS title bytes, transcodes through `CP_SHIFT_JIS` to UTF-16, and appends a version identifier for this project.
///
/// This is independent of locale because we use the literal Shift-JIS code page, not the system ANSI code page.
fn build_extended_title_from_sjis(original: Untrusted<u8>) -> Vec<u16> {
    const BUF_LEN: usize = 512;

    let mut buf = [0u8; BUF_LEN];
    let sjis = original.safe_read_until(&mut buf, 0);

    // Lenient conversion, like the system thunk would be: an empty or unconvertible title
    // degrades to just the suffix.
    let mut wide = crate::ansi::to_wide(crate::ansi::CP_SHIFT_JIS, sjis, false).unwrap_or_default();
    // Drop `to_wide`'s NUL terminator; `append_suffix` re-terminates after the suffix.
    wide.pop();
    append_suffix(&mut wide);
    wide
}

/// Reads the game's UTF-16 title bytes and appends a version identifier for this project.
fn build_extended_title_from_wide(original: Untrusted<u16>) -> Vec<u16> {
    const BUF_LEN: usize = 512;
    let mut buf = [0u16; BUF_LEN];
    let mut wide = original.safe_read_until(&mut buf, 0).to_vec();
    append_suffix(&mut wide);
    wide
}

fn append_suffix(wide: &mut Vec<u16>) {
    wide.extend(" + neopatch v".encode_utf16());
    wide.extend(env!("CARGO_PKG_VERSION").encode_utf16());
    wide.push(0);
}

fn apply(hwnd: HWND, cfg: &ResolvedWindowCfg, title: &[u16]) {
    unsafe {
        // We do this before `SetWindowPos` so the `SWP_FRAMECHANGED`-driven first paint of the title bar gets the new UTF-16 title.
        set_window_text_lossless(hwnd, title);

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
