//! th15-specific configuration.

use neopatch_core::config::{CoreConfig, for_each_setting, parse_core_only};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Result as IoResult, Write};
use std::sync::OnceLock;

pub(crate) static CONFIG: OnceLock<Th15Config> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Th15Config {
    pub(crate) resolution: Resolution,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Resolution {
    R640x480 = 0,
    R960x720 = 1,
    #[default]
    R1280x960 = 2,
}

const _: () = {
    assert!(Resolution::R640x480.index() == 0);
    assert!(Resolution::R960x720.index() == 1);
    assert!(Resolution::R1280x960.index() == 2);
};

impl Resolution {
    pub(crate) const fn index(self) -> u8 {
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

impl Display for Resolution {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::R640x480 => "640x480",
            Self::R960x720 => "960x720",
            Self::R1280x960 => "1280x960",
        })
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
pub(crate) fn parse_config(text: &str) -> (Th15Config, CoreConfig) {
    (parse_th15_only(text), parse_core_only(text))
}

fn parse_th15_only(text: &str) -> Th15Config {
    let mut cfg = Th15Config::default();
    for_each_setting(text, |section, k, v| {
        if section.eq_ignore_ascii_case("display")
            && k.eq_ignore_ascii_case("resolution")
            && let Some(r) = parse_resolution(v)
        {
            cfg.resolution = r;
        }
    });
    cfg
}

/// Writes th15-specific manifest lines that aren't already covered by the core configuration.
pub(crate) fn write_manifest_extras<W: Write + ?Sized>(
    w: &mut W,
    th15: &Th15Config,
) -> IoResult<()> {
    writeln!(w, "display.resolution={}", th15.resolution)
}

#[cfg(test)]
mod tests {
    use super::{Resolution, parse_config, parse_resolution};
    use neopatch_core::config::{CoreConfig, DisplayMode};

    #[test]
    fn parse_resolution_rejects_unsupported() {
        assert_eq!(parse_resolution("640x480"), Some(Resolution::R640x480));
        assert_eq!(parse_resolution("960x720"), Some(Resolution::R960x720));
        assert_eq!(parse_resolution("1280x960"), Some(Resolution::R1280x960));
        assert_eq!(parse_resolution("1920x1080"), None);
    }

    #[test]
    fn default_matches_documented_defaults() {
        let (th15, core) = parse_config("");
        assert_eq!(th15.resolution, Resolution::R1280x960);
        assert_eq!(core, CoreConfig::default());
    }

    #[test]
    fn parse_applies_known_keys() {
        let text = "
            [framerate]
            game_fps = 120

            [display]
            resolution = 960x720
        ";
        let (th15, core) = parse_config(text);
        assert_eq!(th15.resolution, Resolution::R960x720);
        assert_eq!(core.framerate.game_fps, 120);
    }

    #[test]
    fn parse_handles_quoted_values_and_comments() {
        let (th15, core) = parse_config(
            "[display]\nmode = \"fullscreen\" ; trailing comment\nresolution = '960x720'",
        );
        assert_eq!(core.display.mode, DisplayMode::Fullscreen);
        assert_eq!(th15.resolution, Resolution::R960x720);
    }
}
