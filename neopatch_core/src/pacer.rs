//! Frame pacer.
//!
//! We drive frame timing in software so the game's render cadence is independent of the display's
//! refresh rate. The deadline is tracked in QPC ticks and advanced by exactly one period per frame,
//! so transient hitches don't smear into permanent drift. Within a constant-rate window,
//! the long-run frame rate is exactly `target_fps` regardless of OS scheduler jitter.
//! Across mode transitions, `apply_policy()` resets the deadline so the next `wait()` resyncs.
//! This is intentional but means "exact rate" is technically per-window, not per-session.
//!
//! Lead-time is fixed at `period / 2`. The wait fires that much earlier than the stored deadline,
//! so the next game tick (which reads input) starts that much earlier in wall-clock time.
//! The deadline itself is unaffected, so the long-run cadence still locks to `target_fps`.
//! Whether the shift applies is encoded in `PacingPolicy`: `LiveInput` enables it,
//! since that's when input timing matters; `InternalCadence` disables it, since replay-skip/slow
//! drive the schedule from inside the game and shaving real-time latency is meaningless there.
//!
//! `period / 2` isn't a hard ceiling: any `L < period` works since the only correctness constraint
//! is that per-frame work must fit in one period. It's the largest `L` for which the lead snaps
//! into place within one frame of a resync, since immediate convergence needs `work < period - L`.
//! We assume computers running neopatch will comfortably finish per-frame work in under `period / 2`.
//! The choice of `L` also rests on the prior that an earlier `Present` submission plausibly leaves
//! more slack against jitter or misalignment in the `Present`-to-display chain. We can't actually
//! verify whether that slack helps from within the pacer itself. We pick `period / 2` because
//! it is the largest `L` that captures this prior at no transient cost. Larger `L` also captures
//! this prior but causes longer post-resync transients.
//!
//! For waiting, we use `CreateWaitableTimerExW` with the Windows 10 1803+ `HIGH_RESOLUTION` flag,
//! closed by a short QPC spin to the exact deadline. We fall back to plain `Sleep(ms-1) + spin`
//! (with the timer resolution pinned to 1 ms by `timer_period`) on older OS versions.

use crate::thread::{MainCell, MainToken};
use std::hint::spin_loop;
use std::ptr::null;
use std::sync::OnceLock;
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

pub static PACER: OnceLock<Pacer> = OnceLock::new();

/// Distinguishes when the game reads real-time input (lead applies)
/// from when the game drives its own schedule (lead doesn't apply).
#[derive(Clone, Copy, Debug)]
pub enum PacingPolicy {
    LiveInput { target_fps: u32 },
    InternalCadence { target_fps: u32 },
}

impl PacingPolicy {
    #[must_use]
    pub(crate) fn target_fps(self) -> u32 {
        match self {
            Self::LiveInput { target_fps } | Self::InternalCadence { target_fps } => target_fps,
        }
    }
    fn lead_active(self) -> bool {
        matches!(self, Self::LiveInput { .. })
    }
}

pub struct Pacer {
    qpc_freq: i64,
    resync_threshold_qpc: i64,
    /// Created eagerly in `new`; null means `CreateWaitableTimerExW` failed both attempts
    /// and `sleep_until` falls through to Sleep+spin.
    timer: MainCell<HANDLE>,
    /// Cached `qpc_freq / target_fps`. `0` disables pacing.
    period_qpc: MainCell<i64>,
    lead_active: MainCell<bool>,
    /// `0` means "resync on next call."
    deadline_qpc: MainCell<i64>,
}

impl Pacer {
    #[must_use]
    pub fn new(policy: PacingPolicy) -> Self {
        let qpc_freq = read_qpc_freq();
        Self {
            qpc_freq,
            resync_threshold_qpc: qpc_freq * RESYNC_THRESHOLD_MS / 1000,
            timer: MainCell::new(create_waitable_timer()),
            period_qpc: MainCell::new(period_qpc_from(policy.target_fps(), qpc_freq)),
            lead_active: MainCell::new(policy.lead_active()),
            deadline_qpc: MainCell::new(0),
        }
    }

    /// A `target_fps` of 0 in the policy disables pacing.
    ///
    /// Resets the deadline so the next `wait()` resyncs. Otherwise, a stale deadline
    /// would chase the wrong period for several frames after a rate change.
    pub(crate) fn apply_policy(&self, tok: &MainToken, policy: PacingPolicy) {
        self.period_qpc
            .set(tok, period_qpc_from(policy.target_fps(), self.qpc_freq));
        self.lead_active.set(tok, policy.lead_active());
        self.deadline_qpc.set(tok, 0);
    }

    /// Blocks until the next frame's deadline, then advances the deadline.
    /// Call once per `Present`.
    pub(crate) fn wait(&self, tok: &MainToken) {
        let period = self.period_qpc.get(tok);
        if period == 0 {
            return;
        }
        let now = qpc();
        let mut deadline = self.deadline_qpc.get(tok);
        if deadline == 0 || now > deadline + self.resync_threshold_qpc {
            // First call or beyond the resync threshold. Below the threshold,
            // the fall-through path absorbs the gap via catchup-sprint.
            deadline = now + period;
            self.deadline_qpc.set(tok, deadline);
            return;
        }
        // Shift only the wait target, not the stored deadline.
        // Long-run cadence still locks to `target_fps`.
        let target = if self.lead_active.get(tok) {
            deadline - period / 2
        } else {
            deadline
        };
        if now < target {
            self.sleep_until(tok, target, now);
        }
        self.deadline_qpc.set(tok, deadline + period);
    }

    fn sleep_until(&self, tok: &MainToken, deadline: i64, now: i64) {
        let remaining_qpc = deadline - now;
        let h = self.timer.get(tok);
        if h.is_null() {
            let ms = qpc_to_ms(remaining_qpc, self.qpc_freq);
            if ms > 1 {
                unsafe { Sleep(ms - 1) };
            }
            spin_until(deadline);
            return;
        }
        // By `SetWaitableTimer` convention, a negative `due_time`
        // indicates a relative interval in 100ns units.
        // We shave a safety margin and spin to the exact deadline.
        let hundred_ns = qpc_to_100ns(remaining_qpc, self.qpc_freq);
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
}

/// Tries `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` first; falls back to a plain waitable timer;
/// returns null if both fail (caller falls back to Sleep+spin). Logs which path was taken.
fn period_qpc_from(target_fps: u32, qpc_freq: i64) -> i64 {
    if target_fps == 0 {
        0
    } else {
        qpc_freq / i64::from(target_fps)
    }
}

fn create_waitable_timer() -> HANDLE {
    let h = unsafe {
        CreateWaitableTimerExW(
            null(),
            null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if !h.is_null() {
        info!(kind = "waitable_timer", path = "high-resolution");
        return h;
    }
    let h = unsafe { CreateWaitableTimerExW(null(), null(), 0, TIMER_ALL_ACCESS) };
    let path = if h.is_null() {
        "Sleep+spin fallback"
    } else {
        "non-high-resolution"
    };
    info!(kind = "waitable_timer", path);
    h
}

fn qpc() -> i64 {
    let mut t: i64 = 0;
    unsafe {
        QueryPerformanceCounter(&raw mut t);
    }
    t
}

// `QueryPerformanceFrequency` is documented as fixed at boot, so a single read
// at `Pacer::new` is enough; the value is then stored as a field.
fn read_qpc_freq() -> i64 {
    let mut f: i64 = 0;
    unsafe {
        QueryPerformanceFrequency(&raw mut f);
    }
    f
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
    match ticks.checked_mul(10_000_000) {
        Some(scaled) => scaled / freq,
        None => i64::MAX,
    }
}

fn spin_until(deadline: i64) {
    // PAUSE-loop rather than `SwitchToThread`: this only runs after the timer
    // has already brought us within `SAFETY_MARGIN_100NS` of the deadline,
    // so we want the core to ourselves for the ~0.2 ms closing stretch.
    // A `SwitchToThread` here would either no-op (no ready neighbor) at the
    // cost of a syscall per iteration, or yield and risk missing the deadline.
    while qpc() < deadline {
        spin_loop();
    }
}
