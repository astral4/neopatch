//! Per-session logging.
//!
//! Each session writes `events.log`, `manifest.txt`, and any crash minidumps into a new `<log_root>/<session_id>/` directory.
//! Candidate roots are tried in order: `<install_dir>\neopatch_logs\`, then `%LOCALAPPDATA%\neopatch_logs\`,
//! then `%TEMP%\neopatch_logs\`. The first fails for read-only installs (e.g. `Program Files`), but the others should be writable.

use crate::config::{CoreConfig, write_manifest_common};
use std::cell::{Cell, RefCell};
use std::env::var_os;
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
use tracing::{Event, Level, Metadata, Subscriber, info};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, registry as default_registry};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

/// Emits one `tracing` event at `$yes` level when `$cond` holds and at `$no` level otherwise.
macro_rules! log_at {
    ($cond:expr => $yes:ident / $no:ident, $($fields:tt)+) => {
        if $cond {
            ::tracing::$yes!($($fields)+);
        } else {
            ::tracing::$no!($($fields)+);
        }
    };
}
pub(crate) use log_at;

// Each `on_event` write goes straight to the OS via `write_all`. We don't use `BufWriter`, so pending event lines
// won't be silently erased under `panic = "abort"`. The mutex serializes concurrent writers.
static FILE_WRITER: Mutex<Option<File>> = Mutex::new(None);
// `FILE_HANDLE` is used by `flush` for `FlushFileBuffers` without taking the mutex,
// so the crash path can fsync even when the panicking thread holds the writer mutex.
static FILE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Sets up the per-session log directory, opens `events.log`, writes `manifest.txt`,
/// and installs the global tracing layer. This is a no-op if logging is off or already initialized.
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

    let session_id = make_session_id();
    let (claimed, decisions) =
        claim_log_root(install_dir, core_cfg.log.log_dir.as_deref(), &session_id);
    let Some((log_root, session_dir)) = claimed else {
        return false;
    };

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
    // We publish `FILE_HANDLE` inside the same lock as `FILE_WRITER` so `flush` never sees one without the other.
    let raw_handle: *mut c_void = file.as_raw_handle().cast();
    if let Ok(mut guard) = FILE_WRITER.lock() {
        *guard = Some(file);
        FILE_HANDLE.store(raw_handle, Ordering::Release);
    }

    drop(SESSION_DIR.set(session_dir.clone()));

    let layer = NeopatchLayer { level };
    let subscriber = default_registry().with(layer);
    drop(set_global_default(subscriber));

    info!(
        kind = "log_init",
        neopatch_version = env!("CARGO_PKG_VERSION"),
        session_dir = %session_dir.display(),
    );
    for d in decisions {
        log_at!(d.outcome.is_success() => info / warn,
            kind = "log_root_decision",
            candidate = %d.path.display(),
            outcome = %d.outcome,
        );
    }
    true
}

/// Forces pending log writes to disk. This is safe to call from crash and exit hooks.
pub(crate) fn flush() {
    // `on_event` writes through `File::write_all`, which directly lands in the OS file cache.
    // `FlushFileBuffers` requests the OS to commit that cache to physical disk, which matters for power-off/hard-crash scenarios.
    let raw = FILE_HANDLE.load(Ordering::Acquire);
    if !raw.is_null() {
        unsafe { FlushFileBuffers(raw) };
    }
}

/// Returns the per-session directory where crash handlers should write minidumps. Returns `None` before `init` has run.
pub(crate) fn dump_dir() -> Option<&'static Path> {
    SESSION_DIR.get().map(PathBuf::as_path)
}

/// Returns the number of seconds since `init`. Returns `0.0` before `init`.
fn elapsed_secs() -> f64 {
    START.get().map_or(0., |s| s.elapsed().as_secs_f64())
}

/// Returns the number of milliseconds since `init`. Returns `0` before `init`.
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

/// Selects the log root by claiming `<root>/<session_id>/` at each candidate in turn.
/// Returns the chosen `(root, session_dir)` and the trace of candidates considered.
fn claim_log_root(
    install_dir: &Path,
    override_dir: Option<&Path>,
    session_id: &str,
) -> (Option<(PathBuf, PathBuf)>, Vec<LogRootDecision>) {
    let mut trace = Vec::new();

    let candidates = override_dir
        .map(Path::to_path_buf)
        .into_iter()
        .map(|p| (p, true))
        .chain(
            [
                install_dir.join("neopatch_logs"),
                appdata_subdir("LOCALAPPDATA"),
                appdata_subdir("TEMP"),
            ]
            .into_iter()
            .map(|p| (p, false)),
        );

    for (root, is_override) in candidates {
        // An empty path means the source env var is unset, so we skip it.
        if root.as_os_str().is_empty() {
            continue;
        }

        match claim_session_dir(&root, session_id) {
            Ok(session_dir) => {
                let outcome = if is_override {
                    LogRootOutcome::ChosenOverride
                } else {
                    LogRootOutcome::Chosen
                };
                trace.push(LogRootDecision {
                    path: root.clone(),
                    outcome,
                });
                return (Some((root, session_dir)), trace);
            }
            Err(outcome) => {
                let outcome = if is_override && matches!(outcome, LogRootOutcome::CreateFailed) {
                    LogRootOutcome::OverrideCreateFailed
                } else {
                    outcome
                };
                trace.push(LogRootDecision {
                    path: root,
                    outcome,
                });
            }
        }
    }
    (None, trace)
}

/// Returns `<%env_var%>\neopatch_logs\`, or an empty path if `env_var` is unset.
fn appdata_subdir(env_var: &str) -> PathBuf {
    var_os(env_var).map_or_else(PathBuf::new, |s| PathBuf::from(s).join("neopatch_logs"))
}

/// Creates `<root>/<session_id>/` and verifies it is actually located where the path says.
/// On rejection, the empty session directory is cleaned up, though a root created on the way is left behind.
fn claim_session_dir(root: &Path, session_id: &str) -> Result<PathBuf, LogRootOutcome> {
    let session_dir = root.join(session_id);
    if create_dir_all(&session_dir).is_err() {
        return Err(LogRootOutcome::CreateFailed);
    }
    let Ok(canonical) = canonicalize(&session_dir) else {
        // `remove_dir` only removes empty directories, so the cleanup is safe even if another process has already populated the leaf.
        drop(remove_dir(&session_dir));
        return Err(LogRootOutcome::CanonicalizeFailed);
    };
    if canonical
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("VirtualStore"))
    {
        drop(remove_dir(&session_dir));
        return Err(LogRootOutcome::VirtualStoreRedirected);
    }
    Ok(session_dir)
}

fn make_session_id() -> String {
    let mut st: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetLocalTime(&raw mut st) };
    // PID disambiguates concurrent same-second launches that would otherwise share a directory and clobber each other's logs.
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

/// Returns true if `name` matches the `YYYYMMDD_HHMMSS_pPID` format of `make_session_id`; false otherwise.
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
        // Lower ordering = higher priority (e.g. `Level::ERROR < Level::INFO`).
        metadata.level() <= &self.level
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        thread_local! {
            // Per-thread line buffer to avoid per-event allocation.
            static LINE_BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
            // Re-entry guard. Prevents deadlock from recursion into `FILE_WRITER.lock()` when this thread is inside `on_event`
            // holding `FILE_WRITER` while a crash handler fires `error!` on the same thread.
            // When this happens, the guard drops the inner line instead.
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
                    // We don't flush for each line since the crash/exit hooks are responsible for durability.
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
    use super::{Level, NeopatchLayer, is_session_id};
    use tracing::subscriber::with_default as set_default_subscriber;
    use tracing::warn;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

    #[test]
    fn hooks_restore_last_error_after_emitting_event() {
        let subscriber = registry().with(NeopatchLayer {
            level: Level::TRACE,
        });
        set_default_subscriber(subscriber, || {
            unsafe { SetLastError(0xC0DE) };
            warn!(kind = "last_error_probe", detail = "allocates and formats");
            assert_ne!(unsafe { GetLastError() }, 0xC0DE);
        });
    }

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
