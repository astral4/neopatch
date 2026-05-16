//! Logic for pinning the scheduler timer resolution at 1 ms.
//!
//! The game's per-frame `timeBeginPeriod(1)` / `timeEndPeriod(1)` round-trip
//! makes the resolution flap each frame, so we bump it once and stub the game's calls.
//! OILP also does this.

use crate::iat_hook;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Media::{MMSYSERR_NOERROR, timeBeginPeriod};

iat_hook! {
    REAL_TIME_BEGIN_PERIOD / real_time_begin_period : c"timeBeginPeriod"
        as fn(period: u32) -> u32;
}
iat_hook! {
    REAL_TIME_END_PERIOD / real_time_end_period : c"timeEndPeriod"
        as fn(period: u32) -> u32;
}

extern "system" fn stub_time_begin_period(_period: u32) -> u32 {
    MMSYSERR_NOERROR
}

extern "system" fn stub_time_end_period(_period: u32) -> u32 {
    MMSYSERR_NOERROR
}

pub(crate) unsafe fn install(host: HMODULE) {
    unsafe {
        // We never call `timeEndPeriod`, so the resolution holds.
        timeBeginPeriod(1);
        REAL_TIME_BEGIN_PERIOD.install(host, stub_time_begin_period as *mut ());
        REAL_TIME_END_PERIOD.install(host, stub_time_end_period as *mut ());
    }
}
