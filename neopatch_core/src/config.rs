//! Shared configuration schema and INI parsing helpers.

use std::borrow::Cow;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::read;
use std::io::{Result as IoResult, Write};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::level_filters::LevelFilter;

const DEFAULT_GAME_FPS: u32 = 60;
const DEFAULT_REPLAY_SKIP_FPS: u32 = 240;
const DEFAULT_REPLAY_SLOW_FPS: u32 = 30;
const DEFAULT_SESSIONS_TO_KEEP: NonZero<u32> = NonZero::new(10).unwrap();

/// The characters that open a comment.
const COMMENT: [char; 2] = [';', '#'];

/// Process-wide handle to the active core configuration. Set by the game crate at install time, before any hook that reads it.
pub static CONFIG: OnceLock<CoreConfig> = OnceLock::new();

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CoreConfig {
    pub display: DisplayCfg,
    pub window: WindowCfg,
    pub framerate: FramerateCfg,
    pub input: InputCfg,
    pub process: ProcessCfg,
    pub log: LogCfg,
}

#[derive(Debug, PartialEq, Eq)]
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

// Window dimensions and frame default to game-derived values supplied at install time:
// matching framebuffer dimensions; `Borderless` in fullscreen and `Frameless` in windowed.
// This configuration only applies to windowed-mode (non-popup) window creations since exclusive-fullscreen is managed by D3D.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WindowCfg {
    /// `x` and `y` indicate the requested outer window position.
    /// `None` (both unset) leaves placement to the system and the window manager. Setting only one of the pair treats the other as `0`.
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<NonZero<u32>>,
    pub height: Option<NonZero<u32>>,
    pub frame: Option<WindowFrame>,
    pub always_on_top: bool,
}

// Game logic is frame-locked at one tick per `Present`, so higher rates speed everything up.
// A field set to `0` disengages the pacer for that mode, making `Present` run as fast as the CPU/GPU allows.
#[derive(Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq)]
pub struct InputCfg {
    /// Fold joystick POV hat / D-pad inputs into directions read by the game.
    pub dpad: bool,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self { dpad: true }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProcessCfg {
    pub priority: PriorityClass,
    /// `None` means `SetProcessAffinityMask` is not called and the OS scheduler default is used.
    // This is `u32` because 32-bit processes can't address cores beyond bit 31.
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

#[derive(Debug, PartialEq, Eq)]
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
    /// Highest supported rate not above the desktop's rate at the chosen resolution; otherwise the lowest supported rate.
    Native,
    /// Highest supported multiple-of-60 rate not above the desktop's rate at the chosen resolution; otherwise identical to `Native`.
    NativeMultiple,
    /// Force a specific rate in Hz. Requests the adapter's matching advertised mode (including its NTSC-derived variant,
    /// e.g. 119 for a `Fixed(120)`) when one exists. Falls back to the game's original rate if rejected at the chosen resolution.
    Fixed(NonZero<u32>),
}

/// Window frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFrame {
    /// The "normal" desktop app appearance: title bar with system menu and minimize/maximize/close buttons.
    Framed,
    /// No caption or border, but the system menu remains fully functional (Alt+Space for Move/Minimize/Maximize/Close).
    Frameless,
    /// Pure pixel rectangle; no frame or system menu.
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

// The `apply_*` functions below apply a key/value pair from a specific section to `cfg`. Unknown keys are silently ignored.
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
        "x" => cfg.x = parse_i32(v),
        "y" => cfg.y = parse_i32(v),
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
            // `v` is already outer-trimmed and unquoted. We preserve any inner whitespace a user intentionally quotes.
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

/// Scans `text` (assuming INI format), invoking `f(section, key, value)` for each `key = value` line.
/// Sections track the most recent `[name]` header (empty before the first), and values are unquoted.
/// Comments are stripped and malformed lines are silently skipped.
fn for_each_setting(text: &str, mut f: impl FnMut(&str, &str, &str)) {
    let mut section = "";
    for raw in text.lines() {
        // A line's kind is decided by its first significant character, before any comment or `=` handling.
        // `[` opens a section header, and anything else is a `key = value` candidate.
        if let Some(body) = raw.trim_start().strip_prefix('[') {
            if let Some(name) = strip_comment(body).trim_end().strip_suffix(']') {
                section = name.trim();
            }
            continue;
        }
        let Some((head, tail)) = raw.split_once('=') else {
            continue;
        };
        if head.contains(COMMENT) {
            continue;
        }
        let key = head.trim();
        if key.is_empty() {
            continue;
        }
        f(section, key, unquote(value_before_comment(tail.trim())));
    }
}

/// Strips a comment from the input line.
#[must_use]
fn strip_comment(line: &str) -> &str {
    match line.find(COMMENT) {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Returns `v` up to the start of any trailing comment.
#[must_use]
fn value_before_comment(v: &str) -> &str {
    let opener = v.chars().next().filter(|&c| c == '"' || c == '\'');
    if let Some(q) = opener
        && let Some(rel_end) = v[q.len_utf8()..].find(q)
    {
        // We keep both quotation marks here since `unquote` will strip the pair.
        return &v[..q.len_utf8() + rel_end + q.len_utf8()];
    }
    strip_comment(v).trim_end()
}

/// Strips one matching `"..."` or `'...'` pair so quoted INI values like `mode = "fullscreen"` parse the same as unquoted ones.
#[must_use]
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

#[must_use]
fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

#[must_use]
fn parse_u32(v: &str) -> Option<u32> {
    v.parse().ok()
}

#[must_use]
fn parse_nonzero_u32(v: &str) -> Option<NonZero<u32>> {
    parse_u32(v).and_then(NonZero::new)
}

#[must_use]
fn parse_i32(v: &str) -> Option<i32> {
    v.parse().ok()
}

#[must_use]
fn parse_display_mode(v: &str) -> Option<DisplayMode> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(DisplayMode::Windowed),
        "fullscreen" => Some(DisplayMode::Fullscreen),
        _ => None,
    }
}

#[must_use]
fn parse_refresh_rate(v: &str) -> Option<RefreshRateMode> {
    match v.to_ascii_lowercase().as_str() {
        "native" => Some(RefreshRateMode::Native),
        "nativemultiple" => Some(RefreshRateMode::NativeMultiple),
        _ => parse_nonzero_u32(v).map(RefreshRateMode::Fixed),
    }
}

#[must_use]
fn parse_window_frame(v: &str) -> Option<WindowFrame> {
    match v.to_ascii_lowercase().as_str() {
        "framed" => Some(WindowFrame::Framed),
        "frameless" => Some(WindowFrame::Frameless),
        "borderless" => Some(WindowFrame::Borderless),
        _ => None,
    }
}

#[must_use]
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

// `0x` / `0o` / `0b` radix prefixes are recognized. Bare numbers are interpreted as decimal.
#[must_use]
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

/// Strips a BOM if present and lossily decodes the input as UTF-8.
#[must_use]
pub fn decode_text(bytes: &[u8]) -> Cow<'_, str> {
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe") {
        return Cow::Owned(decode_utf16(body, u16::from_le_bytes));
    }
    if let Some(body) = bytes.strip_prefix(b"\xfe\xff") {
        return Cow::Owned(decode_utf16(body, u16::from_be_bytes));
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    String::from_utf8_lossy(body)
}

/// Lossily decodes `body`, assumed to be UTF-16.
fn decode_utf16(body: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| unit([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Reads and decodes the `neopatch.ini` file next to the host executable.
/// Returns an empty string if the directory is unknown or the file is unreadable.
#[must_use]
pub fn read_ini_text(exe_dir: Option<&Path>) -> String {
    exe_dir
        .and_then(|d| read(d.join("neopatch.ini")).ok())
        .map_or_else(String::new, |b| decode_text(&b).into_owned())
}

/// Applies one setting to the matching core section. Unknown sections and keys are silently ignored.
fn apply_core_setting(core: &mut CoreConfig, section: &str, k: &str, v: &str) {
    match section.to_ascii_lowercase().as_str() {
        "display" => apply_display(&mut core.display, k, v),
        "window" => apply_window(&mut core.window, k, v),
        "framerate" => apply_framerate(&mut core.framerate, k, v),
        "input" => apply_input(&mut core.input, k, v),
        "process" => apply_process(&mut core.process, k, v),
        "log" => apply_log(&mut core.log, k, v),
        _ => {}
    }
}

impl CoreConfig {
    /// Parses INI text using only the shared section dispatcher.
    /// Game crates without game-specific config keys should call this directly from `install_hooks`.
    /// Game crates with such keys should use [`ResolutionConfig::parse`] or [`ResolutionConfigExt::parse`] instead.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut core = Self::default();
        for_each_setting(text, |section, k, v| {
            apply_core_setting(&mut core, section, k, v);
        });
        core
    }
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
        fmt_opt(win.x.as_ref()),
        fmt_opt(win.y.as_ref()),
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

fn fmt_opt<T: Display>(v: Option<&T>) -> Cow<'static, str> {
    v.map_or_else(|| "auto".into(), |v| v.to_string().into())
}

fn fmt_mask(v: Option<NonZero<u32>>) -> Cow<'static, str> {
    v.map_or_else(
        || "0 (default)".into(),
        |v| format!("{:#x}", v.get()).into(),
    )
}

/// Back buffer size for games with a `[display] resolution` setting.
/// Games whose own display mode includes borderless ignore it there and size to the desktop.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resolution {
    R640x480 = 0,
    R960x720 = 1,
    #[default]
    R1280x960 = 2,
}

impl Resolution {
    #[must_use]
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// Returns the index of the resolution radio button for this size and `mode` in the option-dialog layout used by th18 and th20.
    #[must_use]
    pub fn radio_index(self, mode: DisplayModeExt) -> u8 {
        match (mode, self) {
            (DisplayModeExt::Fullscreen, Self::R640x480) => 0,
            (DisplayModeExt::Fullscreen, Self::R960x720) => 1,
            (DisplayModeExt::Fullscreen, Self::R1280x960) => 2,
            (DisplayModeExt::Windowed, Self::R640x480) => 3,
            (DisplayModeExt::Windowed, Self::R960x720) => 4,
            (DisplayModeExt::Windowed, Self::R1280x960) => 5,
            (DisplayModeExt::Borderless, _) => 8,
        }
    }

    /// Returns the index of the render scale for this size and `mode` into the render scale table used by th18 and th20.
    #[must_use]
    pub fn scale_index(self, mode: DisplayModeExt) -> u8 {
        match self.radio_index(mode) {
            i @ 0..=2 => i,
            i @ 3..=5 => i - 3,
            8 => 5,
            _ => unreachable!("radio_index produces only 0..=5 or 8"),
        }
    }

    #[must_use]
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::R640x480 => (640, 480),
            Self::R960x720 => (960, 720),
            Self::R1280x960 => (1280, 960),
        }
    }
}

impl Display for Resolution {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let (w, h) = self.dimensions();
        write!(f, "{w}x{h}")
    }
}

/// [`DisplayMode`] extended with borderless.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayModeExt {
    #[default]
    Windowed,
    Fullscreen,
    Borderless,
}

impl DisplayModeExt {
    #[must_use]
    pub fn to_core(self) -> DisplayMode {
        match self {
            Self::Windowed => DisplayMode::Windowed,
            Self::Fullscreen | Self::Borderless => DisplayMode::Fullscreen,
        }
    }
}

impl Display for DisplayModeExt {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Windowed => "Windowed",
            Self::Fullscreen => "Fullscreen",
            Self::Borderless => "Borderless",
        })
    }
}

#[must_use]
fn parse_resolution(v: &str) -> Option<Resolution> {
    match v.to_ascii_lowercase().as_str() {
        "640x480" => Some(Resolution::R640x480),
        "960x720" => Some(Resolution::R960x720),
        "1280x960" => Some(Resolution::R1280x960),
        _ => None,
    }
}

#[must_use]
fn parse_display_mode_ext(v: &str) -> Option<DisplayModeExt> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(DisplayModeExt::Windowed),
        "fullscreen" => Some(DisplayModeExt::Fullscreen),
        "borderless" => Some(DisplayModeExt::Borderless),
        _ => None,
    }
}

/// Game-specific configuration for the games whose startup dialog exposes only a resolution choice (th14–th17).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResolutionConfig {
    pub resolution: Resolution,
}

impl ResolutionConfig {
    /// Parses INI text into this configuration plus the core configuration, with defaults for any keys/sections the text omits.
    #[must_use]
    pub fn parse(text: &str) -> (Self, CoreConfig) {
        let mut game = Self::default();
        let mut core = CoreConfig::default();
        for_each_setting(text, |section, k, v| {
            if section.eq_ignore_ascii_case("display") && k.eq_ignore_ascii_case("resolution") {
                if let Some(r) = parse_resolution(v) {
                    game.resolution = r;
                }
            } else {
                apply_core_setting(&mut core, section, k, v);
            }
        });
        (game, core)
    }

    /// Writes the game-specific manifest lines that aren't already covered by the core configuration.
    ///
    /// # Errors
    /// Propagates errors from writing to `w`.
    pub fn write_manifest_extras<W: Write + ?Sized>(&self, w: &mut W) -> IoResult<()> {
        writeln!(w, "display.resolution={}", self.resolution)
    }
}

/// [`ResolutionConfig`] extended with the game's own display mode, for the games whose `[display] mode` includes borderless (th18, th20).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResolutionConfigExt {
    pub display_mode: DisplayModeExt,
    pub resolution: Resolution,
}

impl ResolutionConfigExt {
    /// Parses INI text into this configuration plus the core configuration, with defaults for any keys/sections the text omits.
    #[must_use]
    pub fn parse(text: &str) -> (Self, CoreConfig) {
        let mut game = Self::default();
        let mut core = CoreConfig::default();
        for_each_setting(text, |section, k, v| {
            if section.eq_ignore_ascii_case("display") && k.eq_ignore_ascii_case("mode") {
                if let Some(m) = parse_display_mode_ext(v) {
                    game.display_mode = m;
                }
            } else if section.eq_ignore_ascii_case("display")
                && k.eq_ignore_ascii_case("resolution")
            {
                if let Some(r) = parse_resolution(v) {
                    game.resolution = r;
                }
            } else {
                apply_core_setting(&mut core, section, k, v);
            }
        });
        core.display.mode = game.display_mode.to_core();
        (game, core)
    }

    /// Writes the game-specific manifest lines that aren't already covered by the core configuration.
    ///
    /// # Errors
    /// Propagates errors from writing to `w`.
    pub fn write_manifest_extras<W: Write + ?Sized>(&self, w: &mut W) -> IoResult<()> {
        if self.display_mode == DisplayModeExt::Borderless {
            writeln!(w, "display.resolution=auto (Borderless)")
        } else {
            writeln!(w, "display.resolution={}", self.resolution)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CoreConfig, DEFAULT_GAME_FPS, DEFAULT_REPLAY_SKIP_FPS, DEFAULT_REPLAY_SLOW_FPS,
        DEFAULT_SESSIONS_TO_KEEP, DisplayCfg, DisplayMode, DisplayModeExt, FramerateCfg, InputCfg,
        LogCfg, PriorityClass, ProcessCfg, RefreshRateMode, Resolution, ResolutionConfig,
        ResolutionConfigExt, WindowCfg, WindowFrame, apply_display, apply_framerate, apply_input,
        apply_log, apply_process, apply_window, decode_text, for_each_setting, parse_bitmask,
        parse_bool, parse_display_mode_ext, parse_priority_class, parse_refresh_rate,
        parse_resolution, parse_u32, parse_window_frame, read_ini_text, unquote,
    };
    use std::num::NonZero;
    use std::path::Path;
    use tracing::level_filters::LevelFilter;

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
    }

    #[test]
    fn read_ini_text_missing_file() {
        assert_eq!(read_ini_text(None), "");
        let missing = Path::new("nonexistent_dir_for_test");
        assert_eq!(read_ini_text(Some(missing)), "");
    }

    #[test]
    fn decode_text_strip_bom() {
        assert_eq!(decode_text(b"\xef\xbb\xbfhello"), "hello");
        assert_eq!(decode_text(b"hello"), "hello");
        // Only one BOM is stripped.
        assert_eq!(decode_text(b"\xef\xbb\xbf\xef\xbb\xbfx"), "\u{feff}x",);
    }

    #[test]
    fn decode_text_utf16_boms() {
        let mut le = vec![0xff, 0xfe];
        for u in "[a]\nk = 1".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_text(&le), "[a]\nk = 1");

        let mut be = vec![0xfe, 0xff];
        for u in "x = y".encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(decode_text(&be), "x = y");

        le.push(0x41);
        assert_eq!(decode_text(&le), "[a]\nk = 1");
    }

    type Setting = (String, String, String);

    fn settings(text: &str) -> Vec<Setting> {
        let mut out = Vec::new();
        for_each_setting(text, |s, k, v| {
            out.push((s.to_string(), k.to_string(), v.to_string()));
        });
        out
    }

    fn one(section: &str, key: &str, value: &str) -> Vec<Setting> {
        vec![(section.into(), key.into(), value.into())]
    }

    #[test]
    fn parse_comments_quotes_headers() {
        let cases = &[
            ("[a]\nk = v ; note", one("a", "k", "v")),
            ("[a]\nk = 120;faster", one("a", "k", "120")),
            ("[a]\nk = 1#gamepad", one("a", "k", "1")),
            (
                "[a]\nk = D:\\Marisa's Logs   ; keep",
                one("a", "k", "D:\\Marisa's Logs"),
            ),
            (
                "[a]\nk = 'D:\\Touhou #2\\logs'",
                one("a", "k", "D:\\Touhou #2\\logs"),
            ),
            ("[a]\nk = \"C:\\foo;bar\"", one("a", "k", "C:\\foo;bar")),
            ("[a]\nk = \"a ; b\" ; c", one("a", "k", "a ; b")),
            (
                "[a]\nk = 'unterminated ; note",
                one("a", "k", "'unterminated"),
            ),
            ("[a]\nk = 'D:\\Marisa's Logs'", one("a", "k", "D:\\Marisa")),
            (
                "[a]\nk = \"D:\\Marisa's Logs\"",
                one("a", "k", "D:\\Marisa's Logs"),
            ),
            ("[a]\n; k = v", vec![]),
            ("[a]\n# k = v", vec![]),
            ("[a] ; note\nk = v", one("a", "k", "v")),
            ("[input]\ndpad ; turn this off = false", vec![]),
            ("[input]\nNOTE # see dpad = 0 to disable", vec![]),
            ("[input]\ndpad = 1 ; 0 = off", one("input", "dpad", "1")),
            ("[a] ; note = x\nk = v", one("a", "k", "v")),
            ("[a] # note = x\nk = v", one("a", "k", "v")),
            ("[a junk = 1\nk = v", one("", "k", "v")),
        ];
        for (text, expected) in cases {
            assert_eq!(&settings(text), expected, "{text:?}");
        }
    }

    #[test]
    fn unquote_strip_matching_pairs() {
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
    fn parse_bool_aliases() {
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
    fn parse_bitmask_radix_prefixes() {
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
    fn parse_refresh_rate_fallback() {
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
        assert_eq!(parse_refresh_rate("0"), None);
        assert_eq!(parse_refresh_rate("0x0"), None);
    }

    #[test]
    fn parse_window_frame_variants() {
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
    fn apply_log_set_known_keys() {
        let mut cfg = LogCfg::default();
        apply_log(&mut cfg, "level", "off");
        // Zero sessions would delete the session being written, so it falls back to the default.
        apply_log(&mut cfg, "sessions_to_keep", "0");
        apply_log(&mut cfg, "log_dir", "");
        assert_eq!(cfg.level, LevelFilter::OFF);
        assert_eq!(cfg.sessions_to_keep, DEFAULT_SESSIONS_TO_KEEP);
        assert_eq!(cfg.log_dir, None);
    }

    #[test]
    fn apply_display_ignore_unknown_keys() {
        // Game-specific keys should be silently skipped by `apply_display` here.
        let mut cfg = DisplayCfg::default();
        let baseline_mode = cfg.mode;
        let baseline_rate = cfg.refresh_rate;
        apply_display(&mut cfg, "resolution", "640x480");
        assert_eq!(cfg.mode, baseline_mode);
        assert_eq!(cfg.refresh_rate, baseline_rate);
    }

    #[test]
    fn apply_framerate_set_known_keys() {
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
        // An unset dimension remains `None` so the game-derived value is used.
        assert_eq!(cfg.height, None);
        assert_eq!(cfg.frame, Some(WindowFrame::Borderless));
        assert!(cfg.always_on_top);
    }

    #[test]
    fn zero_means_unset() {
        let mut window = WindowCfg::default();
        apply_window(&mut window, "width", "0");
        apply_window(&mut window, "height", "0");
        assert_eq!(window.width, None);
        assert_eq!(window.height, None);

        let mut process = ProcessCfg::default();
        apply_process(&mut process, "affinity_mask", "0");
        assert_eq!(process.affinity_mask, None);
    }

    #[test]
    fn default_core_config_matches_documented_defaults() {
        let cfg = CoreConfig::default();
        assert_eq!(cfg.display.mode, DisplayMode::Windowed);
        assert_eq!(cfg.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(cfg.window.x, None);
        assert_eq!(cfg.window.y, None);
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
        assert_eq!(CoreConfig::parse(""), cfg);
    }

    #[test]
    fn apply_input_dpad_toggle() {
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
    fn core_config_apply_known_keys() {
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
        let cfg = CoreConfig::parse(text);
        assert_eq!(cfg.framerate.game_fps, 120);
        assert_eq!(cfg.framerate.replay_skip_fps, 480);
        assert_eq!(cfg.process.priority, PriorityClass::High);
        assert_eq!(cfg.process.affinity_mask, Some(nz(0xff)));
        assert_eq!(cfg.display.mode, DisplayMode::Fullscreen);
    }

    #[test]
    fn parse_resolution_setting() {
        assert_eq!(parse_resolution("640x480"), Some(Resolution::R640x480));
        assert_eq!(parse_resolution("960x720"), Some(Resolution::R960x720));
        assert_eq!(parse_resolution("1280x960"), Some(Resolution::R1280x960));
        assert_eq!(parse_resolution("1920x1080"), None);
        assert_eq!(parse_resolution("borderless"), None);
    }

    #[test]
    fn parse_display_mode_ext_setting() {
        assert_eq!(
            parse_display_mode_ext("windowed"),
            Some(DisplayModeExt::Windowed)
        );
        assert_eq!(
            parse_display_mode_ext("Fullscreen"),
            Some(DisplayModeExt::Fullscreen)
        );
        assert_eq!(
            parse_display_mode_ext("BORDERLESS"),
            Some(DisplayModeExt::Borderless)
        );
        assert_eq!(parse_display_mode_ext("idk"), None);
    }

    #[test]
    fn dialog_indices_match_startup_dialog() {
        // Borderless has no resolution buttons of its own, so all three sizes collapse onto one pair.
        let cases = [
            (DisplayModeExt::Fullscreen, Resolution::R640x480, 0, 0),
            (DisplayModeExt::Fullscreen, Resolution::R960x720, 1, 1),
            (DisplayModeExt::Fullscreen, Resolution::R1280x960, 2, 2),
            (DisplayModeExt::Windowed, Resolution::R640x480, 3, 0),
            (DisplayModeExt::Windowed, Resolution::R960x720, 4, 1),
            (DisplayModeExt::Windowed, Resolution::R1280x960, 5, 2),
            (DisplayModeExt::Borderless, Resolution::R640x480, 8, 5),
            (DisplayModeExt::Borderless, Resolution::R960x720, 8, 5),
            (DisplayModeExt::Borderless, Resolution::R1280x960, 8, 5),
        ];

        for (mode, res, radio, scale) in cases {
            assert_eq!(res.radio_index(mode), radio, "{mode:?} {res:?} radio");
            assert_eq!(res.scale_index(mode), scale, "{mode:?} {res:?} scale");
        }
    }

    #[test]
    fn to_core_borderless() {
        assert_eq!(DisplayModeExt::Windowed.to_core(), DisplayMode::Windowed);
        assert_eq!(
            DisplayModeExt::Fullscreen.to_core(),
            DisplayMode::Fullscreen
        );
        assert_eq!(
            DisplayModeExt::Borderless.to_core(),
            DisplayMode::Fullscreen
        );
    }

    #[test]
    fn default_resolution_config_matches_documented_defaults() {
        let (game, core) = ResolutionConfig::parse("");
        assert_eq!(game.resolution, Resolution::R1280x960);
        assert_eq!(core, CoreConfig::default());
    }

    #[test]
    fn resolution_config_apply_known_keys() {
        let text = "
            [framerate]
            game_fps = 120

            [display]
            resolution = 960x720
        ";
        let (game, core) = ResolutionConfig::parse(text);
        assert_eq!(game.resolution, Resolution::R960x720);
        assert_eq!(core.framerate.game_fps, 120);
    }

    #[test]
    fn default_resolution_config_ext_matches_documented_defaults() {
        let (game, core) = ResolutionConfigExt::parse("");
        assert_eq!(game.display_mode, DisplayModeExt::Windowed);
        assert_eq!(game.resolution, Resolution::R1280x960);
        assert_eq!(core, CoreConfig::default());
    }

    #[test]
    fn resolution_config_ext_apply_known_keys() {
        let text = "
            [framerate]
            game_fps = 120

            [display]
            mode = Borderless
            resolution = 960x720
        ";
        let (game, core) = ResolutionConfigExt::parse(text);
        assert_eq!(game.display_mode, DisplayModeExt::Borderless);
        assert_eq!(game.resolution, Resolution::R960x720);
        assert_eq!(core.display.mode, DisplayMode::Fullscreen);
        assert_eq!(core.framerate.game_fps, 120);
    }

    #[test]
    fn core_config_ignore_unknown_sections_and_keys() {
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
        let cfg = CoreConfig::parse(text);
        assert_eq!(cfg.framerate.game_fps, DEFAULT_GAME_FPS);
    }
}
