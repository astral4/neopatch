//! th16-specific configuration.

use neopatch_core::config::{self, CoreConfig};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Result as IoResult, Write};
use std::sync::OnceLock;

#[derive(Default)]
pub(crate) struct Th16Config {
    pub resolution: Resolution,
}

pub(crate) static CONFIG: OnceLock<Th16Config> = OnceLock::new();

// Important: discriminants are load-bearing! They're the resolution component of:
// - the display-mode byte (`byte % 3`, with `byte / 3 == 0` selecting fullscreen and
//   `== 1` selecting windowed)
// - the offset from `RES_RADIO_FIRST_ID` (`0xCD`) for the dialog radio control IDs
// - the index into the `ascii.anm` / `ascii_960.anm` / `ascii_1280.anm` font selection
//   done by the `AsciiInf` dispatcher
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Resolution {
    R640x480 = 0,
    R960x720 = 1,
    #[default]
    R1280x960 = 2,
}

const _: () = {
    assert!(Resolution::R640x480 as u8 == 0);
    assert!(Resolution::R960x720 as u8 == 1);
    assert!(Resolution::R1280x960 as u8 == 2);
};

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

/// Parses INI text into a `(Th16Config, CoreConfig)` pair,
/// with defaults for any keys/sections the text omits.
pub(crate) fn parse(text: &str) -> (Th16Config, CoreConfig) {
    (parse_th16_only(text), config::parse_core_only(text))
}

fn parse_th16_only(text: &str) -> Th16Config {
    let mut cfg = Th16Config::default();
    config::for_each_setting(text, |section, k, v| {
        if section.eq_ignore_ascii_case("display") && k.eq_ignore_ascii_case("resolution") {
            if let Some(r) = parse_resolution(v) {
                cfg.resolution = r;
            }
        }
    });
    cfg
}

/// Writes th16-specific manifest lines that aren't already covered by the core configuration.
pub(crate) fn write_manifest_extras<W: Write + ?Sized>(
    w: &mut W,
    th16: &Th16Config,
) -> IoResult<()> {
    writeln!(w, "display.resolution={}", th16.resolution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neopatch_core::config::{DisplayMode, PriorityClass, RefreshRateMode};
    use std::num::NonZero;

    fn nz(n: u32) -> NonZero<u32> {
        NonZero::new(n).unwrap()
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
    fn default_matches_documented_defaults() {
        let (th16, core) = parse("");
        assert_eq!(core.display.mode, DisplayMode::Windowed);
        assert_eq!(core.display.refresh_rate, RefreshRateMode::NativeMultiple);
        assert_eq!(th16.resolution, Resolution::R1280x960);
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
            resolution = 960x720
        ";
        let (th16, core) = parse(text);
        assert_eq!(core.framerate.game_fps, 120);
        assert_eq!(core.framerate.replay_skip_fps, 480);
        assert_eq!(core.process.priority, PriorityClass::High);
        assert_eq!(core.process.affinity_mask, Some(nz(0xFF)));
        assert_eq!(th16.resolution, Resolution::R960x720);
    }

    #[test]
    fn parse_handles_quoted_values_and_comments() {
        let (th16, core) =
            parse("[display]\nmode = \"fullscreen\" ; trailing comment\nresolution = '960x720'");
        assert_eq!(core.display.mode, DisplayMode::Fullscreen);
        assert_eq!(th16.resolution, Resolution::R960x720);
    }
}
