//! th20-specific configuration.

use neopatch_core::config::{
    CoreConfig, DisplayMode as CoreDisplayMode, for_each_setting, parse_core_only,
};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Result as IoResult, Write};
use std::sync::OnceLock;

pub(crate) static CONFIG: OnceLock<Th20Config> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Th20Config {
    pub(crate) display_mode: Th20DisplayMode,
    pub(crate) resolution: Resolution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Th20DisplayMode {
    #[default]
    Windowed,
    Fullscreen,
    Borderless,
}

impl Th20DisplayMode {
    pub(crate) fn to_core(self) -> CoreDisplayMode {
        match self {
            Self::Windowed => CoreDisplayMode::Windowed,
            Self::Fullscreen | Self::Borderless => CoreDisplayMode::Fullscreen,
        }
    }
}

impl Display for Th20DisplayMode {
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
    pub(crate) fn radio_index(self, mode: Th20DisplayMode) -> u8 {
        match (mode, self) {
            (Th20DisplayMode::Fullscreen, Self::R640x480) => 0,
            (Th20DisplayMode::Fullscreen, Self::R960x720) => 1,
            (Th20DisplayMode::Fullscreen, Self::R1280x960) => 2,
            (Th20DisplayMode::Windowed, Self::R640x480) => 3,
            (Th20DisplayMode::Windowed, Self::R960x720) => 4,
            (Th20DisplayMode::Windowed, Self::R1280x960) => 5,
            (Th20DisplayMode::Borderless, _) => 8,
        }
    }

    /// Index of the render scale for this size and `mode`. Indexes the game's scale table at `0x5ae1d0`.
    pub(crate) fn scale_index(self, mode: Th20DisplayMode) -> u8 {
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

fn parse_display_mode(v: &str) -> Option<Th20DisplayMode> {
    match v.to_ascii_lowercase().as_str() {
        "windowed" => Some(Th20DisplayMode::Windowed),
        "fullscreen" => Some(Th20DisplayMode::Fullscreen),
        "borderless" => Some(Th20DisplayMode::Borderless),
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
pub(crate) fn parse_config(text: &str) -> (Th20Config, CoreConfig) {
    let th20 = parse_th20_only(text);
    let mut core = parse_core_only(text);
    core.display.mode = th20.display_mode.to_core();
    (th20, core)
}

fn parse_th20_only(text: &str) -> Th20Config {
    let mut cfg = Th20Config::default();
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

/// Writes th20-specific manifest lines that aren't already covered by the core configuration.
/// Under borderless, the resolution line carries a sentinel because the value we'd log gets overridden.
pub(crate) fn write_manifest_extras<W: Write + ?Sized>(
    w: &mut W,
    th20: &Th20Config,
) -> IoResult<()> {
    if th20.display_mode == Th20DisplayMode::Borderless {
        writeln!(w, "display.resolution=auto (Borderless)")
    } else {
        writeln!(w, "display.resolution={}", th20.resolution)
    }
}

#[cfg(test)]
mod tests {
    use super::{Resolution, Th20DisplayMode, parse_config, parse_display_mode, parse_resolution};
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
            Some(Th20DisplayMode::Windowed)
        );
        assert_eq!(
            parse_display_mode("Fullscreen"),
            Some(Th20DisplayMode::Fullscreen)
        );
        assert_eq!(
            parse_display_mode("BORDERLESS"),
            Some(Th20DisplayMode::Borderless)
        );
        assert_eq!(parse_display_mode("idk"), None);
    }

    #[test]
    fn dialog_indices_match_startup_dialog() {
        // Borderless has no resolution buttons of its own, so all three sizes collapse onto one pair.
        let cases = [
            (Th20DisplayMode::Fullscreen, Resolution::R640x480, 0, 0),
            (Th20DisplayMode::Fullscreen, Resolution::R960x720, 1, 1),
            (Th20DisplayMode::Fullscreen, Resolution::R1280x960, 2, 2),
            (Th20DisplayMode::Windowed, Resolution::R640x480, 3, 0),
            (Th20DisplayMode::Windowed, Resolution::R960x720, 4, 1),
            (Th20DisplayMode::Windowed, Resolution::R1280x960, 5, 2),
            (Th20DisplayMode::Borderless, Resolution::R640x480, 8, 5),
            (Th20DisplayMode::Borderless, Resolution::R960x720, 8, 5),
            (Th20DisplayMode::Borderless, Resolution::R1280x960, 8, 5),
        ];

        for (mode, res, radio, scale) in cases {
            assert_eq!(res.radio_index(mode), radio, "{mode:?} {res:?} radio");
            assert_eq!(res.scale_index(mode), scale, "{mode:?} {res:?} scale");
        }
    }

    #[test]
    fn to_core_collapses_borderless_into_fullscreen() {
        assert_eq!(
            Th20DisplayMode::Windowed.to_core(),
            CoreDisplayMode::Windowed,
        );
        assert_eq!(
            Th20DisplayMode::Fullscreen.to_core(),
            CoreDisplayMode::Fullscreen,
        );
        assert_eq!(
            Th20DisplayMode::Borderless.to_core(),
            CoreDisplayMode::Fullscreen,
        );
    }

    #[test]
    fn default_matches_documented_defaults() {
        let (th20, core) = parse_config("");
        assert_eq!(th20.display_mode, Th20DisplayMode::Windowed);
        assert_eq!(th20.resolution, Resolution::R1280x960);
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
        let (th20, core) = parse_config(text);
        assert_eq!(th20.display_mode, Th20DisplayMode::Borderless);
        assert_eq!(th20.resolution, Resolution::R960x720);
        // The game's own display mode is what reaches the core config.
        assert_eq!(core.display.mode, CoreDisplayMode::Fullscreen);
        assert_eq!(core.framerate.game_fps, 120);
    }
}
