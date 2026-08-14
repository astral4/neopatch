//! Window setup and hooking.

use crate::ansi::{CP_SHIFT_JIS, to_wide};
use crate::config::{
    DisplayMode, DisplayModeExt, ResolutionConfig, ResolutionConfigExt, WindowCfg, WindowFrame,
};
use crate::iat_hook;
use crate::log::log_at;
use crate::untrusted::Untrusted;
use std::ffi::c_void;
use std::mem::zeroed;
use std::num::NonZero;
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};
use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{HMODULE, HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CW_USEDEFAULT, DefWindowProcW, GWL_EXSTYLE, GWL_STYLE, GetWindowLongA,
    GetWindowRect, HMENU, HWND_NOTOPMOST, HWND_TOPMOST, InternalGetWindowText, IsWindow,
    IsWindowVisible, PM_NOREMOVE, PeekMessageA, SWP_FRAMECHANGED, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongA, SetWindowPos,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETTEXT, WS_CAPTION, WS_CHILD, WS_CLIPSIBLINGS,
    WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_OVERLAPPEDWINDOW, WS_POPUP,
    WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
};

/// The longest stored window title (in UTF-16 units) that we read back out of a window.
const TITLE_READ_LEN: usize = 512;

static STATE: OnceLock<State> = OnceLock::new();
static MAIN_HWND: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// The window last styled by [`prep_main_window`].
static STYLED_WINDOW: AppliedTo = AppliedTo::new();

/// Records which window one piece of window setup was last applied to.
struct AppliedTo(AtomicPtr<c_void>);

impl AppliedTo {
    const fn new() -> Self {
        Self(AtomicPtr::new(null_mut()))
    }

    /// Whether this work was already done for `hwnd`.
    fn covers(&self, hwnd: HWND) -> bool {
        !hwnd.is_null() && self.0.load(Ordering::Acquire) == hwnd
    }

    /// Claims `hwnd` as done.
    fn claim(&self, hwnd: HWND) {
        self.0.store(hwnd, Ordering::Release);
    }
}

/// Returns the game's main render window, or null before the first creation.
pub(crate) fn main_hwnd() -> HWND {
    MAIN_HWND.load(Ordering::Acquire)
}

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

// The policy derivations for the shared game-config shapes live here rather than in `config`
// so the configuration module stays free of window types.
impl ResolutionConfig {
    /// The window policy for games whose fullscreen toggle is the core two-state `mode`:
    /// always restyle to the configured resolution.
    #[must_use]
    pub fn window_policy(&self, display_mode: DisplayMode) -> WindowPolicy {
        WindowPolicy::Restyle {
            framebuffer: self.resolution.dimensions(),
            display_mode,
        }
    }
}

impl ResolutionConfigExt {
    /// The window policy implied by the game's own display mode: borderless games manage their
    /// own window, so the core defers; otherwise restyle to the configured resolution.
    #[must_use]
    pub fn window_policy(&self) -> WindowPolicy {
        match self.display_mode {
            DisplayModeExt::Borderless => WindowPolicy::DeferToGame,
            DisplayModeExt::Windowed | DisplayModeExt::Fullscreen => WindowPolicy::Restyle {
                framebuffer: self.resolution.dimensions(),
                display_mode: self.display_mode.to_core(),
            },
        }
    }
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
    /// The configured outer position, or `None` to leave placement to the system.
    position: Option<(i32, i32)>,
    width: u32,
    height: u32,
    frame: WindowFrame,
    always_on_top: bool,
}

impl ResolvedWindowCfg {
    fn new(cfg: &WindowCfg, framebuffer: (u32, u32), mode: DisplayMode) -> Self {
        Self {
            // `CreateWindowEx` ignores `y` when `x` is `CW_USEDEFAULT`, so a half-specified position isn't sensible.
            position: match (cfg.x, cfg.y) {
                (None, None) => None,
                (x, y) => Some((x.unwrap_or(0), y.unwrap_or(0))),
            },
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
    Restyle { restyle: ResolvedWindowCfg },
    DeferToGame { always_on_top: bool },
}

/// Picks the IAT slot [`install`] hooks. th06-th18 use Ansi; th20 uses Wide.
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
    let state = match policy {
        WindowPolicy::Restyle {
            framebuffer,
            display_mode,
        } => State::Restyle {
            restyle: ResolvedWindowCfg::new(restyle, framebuffer, display_mode),
        },
        WindowPolicy::DeferToGame => State::DeferToGame {
            always_on_top: restyle.always_on_top,
        },
    };
    _ = STATE.set(state);
    match api {
        WindowApi::Ansi => unsafe {
            REAL_CREATEWINDOWEXA.install(host, hook_create_window_ex_a);
        },
        WindowApi::Wide => unsafe {
            REAL_CREATEWINDOWEXW.install(host, hook_create_window_ex_w);
        },
    }
}

/// Generates a `CreateWindowEx*` hook.
macro_rules! create_window_hook {
    ($name:ident, $char:ty, $class:expr, $real:ident, $build_title:path) => {
        unsafe extern "system" fn $name(
            dw_ex_style: u32,
            lp_class_name: *const $char,
            lp_window_name: *const $char,
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
            // IME and sound-thread helpers also use this import, but we only want the game's render window.
            // We match by class name "BASE" to catch both fullscreen (`WS_POPUP`) and windowed (no `WS_POPUP`) branches.
            let is_main = h_wnd_parent.is_null()
                && Untrusted::from_raw(lp_class_name).matches_nul_terminated($class);
            let requested = CreationArgs {
                x,
                y,
                width: n_width,
                height: n_height,
                style: dw_style,
                ex_style: dw_ex_style,
            };
            let args = prep_main_window(STATE.get().unwrap(), is_main, requested);
            if is_main {
                log_create_window_call(requested, args);
            }
            let hwnd = unsafe {
                $real(
                    args.ex_style,
                    lp_class_name,
                    lp_window_name,
                    args.style,
                    args.x,
                    args.y,
                    args.width,
                    args.height,
                    h_wnd_parent,
                    h_menu,
                    h_instance,
                    lp_param,
                )
            };
            finish_main_window(hwnd, is_main, || {
                $build_title(Untrusted::from_raw(lp_window_name))
            });
            hwnd
        }
    };
}

create_window_hook!(
    hook_create_window_ex_a,
    u8,
    b"BASE",
    real_create_window_ex_a,
    build_extended_title_from_sjis
);
create_window_hook!(
    hook_create_window_ex_w,
    u16,
    const { &[b'B' as u16, b'A' as u16, b'S' as u16, b'E' as u16] },
    real_create_window_ex_w,
    build_extended_title_from_wide
);

/// The positional and style arguments of a `CreateWindowEx*` call.
#[derive(Clone, Copy)]
struct CreationArgs {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
}

const fn frame_style(frame: WindowFrame) -> WINDOW_STYLE {
    // `CreateWindowEx*` force-adds `WS_CAPTION` to any window that is neither a child nor a popup,
    // so the captionless options must be popup-based.
    match frame {
        WindowFrame::Framed => {
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_VISIBLE
        }
        WindowFrame::Frameless => {
            WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_VISIBLE
        }
        WindowFrame::Borderless => WS_POPUP | WS_VISIBLE,
    }
}

fn topmost_ex_style(ex_style: u32, always_on_top: bool) -> WINDOW_EX_STYLE {
    if always_on_top {
        ex_style | WS_EX_TOPMOST
    } else {
        ex_style
    }
}

/// Computes the `CreateWindowEx*` arguments for the main render window.
fn prep_main_window(state: &State, is_main: bool, requested: CreationArgs) -> CreationArgs {
    if !is_main {
        return requested;
    }

    let always_on_top = match state {
        State::Restyle { restyle } => restyle.always_on_top,
        State::DeferToGame { always_on_top } => *always_on_top,
    };

    match state {
        State::Restyle { restyle } if (requested.style & WS_POPUP) == 0 => {
            let style = frame_style(restyle.frame);
            let ex_style = topmost_ex_style(0, always_on_top);
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: restyle.width.cast_signed(),
                bottom: restyle.height.cast_signed(),
            };
            unsafe { AdjustWindowRectEx(&raw mut rc, style, 0, ex_style) };

            let (x, y) = restyle.position.unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
            CreationArgs {
                x,
                y,
                width: rc.right - rc.left,
                height: rc.bottom - rc.top,
                style,
                ex_style,
            }
        }
        _ => CreationArgs {
            ex_style: topmost_ex_style(requested.ex_style, always_on_top),
            ..requested
        },
    }
}

fn log_create_window_call(requested: CreationArgs, args: CreationArgs) {
    info!(
        kind = "create_window_call",
        style_in = format_args!("{:#x}", requested.style),
        ex_style_in = format_args!("{:#x}", requested.ex_style),
        x_in = requested.x,
        y_in = requested.y,
        width_in = requested.width,
        height_in = requested.height,
        style_out = format_args!("{:#x}", args.style),
        ex_style_out = format_args!("{:#x}", args.ex_style),
        x_out = args.x,
        y_out = args.y,
        width_out = args.width,
        height_out = args.height,
    );
}

// Does post-creation bookkeeping for the main render window. This runs on first creation and again on every recreation.
fn finish_main_window(hwnd: HWND, is_main: bool, build_title: impl FnOnce() -> Option<Vec<u16>>) {
    if !is_main {
        return;
    }
    if hwnd.is_null() {
        warn!(kind = "main_window_create_failed");
        return;
    }

    MAIN_HWND.store(hwnd, Ordering::Release);

    if let Some(title) = build_title() {
        unsafe { set_window_text_lossless(hwnd, &title) };
    } else {
        warn!(kind = "window_title_not_converted");
    }

    STYLED_WINDOW.claim(hwnd);
    log_created_style(hwnd);
}

/// The style bits that can be affected by our window policies. Everything outside this mask stays as whatever the live window has.
const POLICY_STYLE_MASK: WINDOW_STYLE = (frame_style(WindowFrame::Framed)
    | frame_style(WindowFrame::Frameless)
    | frame_style(WindowFrame::Borderless)
    | WS_THICKFRAME)
    & !WS_VISIBLE;

// th06–th10 create windows with `WS_OVERLAPPEDWINDOW`. The policy must be able to strip every bit of it that a frame doesn't re-add,
// or else device-time restyling leaves a resizable border behind.
const _: () = assert!(WS_OVERLAPPEDWINDOW & !WS_VISIBLE & !POLICY_STYLE_MASK == 0);
// Conversely, bits maintained by the system have to stay outside the mask since a restyle sources everything outside it from the live window.
// The system re-adds them after a plain `SetWindowLong`, so a mask claiming one would leave the restyle plan permanently unsatisfied.
const _: () = assert!(POLICY_STYLE_MASK & WS_CLIPSIBLINGS == 0);

struct RestylePlan {
    /// The full replacement style, or `None` to leave the live one alone.
    style: Option<WINDOW_STYLE>,
    /// The topmost band to move into, or `None` to stay in the one the window is already in.
    band: Option<bool>,
    /// The position to change to, or `None` to keep the position already chosen by the system.
    position: Option<(i32, i32)>,
    /// The window size to change to, or `None` to keep the current size.
    size: Option<(i32, i32)>,
}

/// Returns the window properties to be applied by [`restyle_device_window`], or `None` when the live window already satisfies the policy.
fn restyle_plan(state: &State, live: CreationArgs) -> Option<RestylePlan> {
    let args = prep_main_window(state, true, live);

    let styled = (live.style & !POLICY_STYLE_MASK) | (args.style & POLICY_STYLE_MASK);
    let style = (styled != live.style).then_some(styled);
    let wants_topmost = args.ex_style & WS_EX_TOPMOST != 0;
    let band = (wants_topmost != (live.ex_style & WS_EX_TOPMOST != 0)).then_some(wants_topmost);
    let position = (args.x != CW_USEDEFAULT && (args.x, args.y) != (live.x, live.y))
        .then_some((args.x, args.y));
    let size = ((args.width, args.height) != (live.width, live.height))
        .then_some((args.width, args.height));

    let satisfied = style.is_none() && band.is_none() && position.is_none() && size.is_none();
    (!satisfied).then_some(RestylePlan {
        style,
        band,
        position,
        size,
    })
}

/// Applies the window policy at device-creation time to a window that wasn't covered by the creation hook.
///
/// [`install`]'s import hook gets the window created in its final state without needing to restyle. This is another path in case that slot
/// was rebound by something that initialized after our `DllMain`. This path has the same effect after `SWP_FRAMECHANGED` recomputes the frame.
///
/// Frame changes are only applied to unmapped windows, since restyling a mapped one desyncs the window manager's frame from the Win32 rect.
/// So, visible windows get hidden for the restyle and stay hidden until the returned [`DeviceWindow`] is revealed.
#[must_use]
pub(crate) fn restyle_device_window(hwnd: HWND) -> DeviceWindow {
    let untouched = DeviceWindow {
        hwnd,
        hidden_for_restyle: false,
    };

    if hwnd.is_null() {
        return untouched;
    }
    if !is_top_level(hwnd) {
        warn!(kind = "device_window_skipped", reason = "not_top_level");
        return untouched;
    }

    let recorded = MAIN_HWND.load(Ordering::Acquire);
    if recorded.is_null() || unsafe { IsWindow(recorded) } == 0 {
        MAIN_HWND.store(hwnd, Ordering::Release);
    }

    if STYLED_WINDOW.covers(hwnd) {
        debug!(
            kind = "window_restyle_not_needed",
            reason = "styled_at_creation"
        );
        return untouched;
    }
    let Some(hidden_for_restyle) = apply_window_policy(hwnd) else {
        return untouched;
    };
    STYLED_WINDOW.claim(hwnd);
    DeviceWindow {
        hwnd,
        hidden_for_restyle,
    }
}

/// Reads the live window, plans the policy against it, and applies the result.
///
/// `Some` reports whether the window had to be hidden for a frame change, which only a reveal may undo.
/// `None` means the policy could not be evaluated at all, so the window should not be claimed.
fn apply_window_policy(hwnd: HWND) -> Option<bool> {
    let state = STATE.get()?;
    let Some(rc) = window_rect(hwnd) else {
        warn!(kind = "window_restyle_skipped", reason = "rect_unreadable");
        return None;
    };
    let live = CreationArgs {
        x: rc.left,
        y: rc.top,
        width: rc.right - rc.left,
        height: rc.bottom - rc.top,
        style: unsafe { GetWindowLongA(hwnd, GWL_STYLE) }.cast_unsigned(),
        ex_style: unsafe { GetWindowLongA(hwnd, GWL_EXSTYLE) }.cast_unsigned(),
    };

    let Some(plan) = restyle_plan(state, live) else {
        // The policy was evaluated and the window already satisfies it, so this counts as applied.
        debug!(kind = "window_restyle_not_needed");
        return Some(false);
    };

    let frame_changed = plan.style.is_some();
    let hide = frame_changed && live.style & WS_VISIBLE != 0;
    if hide {
        unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            );
        }
    }

    if let Some(style) = plan.style {
        let style = if hide { style & !WS_VISIBLE } else { style };
        unsafe {
            SetWindowLongA(hwnd, GWL_STYLE, style.cast_signed());
        }
    }

    let (x, y) = plan.position.unwrap_or((0, 0));
    let (width, height) = plan.size.unwrap_or((0, 0));

    let mut flags = SWP_NOACTIVATE;
    if frame_changed {
        flags |= SWP_FRAMECHANGED;
    }
    if plan.position.is_none() {
        flags |= SWP_NOMOVE;
    }
    if plan.size.is_none() {
        flags |= SWP_NOSIZE;
    }
    // Only `SetWindowPos` with `HWND_TOPMOST`/`HWND_NOTOPMOST` actually moves a window across the topmost band boundary.
    let insert_after = match plan.band {
        Some(true) => HWND_TOPMOST,
        Some(false) => HWND_NOTOPMOST,
        None => {
            flags |= SWP_NOZORDER;
            null_mut()
        }
    };

    let ok = unsafe { SetWindowPos(hwnd, insert_after, x, y, width, height, flags) } != 0;

    let (style_now, ex_style_now, rect) = window_snapshot(hwnd);
    log_at!(ok => info / warn,
        kind = "window_restyled_late",
        style_in = format_args!("{:#x}", live.style),
        ex_style_in = format_args!("{:#x}", live.ex_style),
        x_in = live.x,
        y_in = live.y,
        width_in = live.width,
        height_in = live.height,
        style_out = format_args!("{:#x}", plan.style.unwrap_or(live.style)),
        x_out = plan.position.map_or(live.x, |(x, _)| x),
        y_out = plan.position.map_or(live.y, |(_, y)| y),
        width_out = plan.size.map_or(live.width, |(w, _)| w),
        height_out = plan.size.map_or(live.height, |(_, h)| h),
        zorder = match plan.band {
            Some(true) => "TOPMOST",
            Some(false) => "NOTOPMOST",
            None => "KEPT",
        },
        hidden_for_restyle = hide,
        ok,
        style_now = format_args!("{style_now:#x}"),
        ex_style_now = format_args!("{ex_style_now:#x}"),
        rect,
    );

    Some(hide)
}

/// The device's target window.
pub(crate) struct DeviceWindow {
    hwnd: HWND,
    hidden_for_restyle: bool,
}

impl DeviceWindow {
    /// Puts the device window on screen after a successful `CreateDeviceEx`.
    pub(crate) fn reveal(&self) {
        if self.hidden_for_restyle {
            settle_before_remap();
        }
        self.show();
    }

    /// Undoes the restyle's window hide after a failed `CreateDeviceEx`.
    pub(crate) fn restore(&self) {
        if self.hidden_for_restyle {
            settle_before_remap();
            self.show();
        }
    }

    /// Maps the window, taking activation only on its first appearance.
    fn show(&self) {
        if self.hwnd.is_null() {
            return;
        }

        let mut flags = SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_SHOWWINDOW;
        // An already-visible window is left alone so focus isn't stolen mid-session.
        let was_visible = self.hidden_for_restyle || unsafe { IsWindowVisible(self.hwnd) } != 0;
        if was_visible {
            flags |= SWP_NOACTIVATE;
        }

        let ok = unsafe { SetWindowPos(self.hwnd, null_mut(), 0, 0, 0, 0, flags) } != 0;
        log_at!(was_visible => debug / info,
            kind = "device_window_shown",
            was_visible,
            ok,
        );

        settle_after_remap();
    }
}

fn settle_before_remap() {
    let pending = peek_pending();
    debug!(kind = "window_settled_before_remap", pending);
}

fn settle_after_remap() {
    let pending = peek_pending();
    info!(kind = "window_settled_after_remap", pending);
}

/// Peeks at the thread's message queue. Returns whether anything is pending.
fn peek_pending() -> bool {
    let mut msg = unsafe { zeroed() };
    unsafe { PeekMessageA(&raw mut msg, null_mut(), 0, 0, PM_NOREMOVE) != 0 }
}

/// Returns the window's outer rect, or `None` when the handle can't be read.
fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rc = unsafe { zeroed() };
    (unsafe { GetWindowRect(hwnd, &raw mut rc) } != 0).then_some(rc)
}

/// Gets the live style, ex-style, and outer rect of `hwnd` for logging.
fn window_snapshot(hwnd: HWND) -> (WINDOW_STYLE, WINDOW_EX_STYLE, String) {
    let style = unsafe { GetWindowLongA(hwnd, GWL_STYLE) }.cast_unsigned();
    let ex_style = unsafe { GetWindowLongA(hwnd, GWL_EXSTYLE) }.cast_unsigned();
    let rect = window_rect(hwnd).map_or_else(
        || String::from("<unavailable>"),
        |rc| {
            format!(
                "{}x{} at ({},{})",
                rc.right - rc.left,
                rc.bottom - rc.top,
                rc.left,
                rc.top
            )
        },
    );
    (style, ex_style, rect)
}

fn log_created_style(hwnd: HWND) {
    let (style, ex_style, rect) = window_snapshot(hwnd);
    info!(
        kind = "window_created",
        style = format_args!("{style:#x}"),
        ex_style = format_args!("{ex_style:#x}"),
        rect,
    );
}

/// Sets the window title losslessly, bypassing the ANSI message thunk.
///
/// `SetWindowTextW` delivers `WM_SETTEXT` through the target window's procedure.
/// For an ANSI window, the text is converted from UTF-16 to ANSI for the game's procedure and back for storage.
/// Both go through the system ANSI code page, which mangles Japanese text on non-Japanese locales.
unsafe fn set_window_text_lossless(hwnd: HWND, title: &[u16]) {
    // We assume the games' window procedures ignore `WM_SETTEXT`.
    unsafe {
        DefWindowProcW(
            hwnd,
            WM_SETTEXT,
            0,
            title.as_ptr().expose_provenance().cast_signed(),
        );
    }
    // `InternalGetWindowText` reads the internal Unicode buffer directly, bypassing the lossy `WM_GETTEXT` ANSI thunk.
    let mut stored = [0; TITLE_READ_LEN];
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n = unsafe { InternalGetWindowText(hwnd, stored.as_mut_ptr(), stored.len() as i32) };
    info!(
        kind = "window_title_set",
        title = %String::from_utf16_lossy(&stored[..n.max(0).cast_unsigned() as usize]),
    );
}

/// Returns whether `hwnd` is a live top-level window.
fn is_top_level(hwnd: HWND) -> bool {
    if unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    unsafe { GetWindowLongA(hwnd, GWL_STYLE) }.cast_unsigned() & WS_CHILD == 0
}

/// Reads the game's Shift-JIS title bytes, transcodes through `CP_SHIFT_JIS` to UTF-16, and appends a version identifier for this project.
/// Returns `None` if the title is unreadable or cannot be converted.
///
/// This is independent of locale because we use the literal Shift-JIS code page, not the system ANSI code page.
fn build_extended_title_from_sjis(original: Untrusted<u8>) -> Option<Vec<u16>> {
    const BUF_LEN: usize = 512;

    let mut buf = [0u8; BUF_LEN];
    let sjis = original.safe_read_until(&mut buf, 0);
    let mut wide = to_wide(CP_SHIFT_JIS, sjis, false)?;
    wide.pop();
    append_suffix(&mut wide);
    Some(wide)
}

/// Reads the game's UTF-16 title bytes and appends a version identifier for this project. Returns `None` if the title is empty.
fn build_extended_title_from_wide(original: Untrusted<u16>) -> Option<Vec<u16>> {
    const BUF_LEN: usize = 512;
    let mut buf = [0u16; BUF_LEN];
    let read = original.safe_read_until(&mut buf, 0);
    if read.is_empty() {
        return None;
    }
    let mut wide = read.to_vec();
    append_suffix(&mut wide);
    Some(wide)
}

fn append_suffix(wide: &mut Vec<u16>) {
    wide.extend(" + neopatch v".encode_utf16());
    wide.extend(env!("CARGO_PKG_VERSION").encode_utf16());
    wide.push(0);
}

#[cfg(test)]
mod tests {
    use super::{
        CreationArgs, ResolvedWindowCfg, State, WindowPolicy, frame_style, prep_main_window,
        restyle_plan, topmost_ex_style,
    };
    use crate::config::{
        DisplayMode, DisplayModeExt, Resolution, ResolutionConfig, ResolutionConfigExt, WindowCfg,
        WindowFrame,
    };
    use std::num::NonZero;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CW_USEDEFAULT, WS_CAPTION, WS_CLIPSIBLINGS, WS_EX_TOPMOST, WS_EX_WINDOWEDGE,
        WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };

    #[test]
    fn window_policy_borderless() {
        let game = ResolutionConfigExt {
            display_mode: DisplayModeExt::Borderless,
            resolution: Resolution::R960x720,
        };
        assert!(matches!(game.window_policy(), WindowPolicy::DeferToGame));

        for (mode, core_mode) in [
            (DisplayModeExt::Windowed, DisplayMode::Windowed),
            (DisplayModeExt::Fullscreen, DisplayMode::Fullscreen),
        ] {
            let game = ResolutionConfigExt {
                display_mode: mode,
                resolution: Resolution::R960x720,
            };
            assert!(
                matches!(
                    game.window_policy(),
                    WindowPolicy::Restyle {
                        framebuffer: (960, 720),
                        display_mode,
                    } if display_mode == core_mode
                ),
                "{mode:?}"
            );
        }

        let game = ResolutionConfig {
            resolution: Resolution::R640x480,
        };
        assert!(matches!(
            game.window_policy(DisplayMode::Fullscreen),
            WindowPolicy::Restyle {
                framebuffer: (640, 480),
                display_mode: DisplayMode::Fullscreen,
            }
        ));
    }

    #[test]
    fn unmodified_frame_creation_style() {
        for frame in [
            WindowFrame::Framed,
            WindowFrame::Frameless,
            WindowFrame::Borderless,
        ] {
            let style = frame_style(frame);
            assert_ne!(style & WS_VISIBLE, 0, "{frame:?} must be created visible");
            let captioned = style & WS_CAPTION == WS_CAPTION;
            let popup = style & WS_POPUP != 0;
            assert!(captioned != popup, "{frame:?} must be captioned xor popup");
        }
    }

    #[test]
    fn ex_style_topmost_modification() {
        assert_eq!(topmost_ex_style(0, false), 0);
        assert_eq!(topmost_ex_style(0, true), WS_EX_TOPMOST);
        assert_eq!(topmost_ex_style(0x40000, true), 0x40000 | WS_EX_TOPMOST);
        assert_eq!(topmost_ex_style(0x40000, false), 0x40000);
    }

    fn restyle_state(frame: WindowFrame, always_on_top: bool) -> State {
        State::Restyle {
            restyle: ResolvedWindowCfg {
                position: Some((100, 120)),
                width: 640,
                height: 480,
                frame,
                always_on_top,
            },
        }
    }

    fn requested(style: u32) -> CreationArgs {
        CreationArgs {
            x: 11,
            y: 22,
            width: 646,
            height: 512,
            style,
            ex_style: 0x40000,
        }
    }

    #[test]
    fn non_main_windows_not_rewritten() {
        // The game's own request must survive untouched for any window that is not the render window;
        // the IME and sound helper windows come through the same hook.
        let state = restyle_state(WindowFrame::Framed, true);
        let req = requested(0x100a_0000);
        let got = prep_main_window(&state, false, req);
        assert_eq!((got.x, got.y), (req.x, req.y));
        assert_eq!((got.width, got.height), (req.width, req.height));
        assert_eq!((got.style, got.ex_style), (req.style, req.ex_style));
    }

    #[test]
    fn restyle_rewrite_args() {
        let state = restyle_state(WindowFrame::Framed, false);
        let got = prep_main_window(&state, true, requested(0x100a_0000));
        assert_eq!((got.x, got.y), (100, 120));
        assert_eq!(got.style, frame_style(WindowFrame::Framed));
        assert_eq!(got.ex_style, 0);
        assert!(got.width >= 640 && got.height > 480);
    }

    #[test]
    fn fullscreen_style() {
        let req = requested(WS_POPUP | WS_VISIBLE);
        let state = restyle_state(WindowFrame::Framed, true);
        let got = prep_main_window(&state, true, req);
        assert_eq!((got.x, got.y), (req.x, req.y));
        assert_eq!((got.width, got.height), (req.width, req.height));
        assert_eq!(got.style, req.style, "the game's style is kept");
        assert_eq!(got.ex_style, req.ex_style | WS_EX_TOPMOST);
    }

    #[test]
    fn defer_to_game_style() {
        let req = requested(0x1000_0000);
        for (always_on_top, expected_ex) in
            [(false, req.ex_style), (true, req.ex_style | WS_EX_TOPMOST)]
        {
            let state = State::DeferToGame { always_on_top };
            let got = prep_main_window(&state, true, req);
            assert_eq!((got.width, got.height), (req.width, req.height));
            assert_eq!(got.style, req.style);
            assert_eq!(got.ex_style, expected_ex);
        }
    }

    #[test]
    fn unset_position_and_borderless_outer_size() {
        let state = State::Restyle {
            restyle: ResolvedWindowCfg {
                position: None,
                width: 640,
                height: 480,
                frame: WindowFrame::Borderless,
                always_on_top: false,
            },
        };
        let got = prep_main_window(&state, true, requested(0x100a_0000));
        assert_eq!((got.x, got.y), (CW_USEDEFAULT, CW_USEDEFAULT));
        assert_eq!((got.width, got.height), (640, 480));
    }

    #[test]
    fn restyle_plan_for_displaced_hook() {
        let live = CreationArgs {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            style: WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
            ex_style: WS_EX_WINDOWEDGE,
        };

        let state = restyle_state(WindowFrame::Framed, false);
        let plan = restyle_plan(&state, live).expect("policy differs from the game's window");
        // The policy's frame bits replace the game's, while bits outside the mask survive.
        assert_eq!(
            plan.style,
            Some(WS_CLIPSIBLINGS | (frame_style(WindowFrame::Framed) & !WS_VISIBLE))
        );
        assert_eq!(plan.band, None);
        assert_eq!(plan.position, Some((100, 120)));
        let (w, h) = plan
            .size
            .expect("frame changed, so the outer rect for 640x480 changed");
        assert!(w >= 640 && h > 480);

        let applied = CreationArgs {
            x: 100,
            y: 120,
            width: w,
            height: h,
            style: plan.style.expect("the frame changed") | WS_VISIBLE,
            // The restyle never writes the ex-style, so the live one carries over untouched.
            ex_style: live.ex_style,
        };
        assert!(restyle_plan(&state, applied).is_none());
    }

    #[test]
    fn restyle_plan_skip_satisfied_window() {
        let live = CreationArgs {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            style: WS_POPUP | WS_VISIBLE | WS_CLIPSIBLINGS,
            ex_style: 0,
        };
        assert!(restyle_plan(&restyle_state(WindowFrame::Framed, false), live).is_none());
        assert!(
            restyle_plan(
                &State::DeferToGame {
                    always_on_top: false
                },
                live
            )
            .is_none()
        );

        let state = State::DeferToGame {
            always_on_top: true,
        };
        let plan = restyle_plan(&state, live).expect("topmost band not entered yet");
        assert_eq!(plan.style, None);
        assert_eq!(plan.band, Some(true));
        assert!(plan.position.is_none() && plan.size.is_none());
        let applied = CreationArgs {
            ex_style: WS_EX_TOPMOST,
            ..live
        };
        assert!(restyle_plan(&state, applied).is_none());
    }

    #[test]
    fn restyle_plan_demote_topmost() {
        let live = CreationArgs {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            style: WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
            ex_style: WS_EX_TOPMOST,
        };
        let state = restyle_state(WindowFrame::Framed, false);
        let plan = restyle_plan(&state, live).expect("frame and band both differ");
        assert_eq!(plan.band, Some(false));

        // The passthrough arm keeps the game's ex-style, so a popup fullscreen window is never demoted.
        let popup = CreationArgs {
            style: WS_POPUP | WS_VISIBLE | WS_CLIPSIBLINGS,
            ..live
        };
        assert!(restyle_plan(&state, popup).is_none());
    }

    #[test]
    fn restyle_plan_keep_system_position() {
        let state = State::Restyle {
            restyle: ResolvedWindowCfg {
                position: None,
                width: 640,
                height: 480,
                frame: WindowFrame::Borderless,
                always_on_top: false,
            },
        };
        let live = CreationArgs {
            x: 32,
            y: 64,
            width: 646,
            height: 512,
            style: WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
            ex_style: 0,
        };
        let plan = restyle_plan(&state, live).expect("frame and size differ");
        assert_eq!(plan.position, None);
        assert_eq!(plan.size, Some((640, 480)));
        assert_eq!(plan.style, Some(WS_CLIPSIBLINGS | WS_POPUP));
        assert_eq!(plan.band, None);
    }

    #[test]
    fn position_pair() {
        let base = WindowCfg {
            x: None,
            y: None,
            width: None,
            height: None,
            frame: None,
            always_on_top: false,
        };
        let resolve = |x, y| {
            ResolvedWindowCfg::new(
                &WindowCfg { x, y, ..base },
                (640, 480),
                DisplayMode::Windowed,
            )
            .position
        };
        assert_eq!(resolve(None, None), None);
        assert_eq!(resolve(Some(10), Some(20)), Some((10, 20)));
        assert_eq!(resolve(Some(10), None), Some((10, 0)));
        assert_eq!(resolve(None, Some(20)), Some((0, 20)));
        assert_eq!(resolve(Some(0), Some(0)), Some((0, 0)));
    }

    #[test]
    fn unset_size_fallback() {
        let cfg = WindowCfg {
            x: None,
            y: None,
            width: None,
            height: NonZero::new(720),
            frame: Some(WindowFrame::Borderless),
            always_on_top: false,
        };
        let resolved = ResolvedWindowCfg::new(&cfg, (1280, 960), DisplayMode::Windowed);
        assert_eq!((resolved.width, resolved.height), (1280, 720));
    }
}
