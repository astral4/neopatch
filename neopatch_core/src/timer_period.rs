//! Logic for pinning the scheduler timer resolution at 1 ms.
//!
//! The game's per-frame `timeBeginPeriod(1)` / `timeEndPeriod(1)` round-trip makes the resolution flap each frame,
//! so we bump it once and stub the game's calls.

use crate::iat_hook;
use crate::log::log_at;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Media::{MMSYSERR_NOERROR, timeBeginPeriod};

iat_hook! {
    REAL_TIME_BEGIN_PERIOD / real_time_begin_period : "timeBeginPeriod"
        as fn(period: u32) -> u32;
}
iat_hook! {
    REAL_TIME_END_PERIOD / real_time_end_period : "timeEndPeriod"
        as fn(period: u32) -> u32;
}

extern "system" fn stub_time_begin_period(_period: u32) -> u32 {
    MMSYSERR_NOERROR
}

extern "system" fn stub_time_end_period(_period: u32) -> u32 {
    MMSYSERR_NOERROR
}

/// Pins the multimedia timer resolution at 1 ms and stubs out
/// the host's own `timeBeginPeriod` / `timeEndPeriod` calls so they can't change it.
///
/// # Safety
/// `host` must be a loaded module handle.
pub unsafe fn install(host: HMODULE) {
    unsafe {
        // We never call `timeEndPeriod`, so the resolution holds.
        let pin_ok = timeBeginPeriod(1) == MMSYSERR_NOERROR;
        let begin = pin_ok && REAL_TIME_BEGIN_PERIOD.install(host, stub_time_begin_period);
        let end = pin_ok && REAL_TIME_END_PERIOD.install(host, stub_time_end_period);
        log_at!(pin_ok && begin == end => info / warn,
            kind = "timer_period_stubs",
            pin_ok,
            begin_period = begin,
            end_period = end,
        );
    }
}
