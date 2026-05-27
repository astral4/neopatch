//! Per-session logging.
//!
//! Each session writes `events.log`, `manifest.txt`, and any crash minidumps into a new
//! `<log_root>/<session_id>/` directory. Candidate roots are tried in order:
//! `<install_dir>\neopatch_logs\`, then `%LOCALAPPDATA%\neopatch_logs\`, then
//! `%TEMP%\neopatch_logs\`. The first fails on read-only installs (e.g. `Program Files`
//! for a manifested process). The second one fails on UAC-redirected writes into
//! `%LOCALAPPDATA%\VirtualStore\...`. The third should always be writable.

use crate::config::{CoreConfig, write_manifest_common};
use std::cell::{Cell, RefCell};
use std::env::var;
use std::ffi::c_void;
use std::fmt::{Debug, Display, Formatter, Result as FmtResult, Write as _};
use std::fs::{
    File, OpenOptions, canonicalize, create_dir_all, read_dir, remove_dir, remove_dir_all,
};
use std::io::{Result as IoResult, Write};
use std::mem::zeroed;
use std::num::NonZero;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::subscriber::set_global_default;
use tracing::{Event, Level, Metadata, Subscriber, info, warn};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

// Each `on_event` write goes straight to the OS via `write_all`. We don't use `BufWriter`
// so pending event lines won't be silently erased under `panic = "abort"`.
// The mutex serializes concurrent writers.
static FILE_WRITER: Mutex<Option<File>> = Mutex::new(None);
// `FILE_HANDLE` is used by `flush` for `FlushFileBuffers` without taking the mutex,
// so the crash path can fsync even when the panicking thread holds the writer mutex.
static FILE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Sets up the per-session log directory, opens `events.log`, writes `manifest.txt`,
/// and installs the global tracing layer. No-op if logging is off or already initialized.
///
/// The shared neopatch-version / build-target / host-exe / log-root / log preamble and the
/// `[display]` / `[window]` / `[framerate]` / `[process]` keys from `core_cfg` are written
/// automatically; `extra_manifest` writes any genuinely game-specific lines after them.
pub fn init<F>(
    install_dir: &Path,
    core_cfg: &CoreConfig,
    host_exe: Option<&Path>,
    extra_manifest: F,
) -> bool
where
    F: FnOnce(&mut dyn Write) -> IoResult<()>,
{
    let Some(level) = core_cfg.log.level.into_level() else {
        return false;
    };
    if SESSION_DIR.get().is_some() {
        return true;
    }

    _ = START.set(Instant::now());

    let (log_root, decisions) = pick_log_root(install_dir, core_cfg.log.log_dir.as_deref());
    let Some(log_root) = log_root else {
        return false;
    };

    let session_id = make_session_id();
    let session_dir = log_root.join(&session_id);
    if create_dir_all(&session_dir).is_err() {
        return false;
    }

    // Retention runs first so we don't sweep our own new directory.
    apply_retention(&log_root, core_cfg.log.sessions_to_keep, &session_id);

    drop(write_manifest(
        &session_dir,
        host_exe,
        core_cfg,
        &log_root,
        extra_manifest,
    ));

    let events_path = session_dir.join("events.log");
    let Ok(file) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&events_path)
    else {
        return false;
    };
    // We publish `FILE_HANDLE` inside the same lock as `FILE_WRITER`
    // so `flush` never sees one without the other.
    let raw_handle: *mut c_void = file.as_raw_handle().cast();
    if let Ok(mut guard) = FILE_WRITER.lock() {
        *guard = Some(file);
        FILE_HANDLE.store(raw_handle, Ordering::Release);
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
    for d in decisions {
        if d.outcome.is_success() {
            info!(
                kind = "log_root_decision",
                candidate = %d.path.display(),
                outcome = %d.outcome,
            );
        } else {
            warn!(
                kind = "log_root_decision",
                candidate = %d.path.display(),
                outcome = %d.outcome,
            );
        }
    }
    true
}

/// Forces pending log writes to disk. Safe to call from crash and exit hooks.
pub(crate) fn flush() {
    // `on_event` writes through `File::write_all`, which directly lands in the OS file cache.
    // `FlushFileBuffers` requests the OS to commit that cache to physical disk,
    // which matters for power-off/hard-crash scenarios.
    let raw = FILE_HANDLE.load(Ordering::Acquire);
    if !raw.is_null() {
        unsafe {
            FlushFileBuffers(raw);
        }
    }
}

/// Returns the per-session directory where crash handlers should write minidumps.
/// Returns `None` before `init` has run.
pub(crate) fn dump_dir() -> Option<&'static Path> {
    SESSION_DIR.get().map(PathBuf::as_path)
}

/// Returns the number of seconds since `init`.
/// Returns `0.0` before `init`.
fn elapsed_secs() -> f64 {
    START.get().map_or(0.0, |s| s.elapsed().as_secs_f64())
}

/// Returns the number of milliseconds since `init`.
/// Returns `0` before `init`.
pub(crate) fn elapsed_ms() -> u64 {
    START.get().map_or(0, |s| {
        u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
    })
}

#[derive(Clone, Copy, Debug)]
enum LogRootOutcome {
    Chosen,
    ChosenOverride,
    OverrideCreateFailed,
    CreateFailed,
    CanonicalizeFailed,
    VirtualStoreRedirected,
}

impl LogRootOutcome {
    fn is_success(self) -> bool {
        matches!(self, Self::Chosen | Self::ChosenOverride)
    }
}

impl Display for LogRootOutcome {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            Self::Chosen => "chosen",
            Self::ChosenOverride => "chosen_override",
            Self::OverrideCreateFailed => "override_create_failed",
            Self::CreateFailed => "create_failed",
            Self::CanonicalizeFailed => "canonicalize_failed",
            Self::VirtualStoreRedirected => "virtualstore_redirected",
        })
    }
}

struct LogRootDecision {
    path: PathBuf,
    outcome: LogRootOutcome,
}

/// Picks a writable log root and returns the trace of candidates considered.
/// The caller emits one `log_root_decision` event per entry post-subscriber-initialization
/// so a user can see why their logs landed where they did.
fn pick_log_root(
    install_dir: &Path,
    override_dir: Option<&Path>,
) -> (Option<PathBuf>, Vec<LogRootDecision>) {
    let mut trace = Vec::new();
    if let Some(dir) = override_dir {
        if create_dir_all(dir).is_ok() {
            trace.push(LogRootDecision {
                path: dir.to_path_buf(),
                outcome: LogRootOutcome::ChosenOverride,
            });
            return (Some(dir.to_path_buf()), trace);
        }
        trace.push(LogRootDecision {
            path: dir.to_path_buf(),
            outcome: LogRootOutcome::OverrideCreateFailed,
        });
    }
    for candidate in [
        install_dir.join("neopatch_logs"),
        appdata_subdir("LOCALAPPDATA"),
        appdata_subdir("TEMP"),
    ] {
        // Empty path means the source env var is unset; skip silently.
        if candidate.as_os_str().is_empty() {
            continue;
        }
        let outcome = try_use_dir(&candidate);
        trace.push(LogRootDecision {
            path: candidate.clone(),
            outcome,
        });
        if matches!(outcome, LogRootOutcome::Chosen) {
            return (Some(candidate), trace);
        }
    }
    (None, trace)
}

/// Returns `<%env_var%>\neopatch_logs\`, or an empty path if `env_var` is unset.
fn appdata_subdir(env_var: &str) -> PathBuf {
    var(env_var).map_or_else(
        |_| PathBuf::new(),
        |s| PathBuf::from(s).join("neopatch_logs"),
    )
}

/// Creates `dir` and verifies it is actually located where the path says.
fn try_use_dir(dir: &Path) -> LogRootOutcome {
    if create_dir_all(dir).is_err() {
        return LogRootOutcome::CreateFailed;
    }
    let Ok(canonical) = canonicalize(dir) else {
        // `remove_dir` only removes empty directories, so the cleanup is
        // safe even if another process has already populated the leaf.
        drop(remove_dir(dir));
        return LogRootOutcome::CanonicalizeFailed;
    };
    if canonical
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("VirtualStore"))
    {
        drop(remove_dir(dir));
        return LogRootOutcome::VirtualStoreRedirected;
    }
    LogRootOutcome::Chosen
}

fn make_session_id() -> String {
    let mut st: SYSTEMTIME = unsafe { zeroed() };
    unsafe {
        GetLocalTime(&raw mut st);
    }
    // PID disambiguates concurrent same-second launches that would otherwise
    // share a directory and clobber each other's logs.
    let pid = unsafe { GetCurrentProcessId() };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}_p{pid}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
    )
}

fn apply_retention(log_root: &Path, keep: NonZero<u32>, current: &str) {
    let Ok(entries) = read_dir(log_root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n != current && is_session_id(n))
                && p.is_dir()
        })
        .collect();
    // Session IDs sort lexicographically by timestamp; ties are broken by PID.
    dirs.sort();
    // -1 to reserve a slot for the session we're about to write.
    let to_keep = (keep.get() - 1) as usize;
    if dirs.len() > to_keep {
        for old in &dirs[..dirs.len() - to_keep] {
            drop(remove_dir_all(old));
        }
    }
}

/// Returns true if `name` matches the `YYYYMMDD_HHMMSS_pPID` format of make_session_id`;
/// false otherwise.
fn is_session_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    // 15 ("YYYYMMDD_HHMMSS") + 2 ("_p") + at least one PID digit.
    if bytes.len() < 18 {
        return false;
    }
    if bytes[8] != b'_' || bytes[15] != b'_' || bytes[16] != b'p' {
        return false;
    }
    bytes
        .iter()
        .enumerate()
        .all(|(i, b)| matches!(i, 8 | 15 | 16) || b.is_ascii_digit())
}

fn write_manifest<F>(
    session_dir: &Path,
    host_exe: Option<&Path>,
    core_cfg: &CoreConfig,
    log_root: &Path,
    extra: F,
) -> IoResult<()>
where
    F: FnOnce(&mut dyn Write) -> IoResult<()>,
{
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
    writeln!(f, "log.level={}", core_cfg.log.level)?;
    writeln!(f, "log.sessions_to_keep={}", core_cfg.log.sessions_to_keep)?;
    write_manifest_common(&mut f, core_cfg)?;
    extra(&mut f)?;
    Ok(())
}

struct NeopatchLayer {
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
        // Per-thread line buffer to avoid per-event allocation, plus a re-entry guard.
        // The guard prevents deadlock from recursion into `FILE_WRITER.lock()`
        // when this thread is inside `on_event` holding `FILE_WRITER`
        // while a crash handler fires `error!` on the same thread.
        // When this happens, the guard drops the inner line instead.
        thread_local! {
            static LINE_BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
            static IN_EVENT: Cell<bool> = const { Cell::new(false) };
        }
        IN_EVENT.with(|in_event| {
            if in_event.get() {
                return;
            }
            in_event.set(true);
            LINE_BUF.with_borrow_mut(|line| {
                line.clear();
                let ts = elapsed_secs();
                let tid = unsafe { GetCurrentThreadId() };
                let level = event.metadata().level();
                _ = write!(line, "[t={ts:.3}s tid={tid}] level={level}");
                let mut visitor = FieldVisitor { out: line };
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

#[cfg(test)]
mod tests {
    use super::is_session_id;

    #[test]
    fn is_session_id_accepts_real_session_format() {
        assert!(is_session_id("20260516_123045_p1"));
        assert!(is_session_id("20260516_123045_p12345"));
        assert!(is_session_id("00000000_000000_p0"));
        assert!(is_session_id("99999999_999999_p4294967295"));
    }

    #[test]
    fn is_session_id_rejects_unrelated_names() {
        assert!(!is_session_id(""));
        assert!(!is_session_id("important_data"));
        assert!(!is_session_id("20260516"));
        assert!(!is_session_id("20260516_12304"));
        assert!(!is_session_id("20260516_123045"));
        assert!(!is_session_id("20260516_1230450"));
        assert!(!is_session_id("20260516-123045"));
        assert!(!is_session_id("2026051a_123045"));
        assert!(!is_session_id("20260516_12304a"));
        assert!(!is_session_id("20260516_123045_p"));
        assert!(!is_session_id("20260516_123045-p1"));
        assert!(!is_session_id("20260516_123045_x1"));
        assert!(!is_session_id("20260516_123045_p1a"));
    }
}
