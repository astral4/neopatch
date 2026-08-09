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
    pub(crate) display_mode: Th18DisplayMode,
    pub(crate) resolution: Resolution,
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

    /// Index of the render scale for this size and `mode`. Indexes the game's scale table at `0x4b7fbc`.
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
    use super::{Resolution, Th18DisplayMode, parse_config, parse_display_mode, parse_resolution};
    use neopatch_core::config::{CoreConfig, DisplayMode as CoreDisplayMode};

    #[test]
    fn parse_resolution_setting() {
        assert_eq!(parse_resolution("640x480"), Some(Resolution::R640x480));
        assert_eq!(parse_resolution("960x720"), Some(Resolution::R960x720));
        assert_eq!(parse_resolution("1280x960"), Some(Resolution::R1280x960));
        assert_eq!(parse_resolution("1920x1080"), None);
        assert_eq!(parse_resolution("borderless"), None);
    }

    #[test]
    fn parse_display_mode_setting() {
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
    fn dialog_indices_match_startup_dialog() {
        // Borderless has no resolution buttons of its own, so all three sizes collapse onto one pair.
        let cases = [
            (Th18DisplayMode::Fullscreen, Resolution::R640x480, 0, 0),
            (Th18DisplayMode::Fullscreen, Resolution::R960x720, 1, 1),
            (Th18DisplayMode::Fullscreen, Resolution::R1280x960, 2, 2),
            (Th18DisplayMode::Windowed, Resolution::R640x480, 3, 0),
            (Th18DisplayMode::Windowed, Resolution::R960x720, 4, 1),
            (Th18DisplayMode::Windowed, Resolution::R1280x960, 5, 2),
            (Th18DisplayMode::Borderless, Resolution::R640x480, 8, 5),
            (Th18DisplayMode::Borderless, Resolution::R960x720, 8, 5),
            (Th18DisplayMode::Borderless, Resolution::R1280x960, 8, 5),
        ];

        for (mode, res, radio, scale) in cases {
            assert_eq!(res.radio_index(mode), radio, "{mode:?} {res:?} radio");
            assert_eq!(res.scale_index(mode), scale, "{mode:?} {res:?} scale");
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
        assert_eq!(th18.resolution, Resolution::R1280x960);
        assert_eq!(core, CoreConfig::default());
    }

    #[test]
    fn parse_applies_known_keys() {
        let text = "
            [framerate]
            game_fps = 120

            [display]
            mode = Borderless
            resolution = 960x720
        ";
        let (th18, core) = parse_config(text);
        assert_eq!(th18.display_mode, Th18DisplayMode::Borderless);
        assert_eq!(th18.resolution, Resolution::R960x720);
        // The game's own display mode is what reaches the core config.
        assert_eq!(core.display.mode, CoreDisplayMode::Fullscreen);
        assert_eq!(core.framerate.game_fps, 120);
    }
}
