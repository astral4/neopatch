//! Shared configuration schema and INI parsing helpers.
//!
//! `CoreConfig` represents the game-agnostic settings. Each per-game crate defines
//! its own config struct that embeds `CoreConfig` plus game-specific fields
//! (e.g. `Resolution`) and specifies its own parsing logic.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Result as IoResult, Write};
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::level_filters::LevelFilter;

const DEFAULT_GAME_FPS: u32 = 60;
const DEFAULT_REPLAY_SKIP_FPS: u32 = 240;
const DEFAULT_REPLAY_SLOW_FPS: u32 = 30;
const DEFAULT_SESSIONS_TO_KEEP: NonZero<u32> = NonZero::new(10).unwrap();

/// Process-wide handle to the active core configuration.
/// Set by the game crate at install time, before any hook that reads it.
pub static CONFIG: OnceLock<CoreConfig> = OnceLock::new();

#[derive(Default)]
pub struct CoreConfig {
    pub display: DisplayCfg,
    pub window: WindowCfg,
    pub framerate: FramerateCfg,
    pub input: InputCfg,
    pub process: ProcessCfg,
    pub log: LogCfg,
}

pub struct DisplayCfg {
    pub mode: DisplayMode,
    /// Ignored in windowed mode.
    pub refresh_rate: RefreshRateMode,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            refresh_rate: RefreshRateMode::NativeMultiple,
            mode: DisplayMode::Windowed,
        }
    }
}

// Window dimensions and chrome default to game-derived values supplied at install time
// (matching framebuffer dimensions; `Borderless` in fullscreen, `Frameless` in windowed).
// Set explicitly to override.
#[derive(Default)]
pub struct WindowCfg {
    pub x: i32,
    pub y: i32,
    pub width: Option<NonZero<u32>>,
    pub height: Option<NonZero<u32>>,
    pub frame: Option<WindowFrame>,
    pub always_on_top: bool,
}

// Game logic is frame-locked at one tick per `Present`, so higher rates
// speed everything up. A field set to `0` disengages the pacer for that mode,
// making `Present` run as fast as the CPU/GPU allows.
#[allow(clippy::struct_field_names)]
pub struct FramerateCfg {
    pub game_fps: u32,
    pub replay_skip_fps: u32,
    pub replay_slow_fps: u32,
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

pub struct InputCfg {
    /// Fold joystick POV hat / D-pad inputs into directions read by the game.
    pub dpad: bool,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self { dpad: true }
    }
}

pub struct ProcessCfg {
    pub priority: PriorityClass,
    // `u32` because i686 processes can't address cores beyond bit 31 (WoW64 limit).
    // `None` means `SetProcessAffinityMask` is not called, so the OS keeps its scheduler default.
    pub affinity_mask: Option<NonZero<u32>>,
}

impl Default for ProcessCfg {
    fn default() -> Self {
        Self {
            priority: PriorityClass::Unchanged,
            affinity_mask: None,
        }
    }
}

pub struct LogCfg {
    pub level: LevelFilter,
    pub sessions_to_keep: NonZero<u32>,
    pub log_dir: Option<PathBuf>,
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
pub enum DisplayMode {
    Windowed,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshRateMode {
    /// The desktop's current refresh rate. Note: this is not necessarily the highest
    /// the monitor supports at the chosen game resolution. We don't enumerate modes;
    /// see `d3d9::pick_refresh_rate`.
    Native,
    /// Highest multiple of 60 less than or equal to the desktop rate.
    NativeMultiple,
    /// Force a specific refresh rate in Hz. We don't validate that this value is
    /// advertised by the monitor at the chosen back-buffer dimensions.
    /// If it isn't, then device creation fails.
    Fixed(NonZero<u32>),
}

/// Window chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFrame {
    /// `WS_OVERLAPPED | WS_SYSMENU | WS_VISIBLE | WS_CAPTION | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`.
    /// The "normal" desktop app appearance: title bar with system menu
    /// and minimize/maximize/close buttons.
    Framed,
    /// `WS_OVERLAPPED | WS_SYSMENU | WS_VISIBLE | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`.
    /// No caption or border, but the system menu remains fully functional
    /// (Alt+Space for Move/Minimize/Maximize/Close).
    Frameless,
    /// `WS_POPUP | WS_VISIBLE`. Pure pixel rectangle; no chrome and no system menu.
    Borderless,
}

// Realtime is deliberately omitted because we don't want or need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityClass {
    Unchanged,
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
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

// The `apply_*` functions below apply a key/value pair from a specific section to `cfg`.
// Unknown keys are silently ignored.
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

fn apply_input(cfg: &mut InputCfg, k: &str, v: &str) {
    if k.eq_ignore_ascii_case("dpad")
        && let Some(b) = parse_bool(v)
    {
        cfg.dpad = b;
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
            cfg.sessions_to_keep = parse_nonzero_u32(v).unwrap_or(DEFAULT_SESSIONS_TO_KEEP);
        }
        "log_dir" => {
            // `v` is already outer-trimmed and unquoted.
            // We preserve any inner whitespace a user intentionally quotes.
            cfg.log_dir = if v.is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            };
        }
        _ => {}
    }
}

#[must_use]
pub(crate) fn parse_level(v: &str) -> Option<LevelFilter> {
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

/// Scans `text` (assuming INI format), invoking `f(section, key, value)`
/// for each `key = value` line. Comments are stripped. Sections track the most recent
/// `[name]` header (empty before the first), and values are unquoted.
/// Unknown sections and malformed lines are silently skipped.
///
/// Game-specific parsers compose with [`parse_core_only`] by walking `for_each_setting`
/// for the game's own keys and calling `parse_core_only` separately for the core sections.
pub fn for_each_setting(text: &str, mut f: impl FnMut(&str, &str, &str)) {
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
        f(section, k.trim(), unquote(v.trim()));
    }
}

// This is quote-aware: instances of `;` and `#` inside a `"..."` or `'...'` value
// are preserved, so a path like `log_dir = "C:\foo;bar"` parses intact.
// Outside quotes, `;` and `#` mark the start of a comment.
#[must_use]
pub(crate) fn strip_comment(line: &str) -> &str {
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
#[must_use]
pub(crate) fn unquote(v: &str) -> &str {
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

#[must_use]
pub(crate) fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

#[must_use]
pub(crate) fn parse_u32(v: &str) -> Option<u32> {
    v.parse().ok()
}

#[must_use]
pub(crate) fn parse_nonzero_u32(v: &str) -> Option<NonZero<u32>> {
    parse_u32(v).and_then(NonZero::new)
}

#[must_use]
pub(crate) fn parse_i32(v: &str) -> Option<i32> {
    v.parse().ok()
}

#[must_use]
pub(crate) fn parse_display_mode(v: &str) -> Option<DisplayMode> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(DisplayMode::Windowed),
        "fullscreen" => Some(DisplayMode::Fullscreen),
        _ => None,
    }
}

#[must_use]
pub(crate) fn parse_refresh_rate(v: &str) -> Option<RefreshRateMode> {
    match v.to_ascii_lowercase().as_str() {
        "native" => Some(RefreshRateMode::Native),
        "nativemultiple" => Some(RefreshRateMode::NativeMultiple),
        _ => parse_nonzero_u32(v).map(RefreshRateMode::Fixed),
    }
}

#[must_use]
pub(crate) fn parse_window_frame(v: &str) -> Option<WindowFrame> {
    match v.to_ascii_lowercase().as_str() {
        "framed" => Some(WindowFrame::Framed),
        "frameless" => Some(WindowFrame::Frameless),
        "borderless" => Some(WindowFrame::Borderless),
        _ => None,
    }
}

#[must_use]
pub(crate) fn parse_priority_class(v: &str) -> Option<PriorityClass> {
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
#[must_use]
pub(crate) fn parse_bitmask(v: &str) -> Option<u32> {
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
#[must_use]
pub fn decode_text(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    String::from_utf8_lossy(body).into_owned()
}

/// Parses INI text into a `CoreConfig` using only the shared section dispatcher.
/// Per-game crates that don't define their own config keys call this directly from
/// `install_hooks`. Crates with their own sections (e.g. th15's `[display] resolution`)
/// supply their own parse function.
#[must_use]
pub fn parse_core_only(text: &str) -> CoreConfig {
    let mut core = CoreConfig::default();
    for_each_setting(text, |section, k, v| {
        match section.to_ascii_lowercase().as_str() {
            "display" => apply_display(&mut core.display, k, v),
            "window" => apply_window(&mut core.window, k, v),
            "framerate" => apply_framerate(&mut core.framerate, k, v),
            "input" => apply_input(&mut core.input, k, v),
            "process" => apply_process(&mut core.process, k, v),
            "log" => apply_log(&mut core.log, k, v),
            _ => {}
        }
    });
    core
}

/// Writes the game-agnostic manifest lines after the log preamble.
/// Called automatically by [`crate::log::init`] before the game's `extra_manifest` runs.
pub(crate) fn write_manifest_common<W: Write + ?Sized>(
    w: &mut W,
    core: &CoreConfig,
) -> IoResult<()> {
    writeln!(w, "display.mode={}", core.display.mode)?;
    writeln!(w, "display.refresh_rate={}", core.display.refresh_rate)?;
    let win = &core.window;
    writeln!(
        w,
        "window={}x{} at ({},{}) frame={} always_on_top={}",
        fmt_opt(win.width.as_ref()),
        fmt_opt(win.height.as_ref()),
        win.x,
        win.y,
        fmt_opt(win.frame.as_ref()),
        win.always_on_top,
    )?;
    writeln!(w, "framerate.game_fps={}", core.framerate.game_fps)?;
    writeln!(
        w,
        "framerate.replay_skip_fps={}",
        core.framerate.replay_skip_fps
    )?;
    writeln!(
        w,
        "framerate.replay_slow_fps={}",
        core.framerate.replay_slow_fps
    )?;
    writeln!(w, "input.dpad={}", core.input.dpad)?;
    writeln!(w, "process.priority={}", core.process.priority)?;
    writeln!(
        w,
        "process.affinity_mask={}",
        fmt_mask(core.process.affinity_mask)
    )?;
    Ok(())
}

fn fmt_opt<T: Display>(v: Option<&T>) -> String {
    v.map_or_else(|| "auto".to_owned(), ToString::to_string)
}

fn fmt_mask(v: Option<NonZero<u32>>) -> String {
    v.map_or_else(|| "0 (default)".to_owned(), |m| format!("{:#x}", m.get()))
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
    fn parse_refresh_rate_rejects_zero() {
        assert_eq!(parse_refresh_rate("0"), None);
        assert_eq!(parse_refresh_rate("0x0"), None);
        assert_eq!(
            parse_refresh_rate("60"),
            Some(RefreshRateMode::Fixed(nz(60)))
        );
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
    fn parse_level_off_disables_logging() {
        let mut cfg = LogCfg::default();
        apply_log(&mut cfg, "level", "off");
        assert_eq!(cfg.level, LevelFilter::OFF);
    }

    #[test]
    fn apply_log_zero_sessions_to_keep_falls_back_to_default() {
        let mut cfg = LogCfg::default();
        apply_log(&mut cfg, "sessions_to_keep", "0");
        assert_eq!(cfg.sessions_to_keep, DEFAULT_SESSIONS_TO_KEEP);
    }

    #[test]
    fn apply_log_blank_log_dir_is_none() {
        let mut cfg = LogCfg::default();
        apply_log(&mut cfg, "log_dir", "");
        assert_eq!(cfg.log_dir, None);
    }

    #[test]
    fn apply_display_ignores_unknown_keys() {
        // Game-specific keys should be silently skipped by `apply_display` here.
        let mut cfg = DisplayCfg::default();
        let baseline_mode = cfg.mode;
        let baseline_rate = cfg.refresh_rate;
        apply_display(&mut cfg, "resolution", "640x480");
        assert_eq!(cfg.mode, baseline_mode);
        assert_eq!(cfg.refresh_rate, baseline_rate);
    }

    #[test]
    fn apply_framerate_sets_known_keys_and_clamps_defaults() {
        let mut cfg = FramerateCfg::default();
        apply_framerate(&mut cfg, "game_fps", "120");
        apply_framerate(&mut cfg, "replay_skip_fps", "480");
        apply_framerate(&mut cfg, "replay_slow_fps", "garbage");
        assert_eq!(cfg.game_fps, 120);
        assert_eq!(cfg.replay_skip_fps, 480);
        assert_eq!(cfg.replay_slow_fps, DEFAULT_REPLAY_SLOW_FPS);
    }

    #[test]
    fn apply_window_default_and_overrides() {
        let mut cfg = WindowCfg::default();
        apply_window(&mut cfg, "width", "1920");
        apply_window(&mut cfg, "always_on_top", "true");
        apply_window(&mut cfg, "frame", "borderless");
        assert_eq!(cfg.width, Some(nz(1920)));
        assert_eq!(cfg.height, None);
        assert_eq!(cfg.frame, Some(WindowFrame::Borderless));
        assert!(cfg.always_on_top);
    }

    #[test]
    fn apply_window_zero_dim_falls_back_to_none() {
        let mut cfg = WindowCfg::default();
        apply_window(&mut cfg, "width", "0");
        apply_window(&mut cfg, "height", "0");
        assert_eq!(cfg.width, None);
        assert_eq!(cfg.height, None);
    }

    #[test]
    fn apply_process_zero_affinity_mask_is_none() {
        let mut cfg = ProcessCfg::default();
        apply_process(&mut cfg, "affinity_mask", "0");
        assert_eq!(cfg.affinity_mask, None);
    }

    #[test]
    fn default_core_config_matches_documented_defaults() {
        let cfg = CoreConfig::default();
        assert_eq!(cfg.display.mode, DisplayMode::Windowed);
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(cfg.window.x, 0);
        assert_eq!(cfg.window.y, 0);
        assert_eq!(cfg.window.width, None);
        assert_eq!(cfg.window.height, None);
        assert_eq!(cfg.window.frame, None);
        assert!(!cfg.window.always_on_top);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
        assert_eq!(cfg.framerate.replay_skip_fps, DEFAULT_REPLAY_SKIP_FPS);
        assert_eq!(cfg.framerate.replay_slow_fps, DEFAULT_REPLAY_SLOW_FPS);
        assert!(cfg.input.dpad);
        assert_eq!(cfg.process.priority, PriorityClass::Unchanged);
        assert_eq!(cfg.process.affinity_mask, None);
        assert_eq!(cfg.log.level, LevelFilter::INFO);
        assert_eq!(cfg.log.sessions_to_keep, DEFAULT_SESSIONS_TO_KEEP);
        assert_eq!(cfg.log.log_dir, None);
    }

    #[test]
    fn apply_input_toggles_dpad() {
        let mut cfg = InputCfg::default();
        assert!(cfg.dpad);
        apply_input(&mut cfg, "dpad", "off");
        assert!(!cfg.dpad);
        apply_input(&mut cfg, "dpad", "on");
        assert!(cfg.dpad);
        // Unknown values leave the current setting alone.
        apply_input(&mut cfg, "dpad", "garbage");
        assert!(cfg.dpad);
        // Unknown keys are ignored.
        apply_input(&mut cfg, "other_key", "off");
        assert!(cfg.dpad);
    }

    #[test]
    fn parse_core_only_empty_matches_defaults() {
        let cfg = parse_core_only("");
        assert_eq!(cfg.display.mode, DisplayMode::Windowed);
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
    }

    #[test]
    fn parse_core_only_applies_known_keys_across_sections() {
        let text = "
            [framerate]
            game_fps = 120
            replay_skip_fps = 480

            [process]
            priority = High
            affinity_mask = 0xFF

            [display]
            mode = fullscreen
        ";
        let cfg = parse_core_only(text);
        assert_eq!(cfg.framerate.game_fps, 120);
        assert_eq!(cfg.framerate.replay_skip_fps, 480);
        assert_eq!(cfg.process.priority, PriorityClass::High);
        assert_eq!(cfg.process.affinity_mask, Some(nz(0xFF)));
        assert_eq!(cfg.display.mode, DisplayMode::Fullscreen);
    }

    #[test]
    fn parse_core_only_silently_ignores_unknown_sections_and_keys() {
        let text = "
            [does_not_exist]
            x = 1

            [framerate]
            unknown_key = whatever
            game_fps = NotANumber

            no_equals_sign
            ; comment line
            # also a comment
        ";
        let cfg = parse_core_only(text);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
    }

    #[test]
    fn parse_core_only_handles_quoted_values_and_trailing_comments() {
        let cfg = parse_core_only("[display]\nmode = \"fullscreen\" ; trailing comment");
        assert_eq!(cfg.display.mode, DisplayMode::Fullscreen);
    }
}
