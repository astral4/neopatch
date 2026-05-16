//! Logging and `tracing_subscriber::Layer` implementation.
//!
//! Each session writes to `<log_root>/<session_id>/` containing
//! `manifest.txt`, `events.log`, and any minidumps.
//! `<log_root>` is `<install_dir>\neopatch_logs\` when writable,
//! falling back to `%LOCALAPPDATA%\neopatch\logs\`.
//! The `Layer` prefixes every event with information like timestamps and thread IDs,
//! and helps enforce consistent formatting.

use crate::config::{Config, LogCfg};
use std::cell::{Cell, RefCell};
use std::env::var;
use std::fmt::{Debug, Display, Write as _};
use std::fs::{File, OpenOptions, create_dir_all, read_dir, remove_dir_all};
use std::io::{BufWriter, Result as IoResult, Write};
use std::mem::zeroed;
use std::num::NonZero;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::subscriber::set_global_default;
use tracing::{Event, Level, Metadata, Subscriber, info};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

static FILE_WRITER: Mutex<Option<BufWriter<File>>> = Mutex::new(None);
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Idempotent; subsequent calls are no-ops. Returns `false` for `LevelFilter::OFF`.
pub(crate) fn init(
    install_dir: &Path,
    cfg: &LogCfg,
    host_exe: Option<&Path>,
    top_cfg: &Config,
) -> bool {
    let Some(level) = cfg.level.into_level() else {
        return false;
    };
    if SESSION_DIR.get().is_some() {
        return true;
    }

    _ = START.set(Instant::now());

    let log_root = pick_log_root(install_dir, cfg.log_dir.as_deref());
    let Some(log_root) = log_root else {
        return false;
    };

    let session_id = make_session_id();
    let session_dir = log_root.join(&session_id);
    if create_dir_all(&session_dir).is_err() {
        return false;
    }

    // Retention runs first so we don't sweep our own new directory.
    apply_retention(&log_root, cfg.sessions_to_keep, &session_id);

    drop(write_manifest(&session_dir, host_exe, top_cfg, &log_root));

    let events_path = session_dir.join("events.log");
    let Ok(file) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&events_path)
    else {
        return false;
    };
    if let Ok(mut guard) = FILE_WRITER.lock() {
        *guard = Some(BufWriter::with_capacity(8192, file));
    }

    drop(SESSION_DIR.set(session_dir.clone()));

    let layer = NeopatchLayer { level };
    let subscriber = tracing_subscriber::registry().with(layer);
    drop(set_global_default(subscriber));

    info!(
        kind = "log_init",
        neopatch_version = env!("CARGO_PKG_VERSION"),
        session_dir = %session_dir.display(),
    );
    true
}

/// Drains the `BufWriter` and calls `FlushFileBuffers`.
/// The fsync runs outside the mutex so other threads can keep emitting events
/// while the disk sync is in flight; the underlying `HANDLE`
/// lasts for the lifetime of the process by construction.
pub(crate) fn flush() {
    let raw = {
        let Ok(mut guard) = FILE_WRITER.lock() else {
            return;
        };
        let Some(writer) = guard.as_mut() else {
            return;
        };
        drop(writer.flush());
        writer.get_ref().as_raw_handle()
    };
    unsafe {
        FlushFileBuffers(raw);
    }
}

pub(crate) fn dump_dir() -> Option<&'static Path> {
    SESSION_DIR.get().map(PathBuf::as_path)
}

pub(crate) fn elapsed_secs() -> f64 {
    START.get().map_or(0.0, |s| s.elapsed().as_secs_f64())
}

/// Integer-math counterpart to `elapsed_secs`, in milliseconds.
pub(crate) fn elapsed_ms() -> u64 {
    START.get().map_or(0, |s| {
        u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
    })
}

fn pick_log_root(install_dir: &Path, override_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = override_dir
        && create_dir_all(dir).is_ok()
    {
        return Some(dir.to_path_buf());
    }
    let next_to_install = install_dir.join("neopatch_logs");
    if create_dir_all(&next_to_install).is_ok() {
        return Some(next_to_install);
    }
    // Fallback for read-only installs (e.g. Program Files).
    if let Ok(local) = var("LOCALAPPDATA") {
        let appdata = PathBuf::from(local).join("neopatch").join("logs");
        if create_dir_all(&appdata).is_ok() {
            return Some(appdata);
        }
    }
    None
}

fn make_session_id() -> String {
    let mut st: SYSTEMTIME = unsafe { zeroed() };
    unsafe {
        GetLocalTime(&raw mut st);
    }
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
    )
}

fn apply_retention(log_root: &Path, keep: u32, current: &str) {
    let Ok(entries) = read_dir(log_root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n != current && is_session_id(n))
        })
        .collect();
    // Session IDs sort lexicographically by timestamp.
    dirs.sort();
    // -1 to reserve a slot for the session we're about to write.
    let to_keep = (keep as usize).saturating_sub(1);
    if dirs.len() > to_keep {
        for old in &dirs[..dirs.len() - to_keep] {
            drop(remove_dir_all(old));
        }
    }
}

/// Returns true if `name` matches the `YYYYMMDD_HHMMSS` format of `make_session_id`;
/// false otherwise.
fn is_session_id(name: &str) -> bool {
    name.len() == 15
        && name.as_bytes()[8] == b'_'
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit())
}

fn write_manifest(
    session_dir: &Path,
    host_exe: Option<&Path>,
    cfg: &Config,
    log_root: &Path,
) -> IoResult<()> {
    let path = session_dir.join("manifest.txt");
    let mut f = File::create(path)?;
    writeln!(f, "neopatch_version={}", env!("CARGO_PKG_VERSION"))?;
    if let Some(p) = host_exe {
        writeln!(f, "host_exe={}", p.display())?;
    }
    writeln!(
        f,
        "build_target={}",
        if cfg!(target_pointer_width = "32") {
            "i686"
        } else {
            "non-i686"
        }
    )?;
    writeln!(f, "log_root={}", log_root.display())?;

    writeln!(f, "display.mode={}", cfg.display.mode)?;
    writeln!(f, "display.refresh_rate={}", cfg.display.refresh_rate)?;
    writeln!(f, "display.resolution={}", cfg.display.resolution)?;

    let w = &cfg.window;
    writeln!(
        f,
        "window={}x{} at ({},{}) frame={} always_on_top={}",
        fmt_opt(w.width.as_ref()),
        fmt_opt(w.height.as_ref()),
        w.x,
        w.y,
        fmt_opt(w.frame.as_ref()),
        w.always_on_top,
    )?;

    writeln!(f, "framerate.game_fps={}", cfg.framerate.game_fps)?;
    writeln!(
        f,
        "framerate.replay_skip_fps={}",
        cfg.framerate.replay_skip_fps,
    )?;
    writeln!(
        f,
        "framerate.replay_slow_fps={}",
        cfg.framerate.replay_slow_fps,
    )?;

    writeln!(f, "process.priority={}", cfg.process.priority)?;
    writeln!(
        f,
        "process.affinity_mask={}",
        fmt_mask(cfg.process.affinity_mask),
    )?;

    writeln!(f, "log.level={}", cfg.log.level)?;
    writeln!(f, "log.sessions_to_keep={}", cfg.log.sessions_to_keep)?;
    Ok(())
}

fn fmt_opt<T: Display>(v: Option<&T>) -> String {
    v.map_or_else(|| "auto".to_owned(), ToString::to_string)
}

fn fmt_mask(v: Option<NonZero<u32>>) -> String {
    v.map_or_else(|| "0 (default)".to_owned(), |m| format!("{:#x}", m.get()))
}

pub(crate) struct NeopatchLayer {
    level: Level,
}

impl<S> Layer<S> for NeopatchLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        // Lower ordering = higher priority; e.g. `Level::ERROR < Level::INFO`.
        metadata.level() <= &self.level
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Per-thread reusable line buffer, plus a per-thread re-entry guard.
        // The guard handles scenarios where this thread is already inside `on_event`
        // holding `FILE_WRITER`, faults, and the vectored handler calls `error!`
        // and then `on_event` again. `std::sync::Mutex` is non-reentrant,
        // so recursing into `FILE_WRITER.lock()` on the owning thread would deadlock.
        // We'd rather drop the diagnostic line.
        thread_local! {
            static LINE_BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
            static IN_EVENT: Cell<bool> = const { Cell::new(false) };
        }
        IN_EVENT.with(|in_event| {
            if in_event.get() {
                return;
            }
            in_event.set(true);
            LINE_BUF.with(|cell| {
                let mut line = cell.borrow_mut();
                line.clear();
                let ts = elapsed_secs();
                let tid = unsafe { GetCurrentThreadId() };
                let level = event.metadata().level();
                _ = write!(line, "[t={ts:.3}s tid={tid}] level={level}");
                let mut visitor = FieldVisitor { out: &mut line };
                event.record(&mut visitor);
                line.push('\n');
                if let Ok(mut guard) = FILE_WRITER.lock()
                    && let Some(writer) = guard.as_mut()
                {
                    // No per-line flush; watchdog ticks and crash/exit hooks
                    // are responsible for durability.
                    drop(writer.write_all(line.as_bytes()));
                }
            });
            in_event.set(false);
        });
    }
}

struct FieldVisitor<'a> {
    out: &'a mut String,
}

impl Visit for FieldVisitor<'_> {
    // The typed `record_i64`/`record_u64`/`record_bool`/`record_f64`/`record_str`
    // default impls in `tracing-core`'s `Visit` trait delegate to `record_debug`,
    // which is the only override we need.
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // `message` is the synthetic field for free-form `info!("...")` text.
        // We render this without a key, but everything else is in "key=value" form.
        if field.name() == "message" {
            _ = write!(self.out, " msg={value:?}");
        } else {
            _ = write!(self.out, " {}={:?}", field.name(), value);
        }
    }
}

/// Caps the number of times an info site can fire.
/// The first `limit` calls return `Some(n)` (0-indexed); subsequent calls return `None`.
/// This is used at call sites that would otherwise flood the log on a loop or per-frame path.
pub(crate) struct LogCap {
    count: AtomicU32,
    limit: u32,
}

impl LogCap {
    pub(crate) const fn new(limit: u32) -> Self {
        Self {
            count: AtomicU32::new(0),
            limit,
        }
    }

    pub(crate) fn tick(&self) -> Option<u32> {
        // Early-return via `load` introduces a race window, but the window
        // can leak at most one extra increment past the limit, which is harmless.
        if self.count.load(Ordering::Relaxed) >= self.limit {
            return None;
        }
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < self.limit { Some(n) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::is_session_id;

    #[test]
    fn is_session_id_accepts_real_session_format() {
        assert!(is_session_id("20260516_123045"));
        assert!(is_session_id("00000000_000000"));
        assert!(is_session_id("99999999_999999"));
    }

    #[test]
    fn is_session_id_rejects_unrelated_names() {
        assert!(!is_session_id(""));
        assert!(!is_session_id("important_data"));
        assert!(!is_session_id("20260516"));
        assert!(!is_session_id("20260516_12304"));
        assert!(!is_session_id("20260516_1230450"));
        assert!(!is_session_id("20260516-123045"));
        assert!(!is_session_id("2026051a_123045"));
        assert!(!is_session_id("20260516_12304a"));
    }
}
