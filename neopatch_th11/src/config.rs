//! th11-specific configuration.

use neopatch_core::config::{self, CoreConfig};

/// Parses INI text into a `CoreConfig`, with defaults for any keys/sections omitted in `text`.
pub(crate) fn parse(text: &str) -> CoreConfig {
    let mut core = CoreConfig::default();
    config::for_each_setting(text, |section, k, v| {
        match section.to_ascii_lowercase().as_str() {
            "display" => config::apply_display(&mut core.display, k, v),
            "window" => config::apply_window(&mut core.window, k, v),
            "framerate" => config::apply_framerate(&mut core.framerate, k, v),
            "input" => config::apply_input(&mut core.input, k, v),
            "process" => config::apply_process(&mut core.process, k, v),
            "log" => config::apply_log(&mut core.log, k, v),
            _ => {}
        }
    });
    core
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
    fn default_matches_documented_defaults() {
        let core = parse("");
        assert_eq!(core.display.mode, DisplayMode::Windowed);
        assert_eq!(core.display.refresh_rate, RefreshRateMode::NativeMultiple);
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
            mode = fullscreen
        ";
        let core = parse(text);
        assert_eq!(core.framerate.game_fps, 120);
        assert_eq!(core.framerate.replay_skip_fps, 480);
        assert_eq!(core.process.priority, PriorityClass::High);
        assert_eq!(core.process.affinity_mask, Some(nz(0xFF)));
        assert_eq!(core.display.mode, DisplayMode::Fullscreen);
    }

    #[test]
    fn parse_silently_ignores_unknown() {
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
        let core = parse(text);
        assert_eq!(core.framerate.game_fps, 60);
    }

    #[test]
    fn parse_handles_quoted_values_and_comments() {
        let core = parse("[display]\nmode = \"fullscreen\" ; trailing comment");
        assert_eq!(core.display.mode, DisplayMode::Fullscreen);
    }

    #[test]
    fn resolution_key_is_silently_ignored() {
        let core = parse("[display]\nmode = windowed\nresolution = 1280x960");
        assert_eq!(core.display.mode, DisplayMode::Windowed);
    }
}
