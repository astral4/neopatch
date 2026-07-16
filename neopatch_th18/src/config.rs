//! th18-specific configuration.

use neopatch_core::config::{
    CoreConfig, DisplayMode as CoreDisplayMode, for_each_setting, parse_core_only,
};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Result as IoResult, Write};
use std::sync::OnceLock;

pub(crate) static CONFIG: OnceLock<Th18Config> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Th18Config {
    pub display_mode: Th18DisplayMode,
    pub resolution: Resolution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Th18DisplayMode {
    #[default]
    Windowed,
    Fullscreen,
    Borderless,
}

impl Th18DisplayMode {
    pub(crate) fn to_core(self) -> CoreDisplayMode {
        match self {
            Self::Windowed => CoreDisplayMode::Windowed,
            Self::Fullscreen | Self::Borderless => CoreDisplayMode::Fullscreen,
        }
    }
}

impl Display for Th18DisplayMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Windowed => "Windowed",
            Self::Fullscreen => "Fullscreen",
            Self::Borderless => "Borderless",
        })
    }
}

/// Back buffer size for `Windowed` and `Fullscreen`. Ignored under `Borderless`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Resolution {
    R640x480,
    R960x720,
    #[default]
    R1280x960,
}

impl Resolution {
    /// Index of the startup dialog's resolution radio button for this size and `mode`.
    pub(crate) fn radio_index(self, mode: Th18DisplayMode) -> u8 {
        match (mode, self) {
            (Th18DisplayMode::Fullscreen, Self::R640x480) => 0,
            (Th18DisplayMode::Fullscreen, Self::R960x720) => 1,
            (Th18DisplayMode::Fullscreen, Self::R1280x960) => 2,
            (Th18DisplayMode::Windowed, Self::R640x480) => 3,
            (Th18DisplayMode::Windowed, Self::R960x720) => 4,
            (Th18DisplayMode::Windowed, Self::R1280x960) => 5,
            (Th18DisplayMode::Borderless, _) => 8,
        }
    }

    /// Index of the render scale for this size and `mode`.
    pub(crate) fn scale_index(self, mode: Th18DisplayMode) -> u8 {
        match self.radio_index(mode) {
            i @ 0..=2 => i,
            i @ 3..=5 => i - 3,
            8 => 5,
            _ => unreachable!("radio_index produces only 0..=5 or 8"),
        }
    }

    pub(crate) fn dimensions(self) -> (u32, u32) {
        match self {
            Self::R640x480 => (640, 480),
            Self::R960x720 => (960, 720),
            Self::R1280x960 => (1280, 960),
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

fn parse_display_mode(v: &str) -> Option<Th18DisplayMode> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(Th18DisplayMode::Windowed),
        "fullscreen" => Some(Th18DisplayMode::Fullscreen),
        "borderless" => Some(Th18DisplayMode::Borderless),
        _ => None,
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

/// Parses INI text into a configuration, with defaults for any keys/sections the text omits.
pub(crate) fn parse_config(text: &str) -> (Th18Config, CoreConfig) {
    let th18 = parse_th18_only(text);
    let mut core = parse_core_only(text);
    core.display.mode = th18.display_mode.to_core();
    (th18, core)
}

fn parse_th18_only(text: &str) -> Th18Config {
    let mut cfg = Th18Config::default();
    for_each_setting(text, |section, k, v| {
        if section.eq_ignore_ascii_case("display") {
            if k.eq_ignore_ascii_case("mode") {
                if let Some(m) = parse_display_mode(v) {
                    cfg.display_mode = m;
                }
            } else if k.eq_ignore_ascii_case("resolution")
                && let Some(r) = parse_resolution(v)
            {
                cfg.resolution = r;
            }
        }
    });
    cfg
}

/// Writes th18-specific manifest lines that aren't already covered by the core configuration.
/// Under borderless, the resolution line carries a sentinel because the value we'd log gets overridden.
pub(crate) fn write_manifest_extras<W: Write + ?Sized>(
    w: &mut W,
    th18: &Th18Config,
) -> IoResult<()> {
    if th18.display_mode == Th18DisplayMode::Borderless {
        writeln!(w, "display.resolution=auto (Borderless)")
    } else {
        writeln!(w, "display.resolution={}", th18.resolution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neopatch_core::config::{DisplayMode as CoreDisplayMode, PriorityClass, RefreshRateMode};
    use std::num::NonZero;

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
    }

    #[test]
    fn parse_resolution_accepts_canonical_trio() {
        assert_eq!(parse_resolution("640x480"), Some(Resolution::R640x480));
        assert_eq!(parse_resolution("960x720"), Some(Resolution::R960x720));
        assert_eq!(parse_resolution("1280x960"), Some(Resolution::R1280x960));
        assert_eq!(parse_resolution("1920x1080"), None);
        assert_eq!(parse_resolution("borderless"), None);
    }

    #[test]
    fn parse_display_mode_accepts_all_three() {
        assert_eq!(
            parse_display_mode("windowed"),
            Some(Th18DisplayMode::Windowed)
        );
        assert_eq!(
            parse_display_mode("Fullscreen"),
            Some(Th18DisplayMode::Fullscreen)
        );
        assert_eq!(
            parse_display_mode("BORDERLESS"),
            Some(Th18DisplayMode::Borderless)
        );
        assert_eq!(parse_display_mode("idk"), None);
    }

    #[test]
    fn radio_index_combines_mode_and_resolution() {
        assert_eq!(
            Resolution::R640x480.radio_index(Th18DisplayMode::Fullscreen),
            0,
        );
        assert_eq!(
            Resolution::R960x720.radio_index(Th18DisplayMode::Fullscreen),
            1,
        );
        assert_eq!(
            Resolution::R1280x960.radio_index(Th18DisplayMode::Fullscreen),
            2,
        );
        assert_eq!(
            Resolution::R640x480.radio_index(Th18DisplayMode::Windowed),
            3,
        );
        assert_eq!(
            Resolution::R960x720.radio_index(Th18DisplayMode::Windowed),
            4,
        );
        assert_eq!(
            Resolution::R1280x960.radio_index(Th18DisplayMode::Windowed),
            5,
        );
        assert_eq!(
            Resolution::R640x480.radio_index(Th18DisplayMode::Borderless),
            8,
        );
        assert_eq!(
            Resolution::R960x720.radio_index(Th18DisplayMode::Borderless),
            8,
        );
        assert_eq!(
            Resolution::R1280x960.radio_index(Th18DisplayMode::Borderless),
            8,
        );
    }

    #[test]
    fn scale_index_matches_dat_004b7fbc() {
        for r in [
            Resolution::R640x480,
            Resolution::R960x720,
            Resolution::R1280x960,
        ] {
            assert_eq!(
                r.scale_index(Th18DisplayMode::Fullscreen),
                r.radio_index(Th18DisplayMode::Fullscreen),
            );
            assert_eq!(
                r.scale_index(Th18DisplayMode::Windowed),
                r.radio_index(Th18DisplayMode::Windowed) - 3,
            );
            assert_eq!(r.scale_index(Th18DisplayMode::Borderless), 5);
        }
    }

    #[test]
    fn to_core_collapses_borderless_into_fullscreen() {
        assert_eq!(
            Th18DisplayMode::Windowed.to_core(),
            CoreDisplayMode::Windowed,
        );
        assert_eq!(
            Th18DisplayMode::Fullscreen.to_core(),
            CoreDisplayMode::Fullscreen,
        );
        assert_eq!(
            Th18DisplayMode::Borderless.to_core(),
            CoreDisplayMode::Fullscreen,
        );
    }

    #[test]
    fn default_matches_documented_defaults() {
        let (th18, core) = parse_config("");
        assert_eq!(th18.display_mode, Th18DisplayMode::Windowed);
        assert_eq!(core.display.mode, CoreDisplayMode::Windowed);
        assert_eq!(core.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(th18.resolution, Resolution::R1280x960);
        assert_eq!(core.framerate.game_fps, 60);
    }

    #[test]
    fn parse_applies_known_keys() {
        let text = "
            [framerate]
            game_fps = 120
            replay_skip_fps = 480

            [process]
            priority = High
            affinity_mask = 0xFF

            [display]
            mode = Borderless
            resolution = 960x720
        ";
        let (th18, core) = parse_config(text);
        assert_eq!(core.framerate.game_fps, 120);
        assert_eq!(core.framerate.replay_skip_fps, 480);
        assert_eq!(core.process.priority, PriorityClass::High);
        assert_eq!(core.process.affinity_mask, Some(nz(0xff)));
        assert_eq!(th18.display_mode, Th18DisplayMode::Borderless);
        assert_eq!(core.display.mode, CoreDisplayMode::Fullscreen);
        assert_eq!(th18.resolution, Resolution::R960x720);
    }

    #[test]
    fn parse_handles_quoted_values_and_comments() {
        let (th18, core) = parse_config(
            "[display]\nmode = \"borderless\" ; trailing comment\nresolution = '960x720'",
        );
        assert_eq!(th18.display_mode, Th18DisplayMode::Borderless);
        assert_eq!(core.display.mode, CoreDisplayMode::Fullscreen);
        assert_eq!(th18.resolution, Resolution::R960x720);
    }
}
