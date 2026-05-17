//! Crash-time capture. Includes a vectored exception handler, unhandled filter, and minidump.
//!
//! Vectored runs before the SEH chain and can't be overwritten by
//! a later `SetUnhandledExceptionFilter`. The unhandled filter is the fallback path.

use crate::log::{LogCap, dump_dir, elapsed_ms, flush};
use crate::match_named;
use crate::untrusted::safe_read_stack;
use std::fmt::Write as _;
use std::iter::once;
use std::num::NonZero;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{error, info};
use windows_sys::Win32::Foundation::{
    CloseHandle, DBG_PRINTEXCEPTION_C, DBG_PRINTEXCEPTION_WIDE_C, EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ARRAY_BOUNDS_EXCEEDED, EXCEPTION_BREAKPOINT, EXCEPTION_FLT_DENORMAL_OPERAND,
    EXCEPTION_FLT_DIVIDE_BY_ZERO, EXCEPTION_FLT_INVALID_OPERATION, EXCEPTION_FLT_OVERFLOW,
    EXCEPTION_FLT_UNDERFLOW, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_IN_PAGE_ERROR,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_INVALID_DISPOSITION, EXCEPTION_INVALID_HANDLE,
    EXCEPTION_NONCONTINUABLE_EXCEPTION, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_SINGLE_STEP,
    EXCEPTION_STACK_OVERFLOW, GENERIC_WRITE, INVALID_HANDLE_VALUE, NTSTATUS,
    STATUS_FATAL_USER_CALLBACK_EXCEPTION, STATUS_HEAP_CORRUPTION,
    STATUS_INVALID_CRUNTIME_PARAMETER, STATUS_STACK_BUFFER_OVERRUN,
};
use windows_sys::Win32::Storage::FileSystem::{CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    MINIDUMP_EXCEPTION_INFORMATION, MINIDUMP_TYPE, MiniDumpNormal, MiniDumpWithDataSegs,
    MiniDumpWithFullMemoryInfo, MiniDumpWithHandleData, MiniDumpWithProcessThreadData,
    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
    SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::SystemServices::{
    EXCEPTION_EXECUTE_FAULT, EXCEPTION_READ_FAULT, EXCEPTION_WRITE_FAULT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

/// Cap dumps per session: each is 5–20 MB and a re-entrant crash
/// could otherwise fill the disk.
const DUMP_LIMIT: u32 = 8;

// VC++ runtime conventions
const MS_VC_THREAD_NAME: NTSTATUS = 0x406D_1388_u32.cast_signed();
const MS_VC_CXX_EH: NTSTATUS = 0xE06D_7363_u32.cast_signed();

/// Re-entry guard. If our filter itself crashes (e.g. dereferencing an invalid context),
/// the OS would re-invoke us and we'd recurse until stack overflow.
/// This flag is tripped on first entry and never reset.
static IN_FILTER: AtomicBool = AtomicBool::new(false);

/// Cap log volume from noisy first-chance benign codes
/// while still preserving the first few entries in case they lead up to a crash.
static BENIGN_LOG: LogCap = LogCap::new(NonZero::new(16).unwrap());

static DUMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// Returns the dump path on success; `None` on any failure.
///
/// The dump type excludes `MiniDumpWithFullMemory` because full-memory dumps are very large.
/// The included sections should be enough for windbg to identify the faulting frame.
unsafe fn write_minidump(info: *const EXCEPTION_POINTERS, label: &str) -> Option<PathBuf> {
    let n = DUMP_SEQ.fetch_add(1, Ordering::Relaxed);
    if n >= DUMP_LIMIT {
        return None;
    }
    let dir = dump_dir()?;

    let tid = unsafe { GetCurrentThreadId() };
    let elapsed_ms = elapsed_ms();
    let filename = format!("neopatch_dump_{n}_{label}_tid{tid}_t{elapsed_ms}.dmp");
    let path = dir.join(&filename);

    // MiniDumpWriteDump takes a raw `HANDLE`,
    // so we use `CreateFileW` instead of `std::fs::File`.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let file = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return None;
    }

    let mdei = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: tid,
        ExceptionPointers: info.cast_mut(),
        ClientPointers: 0,
    };
    let dump_type: MINIDUMP_TYPE = MiniDumpNormal
        | MiniDumpWithDataSegs
        | MiniDumpWithHandleData
        | MiniDumpWithUnloadedModules
        | MiniDumpWithProcessThreadData
        | MiniDumpWithFullMemoryInfo
        | MiniDumpWithThreadInfo;

    let process = unsafe { GetCurrentProcess() };
    let pid = unsafe { GetCurrentProcessId() };
    let ok = unsafe {
        MiniDumpWriteDump(
            process,
            pid,
            file,
            dump_type,
            &raw const mdei,
            null(),
            null(),
        )
    };
    unsafe {
        CloseHandle(file);
    }

    if ok != 0 { Some(path) } else { None }
}

fn exception_name(code: NTSTATUS) -> &'static str {
    match_named!(
        code,
        EXCEPTION_ACCESS_VIOLATION,
        EXCEPTION_IN_PAGE_ERROR,
        EXCEPTION_INVALID_HANDLE,
        EXCEPTION_ILLEGAL_INSTRUCTION,
        EXCEPTION_NONCONTINUABLE_EXCEPTION,
        EXCEPTION_INVALID_DISPOSITION,
        EXCEPTION_ARRAY_BOUNDS_EXCEEDED,
        EXCEPTION_FLT_DENORMAL_OPERAND,
        EXCEPTION_FLT_DIVIDE_BY_ZERO,
        EXCEPTION_FLT_INVALID_OPERATION,
        EXCEPTION_FLT_OVERFLOW,
        EXCEPTION_FLT_UNDERFLOW,
        EXCEPTION_INT_DIVIDE_BY_ZERO,
        EXCEPTION_PRIV_INSTRUCTION,
        EXCEPTION_STACK_OVERFLOW,
        STATUS_HEAP_CORRUPTION,
        STATUS_STACK_BUFFER_OVERRUN,
        STATUS_INVALID_CRUNTIME_PARAMETER,
        STATUS_FATAL_USER_CALLBACK_EXCEPTION,
    )
}

/// Returns `true` if the exception was logged; `false` for benign codes.
unsafe fn log_exception(info: *const EXCEPTION_POINTERS, source: &str) -> bool {
    let Some(info) = (unsafe { info.as_ref() }) else {
        return false;
    };
    let Some(exc) = (unsafe { info.ExceptionRecord.as_ref() }) else {
        return false;
    };
    let Some(ctx) = (unsafe { info.ContextRecord.as_ref() }) else {
        return false;
    };

    let code = exc.ExceptionCode;
    if matches!(
        code,
        EXCEPTION_BREAKPOINT
            | EXCEPTION_SINGLE_STEP
            | DBG_PRINTEXCEPTION_C
            | DBG_PRINTEXCEPTION_WIDE_C
            | MS_VC_THREAD_NAME
            | MS_VC_CXX_EH,
    ) {
        if let Some(n) = BENIGN_LOG.tick() {
            let address = exc.ExceptionAddress;
            let display_n = n + 1;
            info!(
                "first-chance ({source}) #{display_n}: code={code:#010x} address={address:p} (benign, continuing)",
            );
        }
        return false;
    }

    let address = exc.ExceptionAddress;
    let code_name = exception_name(code);
    let mut msg =
        format!("EXCEPTION ({source}): code={code:#010x} ({code_name}) address={address:p}");
    if code == EXCEPTION_ACCESS_VIOLATION {
        let info_arr = exc.ExceptionInformation;
        #[allow(clippy::cast_possible_truncation)]
        let access_type = info_arr[0] as u32;
        let bad_addr = info_arr[1];
        let access_name = match access_type {
            EXCEPTION_READ_FAULT => "read",
            EXCEPTION_WRITE_FAULT => "write",
            EXCEPTION_EXECUTE_FAULT => "DEP",
            _ => "?",
        };
        let _ = write!(msg, " access={access_name} bad_addr={bad_addr:#010x}");
    }
    error!("{msg}");
    error!(
        "registers: eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x}",
        ctx.Eax, ctx.Ebx, ctx.Ecx, ctx.Edx,
    );
    error!(
        "registers: esi={:#010x} edi={:#010x} ebp={:#010x} esp={:#010x}",
        ctx.Esi, ctx.Edi, ctx.Ebp, ctx.Esp,
    );
    error!(
        "registers: eip={:#010x} eflags={:#010x}",
        ctx.Eip, ctx.EFlags,
    );
    // For an indirect-call fault (`call ecx`), `[esp]` is the return address,
    // which pinpoints the call site to the byte.
    // The stack peek is last so register data is already flushed if this read itself faults.
    let esp = ctx.Esp;
    let mut stack = [0u32; 8];
    safe_read_stack(esp, &mut stack);
    info!(
        "[esp] = {:#010x} (return addr of crashed indirect call)",
        stack[0],
    );
    info!("stack [esp..esp+32]: {stack:#010x?}",);
    true
}

/// Vectored runs "first-chance": we re-arm `IN_FILTER` after handling
/// so the next exception can be processed, and we also skip the minidump
/// for benign codes since the log line is enough.
/// Unhandled runs "last-chance": the process is about to die,
/// so we don't bother re-arming and dump even on benign codes since we want all the details.
#[derive(Clone, Copy)]
enum ExceptionSource {
    Vectored,
    Unhandled,
}

impl ExceptionSource {
    fn label(self) -> &'static str {
        match self {
            Self::Vectored => "vectored",
            Self::Unhandled => "unhandled",
        }
    }

    fn release_filter(self) -> bool {
        matches!(self, Self::Vectored)
    }

    fn dump_on_benign(self) -> bool {
        matches!(self, Self::Unhandled)
    }
}

unsafe fn handle_exception(info: *const EXCEPTION_POINTERS, source: ExceptionSource) -> i32 {
    if IN_FILTER.swap(true, Ordering::Relaxed) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let label = source.label();
    let logged = unsafe { log_exception(info, label) };
    if logged || source.dump_on_benign() {
        let dump_path = unsafe { write_minidump(info, label) };
        if let Some(p) = dump_path {
            info!(kind = "minidump_written", path = %p.display());
        } else {
            info!(kind = "minidump_skipped");
        }
        flush();
    }
    if source.release_filter() {
        IN_FILTER.store(false, Ordering::Relaxed);
    }
    EXCEPTION_CONTINUE_SEARCH
}

unsafe extern "system" fn vectored_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    unsafe { handle_exception(info.cast_const(), ExceptionSource::Vectored) }
}

unsafe extern "system" fn unhandled_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    unsafe { handle_exception(info, ExceptionSource::Unhandled) }
}

pub(crate) fn install_handlers() {
    unsafe {
        // 1 = call our handler first in the dispatch order.
        AddVectoredExceptionHandler(1, Some(vectored_handler));
        SetUnhandledExceptionFilter(Some(unhandled_filter));
    }
}
