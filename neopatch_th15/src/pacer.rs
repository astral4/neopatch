//! Frame pacer.
//!
//! We drive frame timing in software so the game's render cadence is independent of
//! the display's refresh rate. The deadline is tracked in QPC ticks and advanced by
//! exactly one period per frame, so transient hitches don't smear into permanent drift.
//! *Within a constant-rate window*, the long-run frame rate is exactly `target_fps`
//! regardless of OS scheduler jitter. Across mode transitions, `set_fps` resets the deadline
//! so the next `wait()` resyncs. This is intentional but means "exact rate"
//! is technically per-window, not per-session.
//!
//! Lead-time is fixed at `period / 2`. The wait fires that much earlier than the stored deadline,
//! so the next game tick (which reads input) starts that much earlier in wall-clock time.
//! The deadline itself is unaffected, so the long-run cadence still locks to `target_fps`.
//! The shift is gated by `lead_active`, which `d3d9::hook_present` clears in replay-skip/slow
//! (those modes don't read real-time input). The `period / 2` choice is the "geometric" maximum
//! allowed by the pacer's wake-tick-present-wait loop. Any computer made in the last 20 years
//! can probably clear the `period / 2` threshold, so lower values are unnecessarily conservative.
//!
//! For waiting, we use `CreateWaitableTimerExW` with the Windows 10 1803+ `HIGH_RESOLUTION` flag,
//! closed by a short QPC spin to the exact deadline. We fall back to plain `Sleep(ms-1) + spin`
//! (with the timer resolution pinned to 1 ms by `timer_period`) on older OS versions.

use crate::thread::debug_assert_main;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::thread::yield_now;
use tracing::info;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    Sleep, TIMER_ALL_ACCESS, WaitForSingleObject,
};

/// HR-timer wake-up safety margin in 100 ns units (i.e. this is 0.2 ms).
/// The timer wakes us this far before our deadline; the final stretch is closed by a QPC spin
/// so we land exactly on the deadline regardless of timer jitter.
const SAFETY_MARGIN_100NS: i64 = 2_000;

/// Wall-clock threshold beyond which `wait()` resyncs instead of catchup-sprinting.
/// Below this gap, missed deadlines are absorbed by skipping the sleep on the next few frames.
/// Above it, the deadline is reseeded. We go by wall-clock time rather than a period multiple
/// so the meaning of "absorb hitches up to 50 ms" stays consistent across `game_fps` values.
/// There isn't anything special about 50 ms in particular.
const RESYNC_THRESHOLD_MS: i64 = 50;

/// `RESYNC_THRESHOLD_MS` in QPC ticks. Computed once at first `wait()` rather than every frame.
/// Both inputs stay constant after first read, so their product stays constant too.
static RESYNC_QPC_THRESHOLD: LazyLock<i64> =
    LazyLock::new(|| qpc_freq() * RESYNC_THRESHOLD_MS / 1000);

pub(crate) static PACER: OnceLock<Pacer> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct Pacer {
    // This is atomic so `set_fps` can retune without locking.
    period_qpc: AtomicI64,
    /// 0 means "resync on next call."
    deadline_qpc: AtomicI64,
    /// When true, `wait()` releases `period / 2` earlier than the deadline, which is the maximum
    /// allowed by the pacer design. The stored deadline is unaffected, so the long-run rate stays exact.
    /// This should be set to false in replay-skip/slow modes where game logic drives cadence
    /// and shaving input latency is meaningless.
    lead_active: AtomicBool,
    timer: AtomicPtr<c_void>,
}

impl Pacer {
    /// A `target_fps` of 0 disables pacing.
    ///
    /// Resets the deadline so the next `wait()` resyncs. Otherwise, a stale deadline
    /// would chase the wrong period for several frames after a rate change.
    ///
    /// The two stores aren't atomic together, but this is fine because all callers
    /// run on the `Present` thread and are serialized with `wait()`.
    pub(crate) fn set_fps(&self, target_fps: u32) {
        debug_assert_main();
        self.period_qpc
            .store(period_qpc_for(target_fps), Ordering::Relaxed);
        self.deadline_qpc.store(0, Ordering::Relaxed);
    }

    pub(crate) fn set_lead_active(&self, active: bool) {
        debug_assert_main();
        self.lead_active.store(active, Ordering::Relaxed);
    }

    pub(crate) fn new(target_fps: u32) -> Self {
        Self {
            period_qpc: AtomicI64::new(period_qpc_for(target_fps)),
            deadline_qpc: AtomicI64::new(0),
            lead_active: AtomicBool::new(true),
            timer: AtomicPtr::new(null_mut()),
        }
    }

    /// Blocks until the next frame's deadline, then advances the deadline.
    /// Call once per `Present`.
    pub(crate) fn wait(&self) {
        debug_assert_main();
        let period = self.period_qpc.load(Ordering::Relaxed);
        if period <= 0 {
            return;
        }
        let now = qpc();
        let mut deadline = self.deadline_qpc.load(Ordering::Relaxed);
        if deadline == 0 || now > deadline + *RESYNC_QPC_THRESHOLD {
            // This code is reached on first call or when beyond the resync threshold.
            // Below the threshold, the fall-through path below absorbs the gap via catchup-spring.
            deadline = now + period;
            self.deadline_qpc.store(deadline, Ordering::Relaxed);
            return;
        }
        // Shift only the wait target, not the stored deadline.
        // Long-run cadence still locks to `target_fps`.
        let target = if self.lead_active.load(Ordering::Relaxed) {
            deadline - period / 2
        } else {
            deadline
        };
        if now < target {
            self.sleep_until(target, now);
        }
        self.deadline_qpc
            .store(deadline + period, Ordering::Relaxed);
    }

    fn sleep_until(&self, deadline: i64, now: i64) {
        let freq = qpc_freq();
        let remaining_qpc = deadline - now;
        let h = self.timer_handle();
        if h.is_null() {
            let ms = qpc_to_ms(remaining_qpc, freq);
            if ms > 1 {
                unsafe { Sleep(ms - 1) };
            }
            spin_until(deadline);
            return;
        }
        // By `SetWaitableTimer` convention, a negative `due_time`
        // indicates a relative interval in 100ns units.
        // We shave a safety margin and spin to the exact deadline.
        let hundred_ns = qpc_to_100ns(remaining_qpc, freq);
        if hundred_ns > SAFETY_MARGIN_100NS {
            let due: i64 = -(hundred_ns - SAFETY_MARGIN_100NS);
            unsafe {
                if SetWaitableTimer(h, &raw const due, 0, None, null(), 0) != 0 {
                    WaitForSingleObject(h, INFINITE);
                }
            }
        }
        spin_until(deadline);
    }

    fn timer_handle(&self) -> HANDLE {
        let cached = self.timer.load(Ordering::Relaxed);
        if !cached.is_null() {
            return cached;
        }
        let mut h = unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        let mut path = "high-resolution";
        if h.is_null() {
            h = unsafe { CreateWaitableTimerExW(null(), null(), 0, TIMER_ALL_ACCESS) };
            path = if h.is_null() {
                "Sleep+spin fallback"
            } else {
                "non-high-resolution"
            };
        }
        info!(kind = "waitable_timer", path);
        // We are single-writer (the `Present` thread is the sole caller of `wait`),
        // so no CAS is needed. A null `h` is stored back so the retry
        // on the next wait still has a chance to succeed.
        self.timer.store(h, Ordering::Relaxed);
        h
    }
}

// 0 is the sentinel value for disengaging the pacer.
// We handle it specially and avoid division by 0.
fn period_qpc_for(target_fps: u32) -> i64 {
    if target_fps == 0 {
        0
    } else {
        qpc_freq() / i64::from(target_fps)
    }
}

fn qpc() -> i64 {
    let mut t: i64 = 0;
    unsafe {
        QueryPerformanceCounter(&raw mut t);
    }
    t
}

// Cached after first call because `QueryPerformanceFrequency` is documented as
// fixed at boot, so a single query is enough.
fn qpc_freq() -> i64 {
    static FREQ: LazyLock<i64> = LazyLock::new(|| {
        let mut f: i64 = 0;
        unsafe {
            QueryPerformanceFrequency(&raw mut f);
        }
        f
    });
    *FREQ
}

fn qpc_to_ms(ticks: i64, freq: i64) -> u32 {
    if ticks <= 0 {
        return 0;
    }
    u32::try_from((ticks * 1_000) / freq).unwrap_or(u32::MAX)
}

fn qpc_to_100ns(ticks: i64, freq: i64) -> i64 {
    if ticks <= 0 {
        return 0;
    }
    i64::try_from(i128::from(ticks) * 10_000_000 / i128::from(freq)).unwrap_or(i64::MAX)
}

fn spin_until(deadline: i64) {
    // `yield_now()` yields to ready-to-run neighbors on this core (`SwitchToThread`).
    while qpc() < deadline {
        yield_now();
    }
}
