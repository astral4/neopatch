//! Logic for pinning `GetDeviceCaps(_, VREFRESH)` to 60 Hz.
//!
//! The game's animation timing is built around 60 Hz, but the actual scanout rate
//! is independent because our frame pacer drives the cadence of frames and logic ticks.

use crate::iat_hook;
use crate::log::LogCap;
use std::num::NonZero;
use tracing::info;
use windows::Win32::Graphics::Gdi::{HDC, VREFRESH as SDK_VREFRESH};
use windows_sys::Win32::Foundation::HMODULE;

// `cast_signed` preserves the bit pattern.
const VREFRESH: i32 = SDK_VREFRESH.0.cast_signed();

iat_hook! {
    REAL_GET_DEVICE_CAPS / real_get_device_caps : "GetDeviceCaps"
        as fn(hdc: HDC, index: i32) -> i32;
}

static VREFRESH_LOG: LogCap = LogCap::new(NonZero::new(1).unwrap());

/// IAT-hooks `GetDeviceCaps` against `host`'s import table.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        REAL_GET_DEVICE_CAPS.install(host, hook_get_device_caps);
    }
}

unsafe extern "system" fn hook_get_device_caps(hdc: HDC, index: i32) -> i32 {
    unsafe {
        if index == VREFRESH {
            if VREFRESH_LOG.tick().is_some() {
                info!(kind = "vrefresh_spoof", spoofed_value = 60);
            }
            return 60;
        }
        real_get_device_caps(hdc, index)
    }
}
