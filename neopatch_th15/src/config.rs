//! UTF-8 INI parser for the configuration file.
//!
//! Parsed once at DllMain. Silent on error:
//! unknown sections/keys/malformed values fall back to documented defaults.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::level_filters::LevelFilter;

const DEFAULT_GAME_FPS: u32 = 60;
const DEFAULT_REPLAY_SKIP_FPS: u32 = 240;
const DEFAULT_REPLAY_SLOW_FPS: u32 = 30;
const DEFAULT_SESSIONS_TO_KEEP: u32 = 10;

pub(crate) static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Config {
    pub(crate) display: DisplayCfg,
    pub(crate) window: WindowCfg,
    pub(crate) framerate: FramerateCfg,
    pub(crate) process: ProcessCfg,
    pub(crate) log: LogCfg,
}

pub(crate) struct DisplayCfg {
    pub(crate) mode: DisplayMode,
    /// Ignored in windowed mode.
    pub(crate) refresh_rate: RefreshRateMode,
    /// Limited to the three values the game itself supports natively.
    pub(crate) resolution: Resolution,
}

// Window dimensions and chrome default to `[display]`-derived values
// to stay close to the game's own choices: matching framebuffer dimensions;
// `Borderless` in fullscreen, `Frameless` in windowed. Set explicitly to override.
// We use `NonZero` for the dimensions because a zero-sized window is never useful;
// the parser maps `0` for these fields to `None`, activating the framebuffer fallback.
#[derive(Default)]
pub(crate) struct WindowCfg {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: Option<NonZero<u32>>,
    pub(crate) height: Option<NonZero<u32>>,
    pub(crate) frame: Option<WindowFrame>,
    pub(crate) always_on_top: bool,
}

// Game logic is frame-locked at one tick per `Present`, so higher frame rates speed everything up.
// Any field set to `0` disengages the pacer for that mode,
// making `Present` run as fast as the CPU/GPU allows.
#[allow(clippy::struct_field_names)]
pub(crate) struct FramerateCfg {
    pub(crate) game_fps: u32,
    pub(crate) replay_skip_fps: u32,
    pub(crate) replay_slow_fps: u32,
}

pub(crate) struct ProcessCfg {
    pub(crate) priority: PriorityClass,
    // This is `u32` because i686 processes can't address cores beyond bit 31 (WoW64 limit).
    // `None` means `SetProcessAffinityMask` is not called, so the OS keeps its scheduler default.
    pub(crate) affinity_mask: Option<NonZero<u32>>,
}

pub(crate) struct LogCfg {
    pub(crate) level: LevelFilter,
    pub(crate) sessions_to_keep: u32,
    pub(crate) log_dir: Option<PathBuf>,
}

impl Default for LogCfg {
    fn default() -> Self {
        Self {
            level: LevelFilter::INFO,
            sessions_to_keep: DEFAULT_SESSIONS_TO_KEEP,
            log_dir: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    Windowed,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshRateMode {
    /// The desktop's current refresh rate as reported by `GetAdapterDisplayModeEx`.
    /// Note: this is the *desktop's* rate, not necessarily the highest the monitor supports
    /// at the chosen game resolution. We don't enumerate modes; see `d3d9.rs::pick_refresh_rate`.
    Native,
    /// Highest multiple of 60 less than or equal to the desktop rate; e.g. 120 on 144 Hz and 60 on 100 Hz.
    /// Bounded by desktop rate, since we can't discover above-desktop modes without `EnumAdapterModes`,
    /// which is disabled; see `d3d9.rs::pick_refresh_rate`.
    NativeMultiple,
    /// Force a specific refresh rate in Hz, passed straight through to `CreateDeviceEx`.
    /// We don't validate that this value is advertised by the monitor at the chosen back-buffer dimensions.
    /// If it isn't, then `CreateDeviceEx` returns failure and the device doesn't come up.
    /// Use `Native` or `NativeMultiple` if you want a guaranteed-supported rate.
    /// This is `NonZero<u32>` because the D3D9 spec rejects 0 in fullscreen,
    /// and downstream callers can't receive a zero rate.
    Fixed(NonZero<u32>),
}

// Important: discriminants are load-bearing!
// They're the game's resolution encoding at `[0x4e79c3]` (mod 3),
// index the asset table at `0x4cb644`, and serve as the offset from `RES_RADIO_FIRST_ID` (`0xCD`)
// for the dialog radio control IDs. Don't reorder or re-number without deliberate care.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    R640x480 = 0,
    R960x720 = 1,
    R1280x960 = 2,
}

/// Window chrome. Maps directly to a combination of Win32 styles;
/// each variant is a different "look."
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowFrame {
    /// The "normal" desktop app appearance: title bar with system menu and
    /// minimize/maximize/close buttons. `WS_OVERLAPPED | WS_SYSMENU |
    /// WS_VISIBLE | WS_CAPTION | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`.
    Framed,
    /// No caption or border, but the system menu remains fully functional
    /// (Alt+Space for Move/Minimize/Maximize/Close).
    /// This matches exactly what the game does for windowed mode; see `fcn.00472f50`.
    /// `WS_OVERLAPPED | WS_SYSMENU | WS_VISIBLE | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`.
    Frameless,
    /// Pure pixel rectangle: no chrome and no system menu.
    /// `WS_POPUP | WS_VISIBLE`.
    Borderless,
}

// Realtime is deliberately omitted because we don't want or need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PriorityClass {
    Unchanged,
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl Resolution {
    pub(crate) fn index(self) -> u8 {
        self as u8
    }

    pub(crate) fn dimensions(self) -> (u32, u32) {
        match self {
            Self::R640x480 => (640, 480),
            Self::R960x720 => (960, 720),
            Self::R1280x960 => (1280, 960),
        }
    }
}

impl Display for DisplayMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Windowed => "Windowed",
            Self::Fullscreen => "Fullscreen",
        })
    }
}

impl Display for RefreshRateMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Native => f.write_str("Native"),
            Self::NativeMultiple => f.write_str("NativeMultiple"),
            Self::Fixed(n) => write!(f, "Fixed({n})"),
        }
    }
}

impl Display for Resolution {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::R640x480 => "640x480",
            Self::R960x720 => "960x720",
            Self::R1280x960 => "1280x960",
        })
    }
}

impl Display for WindowFrame {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Framed => "Framed",
            Self::Frameless => "Frameless",
            Self::Borderless => "Borderless",
        })
    }
}

impl Display for PriorityClass {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Unchanged => "Unchanged",
            Self::Idle => "Idle",
            Self::BelowNormal => "BelowNormal",
            Self::Normal => "Normal",
            Self::AboveNormal => "AboveNormal",
            Self::High => "High",
        })
    }
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            refresh_rate: RefreshRateMode::NativeMultiple,
            mode: DisplayMode::Windowed,
            resolution: Resolution::R1280x960,
        }
    }
}

impl Default for FramerateCfg {
    fn default() -> Self {
        Self {
            game_fps: DEFAULT_GAME_FPS,
            replay_skip_fps: DEFAULT_REPLAY_SKIP_FPS,
            replay_slow_fps: DEFAULT_REPLAY_SLOW_FPS,
        }
    }
}

impl Default for ProcessCfg {
    fn default() -> Self {
        Self {
            priority: PriorityClass::Unchanged,
            affinity_mask: None,
        }
    }
}

impl Config {
    pub(crate) fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        let mut section = "";
        for raw in text.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = name.trim();
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = unquote(v.trim());
            match section.to_ascii_lowercase().as_str() {
                "display" => apply_display(&mut cfg.display, k, v),
                "window" => apply_window(&mut cfg.window, k, v),
                "framerate" => apply_framerate(&mut cfg.framerate, k, v),
                "process" => apply_process(&mut cfg.process, k, v),
                "log" => apply_log(&mut cfg.log, k, v),
                _ => {}
            }
        }
        cfg
    }
}

fn apply_display(cfg: &mut DisplayCfg, k: &str, v: &str) {
    match k.to_ascii_lowercase().as_str() {
        "mode" => {
            if let Some(m) = parse_display_mode(v) {
                cfg.mode = m;
            }
        }
        "refresh_rate" => {
            if let Some(r) = parse_refresh_rate(v) {
                cfg.refresh_rate = r;
            }
        }

        "resolution" => {
            if let Some(r) = parse_resolution(v) {
                cfg.resolution = r;
            }
        }
        _ => {}
    }
}

fn apply_window(cfg: &mut WindowCfg, k: &str, v: &str) {
    match k.to_ascii_lowercase().as_str() {
        "x" => cfg.x = parse_i32(v).unwrap_or(0),
        "y" => cfg.y = parse_i32(v).unwrap_or(0),
        "width" => cfg.width = parse_nonzero_u32(v),
        "height" => cfg.height = parse_nonzero_u32(v),
        "frame" => cfg.frame = parse_window_frame(v),
        "always_on_top" => cfg.always_on_top = parse_bool(v).unwrap_or(false),
        _ => {}
    }
}

fn apply_framerate(cfg: &mut FramerateCfg, k: &str, v: &str) {
    match k.to_ascii_lowercase().as_str() {
        "game_fps" => cfg.game_fps = parse_u32(v).unwrap_or(DEFAULT_GAME_FPS),
        "replay_skip_fps" => {
            cfg.replay_skip_fps = parse_u32(v).unwrap_or(DEFAULT_REPLAY_SKIP_FPS);
        }
        "replay_slow_fps" => {
            cfg.replay_slow_fps = parse_u32(v).unwrap_or(DEFAULT_REPLAY_SLOW_FPS);
        }
        _ => {}
    }
}

fn apply_process(cfg: &mut ProcessCfg, k: &str, v: &str) {
    match k.to_ascii_lowercase().as_str() {
        "priority" => {
            if let Some(p) = parse_priority_class(v) {
                cfg.priority = p;
            }
        }
        "affinity_mask" => {
            cfg.affinity_mask = parse_bitmask(v).and_then(NonZero::new);
        }
        _ => {}
    }
}

fn apply_log(cfg: &mut LogCfg, k: &str, v: &str) {
    match k.to_ascii_lowercase().as_str() {
        "level" => {
            if let Some(level) = parse_level(v) {
                cfg.level = level;
            }
        }
        "sessions_to_keep" => {
            cfg.sessions_to_keep = parse_u32(v).unwrap_or(DEFAULT_SESSIONS_TO_KEEP);
        }
        "log_dir" => {
            // `v` is already outer-trimmed and unquoted; preserve any
            // inner whitespace the user intentionally quoted.
            cfg.log_dir = if v.is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            };
        }
        _ => {}
    }
}

fn parse_level(v: &str) -> Option<LevelFilter> {
    match v.to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

// This is quote-aware: instances of `;` and `#` inside a `"..."` or `'...'` value
// are preserved, so a path like `log_dir = "C:\foo;bar"` parses intact.
// Outside quotes, `;` and `#` mark the start of a comment.
fn strip_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            ';' | '#' if !in_double && !in_single => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Strips one matching `"..."` or `'...'` pair so quoted INI values
/// like `mode = "fullscreen"` parse the same as unquoted ones.
fn unquote(v: &str) -> &str {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &v[1..v.len() - 1];
        }
    }
    v
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

fn parse_u32(v: &str) -> Option<u32> {
    v.parse().ok()
}

fn parse_nonzero_u32(v: &str) -> Option<NonZero<u32>> {
    parse_u32(v).and_then(NonZero::new)
}

fn parse_i32(v: &str) -> Option<i32> {
    v.parse().ok()
}

fn parse_display_mode(v: &str) -> Option<DisplayMode> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(DisplayMode::Windowed),
        "fullscreen" => Some(DisplayMode::Fullscreen),
        _ => None,
    }
}

fn parse_refresh_rate(v: &str) -> Option<RefreshRateMode> {
    match v.to_ascii_lowercase().as_str() {
        "native" => Some(RefreshRateMode::Native),
        "nativemultiple" => Some(RefreshRateMode::NativeMultiple),
        // 0 is unsupported in fullscreen (D3D9 spec) and ignored in
        // windowed; the NonZero filter is in `parse_nonzero_u32`.
        _ => parse_nonzero_u32(v).map(RefreshRateMode::Fixed),
    }
}

fn parse_resolution(v: &str) -> Option<Resolution> {
    match v.to_ascii_lowercase().as_str() {
        "640x480" => Some(Resolution::R640x480),
        "960x720" => Some(Resolution::R960x720),
        "1280x960" => Some(Resolution::R1280x960),
        _ => None,
    }
}

fn parse_window_frame(v: &str) -> Option<WindowFrame> {
    match v.to_ascii_lowercase().as_str() {
        "framed" => Some(WindowFrame::Framed),
        "frameless" => Some(WindowFrame::Frameless),
        "borderless" => Some(WindowFrame::Borderless),
        _ => None,
    }
}

fn parse_priority_class(v: &str) -> Option<PriorityClass> {
    match v.to_ascii_lowercase().as_str() {
        "unchanged" => Some(PriorityClass::Unchanged),
        "idle" => Some(PriorityClass::Idle),
        "belownormal" => Some(PriorityClass::BelowNormal),
        "normal" => Some(PriorityClass::Normal),
        "abovenormal" => Some(PriorityClass::AboveNormal),
        "high" => Some(PriorityClass::High),
        _ => None,
    }
}

// `0x` / `0o` / `0b` radix prefixes are recognized.
// Bare numbers are interpreted as decimal.
fn parse_bitmask(v: &str) -> Option<u32> {
    let bytes = v.as_bytes();
    let (radix, rest) = if bytes.len() >= 2 && bytes[0] == b'0' {
        match bytes[1].to_ascii_lowercase() {
            b'x' => (16, &v[2..]),
            b'o' => (8, &v[2..]),
            b'b' => (2, &v[2..]),
            _ => (10, v),
        }
    } else {
        (10, v)
    };
    u32::from_str_radix(rest, radix).ok()
}

/// Strips an optional UTF-8 BOM.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    String::from_utf8_lossy(body).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
    }

    #[test]
    fn decode_text_strips_bom() {
        assert_eq!(decode_text(b"\xef\xbb\xbfhello"), "hello");
        assert_eq!(decode_text(b"hello"), "hello");
        // Only one BOM is stripped.
        assert_eq!(decode_text(b"\xef\xbb\xbf\xef\xbb\xbfx"), "\u{feff}x",);
    }

    #[test]
    fn unquote_strips_matching_pairs() {
        assert_eq!(unquote("\"hi\""), "hi");
        assert_eq!(unquote("'hi'"), "hi");
        assert_eq!(unquote("hi"), "hi");
        // Mismatched quotes are left intact.
        assert_eq!(unquote("\"hi'"), "\"hi'");
        // Single-character inputs don't qualify (need >= 2 bytes).
        assert_eq!(unquote("\""), "\"");
        // Empty quoted pair becomes empty.
        assert_eq!(unquote("\"\""), "");
    }

    #[test]
    fn parse_bool_recognises_aliases() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("YES"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn parse_u32_decimal_only() {
        assert_eq!(parse_u32("42"), Some(42));
        // No radix prefix support here.
        assert_eq!(parse_u32("0xff"), None);
        assert_eq!(parse_u32("0b10"), None);
        assert_eq!(parse_u32("nope"), None);
    }

    #[test]
    fn parse_bitmask_handles_radix_prefixes() {
        assert_eq!(parse_bitmask("42"), Some(42));
        assert_eq!(parse_bitmask("0xff"), Some(0xff));
        assert_eq!(parse_bitmask("0xFF"), Some(0xff));
        assert_eq!(parse_bitmask("0o17"), Some(0o17));
        assert_eq!(parse_bitmask("0b1010"), Some(0b1010));
        assert_eq!(parse_bitmask("0xFFFFFFFF"), Some(u32::MAX));
        // Anything that wouldn't fit in `u32` is rejected.
        assert_eq!(parse_bitmask("0x100000000"), None);
        assert_eq!(parse_bitmask("nope"), None);
    }

    #[test]
    fn parse_priority_class_no_realtime() {
        assert_eq!(parse_priority_class("high"), Some(PriorityClass::High));
        assert_eq!(parse_priority_class("HIGH"), Some(PriorityClass::High));
        assert_eq!(
            parse_priority_class("unchanged"),
            Some(PriorityClass::Unchanged)
        );
        assert_eq!(parse_priority_class("realtime"), None);
    }

    #[test]
    fn parse_refresh_rate_falls_back_to_fixed() {
        assert_eq!(parse_refresh_rate("native"), Some(RefreshRateMode::Native));
        assert_eq!(
            parse_refresh_rate("nativemultiple"),
            Some(RefreshRateMode::NativeMultiple),
        );
        assert_eq!(
            parse_refresh_rate("144"),
            Some(RefreshRateMode::Fixed(nz(144)))
        );
        // No radix prefix support here.
        assert_eq!(parse_refresh_rate("0xFF"), None);
        assert_eq!(parse_refresh_rate("garbage"), None);
    }

    #[test]
    fn parse_resolution_rejects_unsupported() {
        assert_eq!(parse_resolution("640x480"), Some(Resolution::R640x480));
        assert_eq!(parse_resolution("960x720"), Some(Resolution::R960x720));
        assert_eq!(parse_resolution("1280x960"), Some(Resolution::R1280x960));
        assert_eq!(parse_resolution("1920x1080"), None);
    }

    #[test]
    fn resolution_index_locks_external_encoding() {
        assert_eq!(Resolution::R640x480.index(), 0);
        assert_eq!(Resolution::R960x720.index(), 1);
        assert_eq!(Resolution::R1280x960.index(), 2);
    }

    #[test]
    fn config_parse_applies_known_keys() {
        let text = "
            [framerate]
            game_fps = 120
            replay_skip_fps = 480

            [process]
            priority = High
            affinity_mask = 0xFF
        ";
        let cfg = Config::parse(text);
        assert_eq!(cfg.framerate.game_fps, 120);
        assert_eq!(cfg.framerate.replay_skip_fps, 480);
        assert_eq!(cfg.framerate.replay_slow_fps, DEFAULT_REPLAY_SLOW_FPS);
        assert_eq!(cfg.process.priority, PriorityClass::High);
        assert_eq!(cfg.process.affinity_mask, Some(nz(0xFF)));
    }

    #[test]
    fn config_parse_silently_ignores_unknown() {
        // Unknown sections, unknown keys, and malformed lines all leave defaults.
        let text = "
            [does_not_exist]
            x = 1

            [framerate]
            unknown_key = whatever
            game_fps = NotANumber
            lead_time = also_a_silently_ignored_key

            no_equals_sign
            ; comment line
            # also a comment
        ";
        let cfg = Config::parse(text);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
    }

    #[test]
    fn config_parse_window_defaults_and_overrides() {
        let w = Config::parse("[framerate]\ngame_fps = 60").window;
        assert_eq!(w.width, None);
        assert_eq!(w.height, None);
        assert_eq!(w.frame, None);
        assert!(!w.always_on_top);

        let w = Config::parse("[window]\nwidth = 1920\nalways_on_top = true\nframe = borderless")
            .window;
        assert_eq!(w.width, Some(nz(1920)));
        assert_eq!(w.height, None);
        assert_eq!(w.frame, Some(WindowFrame::Borderless));
        assert!(w.always_on_top);
    }

    #[test]
    fn config_parse_zero_window_dim_falls_back_to_default() {
        let cfg = Config::parse("[window]\nwidth = 0\nheight = 0");
        assert_eq!(cfg.window.width, None);
        assert_eq!(cfg.window.height, None);
    }

    #[test]
    fn parse_window_frame_accepts_each_variant() {
        assert_eq!(parse_window_frame("framed"), Some(WindowFrame::Framed));
        assert_eq!(
            parse_window_frame("FRAMELESS"),
            Some(WindowFrame::Frameless)
        );
        assert_eq!(
            parse_window_frame("borderless"),
            Some(WindowFrame::Borderless),
        );
        assert_eq!(parse_window_frame("nope"), None);
    }

    #[test]
    fn config_parse_handles_quoted_values_and_comments() {
        let cfg = Config::parse(
            "[display]\nmode = \"fullscreen\" ; trailing comment\nresolution = '960x720'",
        );
        assert_eq!(cfg.display.mode, DisplayMode::Fullscreen);
        assert_eq!(cfg.display.resolution, Resolution::R960x720);
    }

    #[test]
    fn config_parse_preserves_comment_chars_inside_quoted_values() {
        let cfg = Config::parse("[log]\nlog_dir = \"C:\\foo;bar\"");
        assert_eq!(cfg.log.log_dir, Some(PathBuf::from("C:\\foo;bar")));

        let cfg = Config::parse("[log]\nlog_dir = 'C:\\hash#path'");
        assert_eq!(cfg.log.log_dir, Some(PathBuf::from("C:\\hash#path")));

        let cfg = Config::parse("[log]\nlog_dir = \"C:\\real;path\" ; comment");
        assert_eq!(cfg.log.log_dir, Some(PathBuf::from("C:\\real;path")));
    }

    #[test]
    fn config_parse_section_match_is_case_insensitive() {
        let cfg = Config::parse("[FrameRate]\nGame_FPS = 30");
        assert_eq!(cfg.framerate.game_fps, 30);
    }

    #[test]
    fn config_parse_explicit_zero_fps() {
        let cfg =
            Config::parse("[framerate]\ngame_fps = 0\nreplay_skip_fps = 0\nreplay_slow_fps = 0");
        assert_eq!(cfg.framerate.game_fps, 0);
        assert_eq!(cfg.framerate.replay_skip_fps, 0);
        assert_eq!(cfg.framerate.replay_slow_fps, 0);
    }

    #[test]
    fn config_parse_garbage_fps_falls_back_to_default() {
        let cfg = Config::parse("[framerate]\ngame_fps = nonsense");
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
    }

    #[test]
    fn config_parse_zero_affinity_mask_is_none() {
        let cfg = Config::parse("[process]\naffinity_mask = 0");
        assert_eq!(cfg.process.affinity_mask, None);
    }

    #[test]
    fn default_config_matches_documented_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.display.mode, DisplayMode::Windowed);
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(cfg.display.resolution, Resolution::R1280x960);
        assert_eq!(cfg.window.x, 0);
        assert_eq!(cfg.window.y, 0);
        assert_eq!(cfg.window.width, None);
        assert_eq!(cfg.window.height, None);
        assert_eq!(cfg.window.frame, None);
        assert!(!cfg.window.always_on_top);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
        assert_eq!(cfg.framerate.replay_skip_fps, DEFAULT_REPLAY_SKIP_FPS);
        assert_eq!(cfg.framerate.replay_slow_fps, DEFAULT_REPLAY_SLOW_FPS);
        assert_eq!(cfg.process.priority, PriorityClass::Unchanged);
        assert_eq!(cfg.process.affinity_mask, None);
        assert_eq!(cfg.log.level, LevelFilter::INFO);
        assert_eq!(cfg.log.sessions_to_keep, DEFAULT_SESSIONS_TO_KEEP);
        assert_eq!(cfg.log.log_dir, None);
    }

    #[test]
    fn config_parse_applies_log_keys() {
        let cfg = Config::parse("[log]\nlevel = trace\nsessions_to_keep = 5\nlog_dir = /tmp/x");
        assert_eq!(cfg.log.level, LevelFilter::TRACE);
        assert_eq!(cfg.log.sessions_to_keep, 5);
        assert_eq!(cfg.log.log_dir, Some(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn parse_level_off_disables_logging() {
        assert_eq!(
            Config::parse("[log]\nlevel = off").log.level,
            LevelFilter::OFF
        );
        assert_eq!(
            Config::parse("[log]\nlevel = OFF").log.level,
            LevelFilter::OFF
        );
        // Unrecognized values leave the default intact.
        assert_eq!(
            Config::parse("[log]\nlevel = nonsense").log.level,
            LevelFilter::INFO,
        );
    }

    #[test]
    fn config_parse_log_dir_blank_is_none() {
        assert_eq!(Config::parse("[log]\nlog_dir =").log.log_dir, None);
        assert_eq!(Config::parse("[log]\nlog_dir = \"\"").log.log_dir, None);
    }

    #[test]
    fn config_parse_applies_display_keys() {
        let cfg =
            Config::parse("[display]\nrefresh_rate = 144\nmode = fullscreen\nresolution = 960x720");
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::Fixed(nz(144)));
        assert_eq!(cfg.display.mode, DisplayMode::Fullscreen);
        assert_eq!(cfg.display.resolution, Resolution::R960x720);

        let cfg = Config::parse("[display]\nrefresh_rate = native");
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::Native);
    }

    #[test]
    fn parse_refresh_rate_rejects_zero() {
        assert_eq!(parse_refresh_rate("0"), None);
        assert_eq!(parse_refresh_rate("0x0"), None);
        assert_eq!(
            parse_refresh_rate("60"),
            Some(RefreshRateMode::Fixed(nz(60)))
        );
    }
}
