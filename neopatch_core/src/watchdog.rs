//! Watchdog for thread snapshots and hang diagnostics.
//!
//! The watchdog suspends every thread in the process except itself, captures `CONTEXT`
//! and the top of the stack, walks EBP frames, and emits an annotated log line per thread.

use crate::d3d9::present_count;
use crate::log::flush;
use crate::modules::{Module, annotate, annotate_resolved, walk_modules};
use crate::thread::main_id;
use crate::untrusted::safe_read_stack;
use std::ffi::c_void;
use std::mem::zeroed;
use std::num::NonZero;
use std::ptr::{null_mut, read_unaligned, with_exposed_provenance_mut};
use std::slice::from_raw_parts;
use std::thread::{Builder, sleep};
use std::time::Duration;
use tracing::info;
use windows_sys::Wdk::Foundation::{NtQueryObject, ObjectTypeInformation};
use windows_sys::Wdk::System::Threading::{
    NtQueryInformationThread, ThreadQuerySetWin32StartAddress,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_CONTROL_X86, CONTEXT_INTEGER_X86, CONTEXT_X86, GetThreadContext,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Subset of `CONTEXT_*` flags we actually need: integer (eax..edi)
/// and control (eip/esp/ebp/eflags/cs/ss). This is distinct from the SDK's `CONTEXT_FULL_X86`
/// which also pulls in `CONTEXT_SEGMENTS_X86`. We don't read ds/es/fs/gs.
const CONTEXT_FULL_X86_NO_SEGMENTS: u32 = CONTEXT_X86 | CONTEXT_CONTROL_X86 | CONTEXT_INTEGER_X86;

/// Leading `UNICODE_STRING` of `ObjectTypeInformation`. The kernel writes the name
/// into the trailing area of our buffer and sets `buffer` to point at it.
#[repr(C)]
#[derive(Clone, Copy)]
struct UnicodeStringHeader {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

/// Per-thread sample. EBP drives `walk_ebp_frames`.
/// In case the EBP walk terminates early, we fall back to the 64-word stack window.
struct ThreadSample {
    // Windows TIDs are non-zero by convention.
    tid: NonZero<u32>,
    eip: u32,
    esp: u32,
    ebp: u32,
    // `NtQueryInformationThread` returns the user-mode entry point
    // passed to `CreateThread`. A zero result means the query failed
    // or the thread is in a state where the address isn't yet recorded.
    start_addr: Option<NonZero<u32>>,
    stack: [u32; 64],
}

pub fn install() {
    Builder::new()
        .name("neopatch-watchdog".into())
        .spawn(watchdog_loop)
        .ok();
}

/// Returns the kernel-object type name (`Event`, `Mutex`, `Thread`, etc.)
/// for `handle`, or `None` on any failure.
fn lookup_handle_type(handle: NonZero<u32>) -> Option<String> {
    unsafe {
        let mut buf = [0u8; 1024];
        #[allow(clippy::cast_possible_truncation)]
        let buf_len = buf.len() as u32;
        let status = NtQueryObject(
            with_exposed_provenance_mut::<c_void>(handle.get() as usize),
            ObjectTypeInformation,
            buf.as_mut_ptr().cast(),
            buf_len,
            null_mut(),
        );
        if status < 0 {
            return None;
        }
        // The kernel writes the name string into the trailing area of our buffer and sets
        // `TypeName.Buffer` to point at it. We check the bounds of the kernel-supplied pointer
        // against our own buffer so a malformed reply can't redirect us at arbitrary kernel memory.
        let header: UnicodeStringHeader = read_unaligned(buf.as_ptr().cast());
        if header.length == 0 || header.buffer.is_null() {
            return None;
        }
        let buf_start = buf.as_ptr() as usize;
        let buf_end = buf_start.saturating_add(buf.len());
        let name_start = header.buffer as usize;
        let name_end = name_start.saturating_add(usize::from(header.length));
        if name_start < buf_start || name_end > buf_end || name_start & 1 != 0 {
            return None;
        }
        let len_chars = usize::from(header.length) / 2;
        let slice = from_raw_parts(header.buffer, len_chars);
        Some(String::from_utf16_lossy(slice))
    }
}

/// Run `f` with `h` suspended. The body runs between `SuspendThread` and `ResumeThread`.
///
/// # Safety
/// `h` must be a valid thread handle with `THREAD_SUSPEND_RESUME` access.
/// `f` must not allocate or otherwise take a lock the suspended thread may hold;
/// if `h` is inside that critical section, the closure deadlocks against it.
unsafe fn with_suspended<R>(h: HANDLE, f: impl FnOnce() -> R) -> R {
    // We don't use a RAII guard because we have `panic = "abort"`.
    unsafe {
        SuspendThread(h);
    }
    let r = f();
    unsafe {
        ResumeThread(h);
    }
    r
}

/// Returns the function passed to `CreateThread` for `thread_handle`.
fn lookup_thread_start(thread_handle: HANDLE) -> Option<NonZero<u32>> {
    unsafe {
        let mut start: u32 = 0;
        let mut returned: u32 = 0;
        #[allow(clippy::cast_possible_truncation)]
        let u32_size = size_of::<u32>() as u32;
        let status = NtQueryInformationThread(
            thread_handle,
            ThreadQuerySetWin32StartAddress,
            (&raw mut start).cast(),
            u32_size,
            &raw mut returned,
        );
        if status >= 0 && returned == u32_size {
            NonZero::new(start)
        } else {
            None
        }
    }
}

/// Opens `tid`, suspends it, snapshots its `CONTEXT` and the top of its stack, then resumes.
/// Returns `None` for `tid = 0`, a thread that cannot be opened, or a thread whose context
/// cannot be read.
fn sample_thread(tid: u32) -> Option<ThreadSample> {
    let tid = NonZero::new(tid)?;
    // SAFETY: `with_suspended` requires `h` valid (non-null after `OpenThread`)
    // and a non-allocating closure; ours only reads thread context into stack locals.
    unsafe {
        let access = THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION;
        let h: HANDLE = OpenThread(access, 0, tid.get());
        if h.is_null() {
            return None;
        }
        // `SuspendThread` can transiently fail (e.g., target thread is terminating).
        // We don't check the returned value because a redundant `ResumeThread`
        // on a thread we never successfully suspended is harmless, and `GetThreadContext`
        // below fails on its own if the thread is gone, falling through to the `None` branch.
        let sampled = with_suspended(h, || {
            let mut ctx: CONTEXT = zeroed();
            ctx.ContextFlags = CONTEXT_FULL_X86_NO_SEGMENTS;
            if GetThreadContext(h, &raw mut ctx) == 0 {
                return None;
            }
            let mut stack = [0u32; 64];
            safe_read_stack(ctx.Esp, &mut stack);
            Some((ctx.Eip, ctx.Esp, ctx.Ebp, stack))
        });
        // The start address is fixed at thread creation,
        // so reading it outside of the suspension window is sound.
        let start_addr = lookup_thread_start(h);
        CloseHandle(h);
        sampled.map(|(eip, esp, ebp, stack)| ThreadSample {
            tid,
            eip,
            esp,
            ebp,
            start_addr,
            stack,
        })
    }
}

/// Samples every thread except `skip_tid` and the watchdog itself.
fn enumerate_thread_samples(skip_tid: u32) -> Vec<ThreadSample> {
    let mut out = Vec::with_capacity(16);
    unsafe {
        let pid = GetCurrentProcessId();
        let self_tid = GetCurrentThreadId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: THREADENTRY32 = zeroed();
        #[allow(clippy::cast_possible_truncation)]
        let entry_size = size_of::<THREADENTRY32>() as u32;
        entry.dwSize = entry_size;
        if Thread32First(snap, &raw mut entry) == 0 {
            CloseHandle(snap);
            return out;
        }
        loop {
            if entry.th32OwnerProcessID == pid
                && entry.th32ThreadID != self_tid
                && entry.th32ThreadID != skip_tid
                && let Some(sample) = sample_thread(entry.th32ThreadID)
            {
                out.push(sample);
            }
            if Thread32Next(snap, &raw mut entry) == 0 {
                break;
            }
        }
        CloseHandle(snap);
    }
    out
}

fn watchdog_loop() -> ! {
    let mut iter: u64 = 0;
    let mut prev_frame: Option<u32> = None;
    loop {
        sleep(Duration::from_secs(1));
        iter += 1;
        // Make the last tick's events durable before the next sleep
        // so a crash between wakes doesn't lose `BufWriter` contents.
        flush();
        let frame = present_count();
        // If there was no `Present` since the last tick, then `main` isn't advancing,
        // which is when the diagnostic snapshot is interesting.
        // On the happy path, we just emit a liveness-only tick instead.
        let stuck = prev_frame == Some(frame);
        prev_frame = Some(frame);
        if !stuck {
            info!(kind = "watchdog_tick", iter, frame);
            continue;
        }
        snapshot_stuck(iter, frame);
    }
}

/// Walks all loaded modules, samples `main` and every other thread, and walks EBP chains
/// and stack words. This is a heavyweight diagnostic and is only called
/// when the frame counter hasn't advanced for ~1 second.
fn snapshot_stuck(iter: u64, frame: u32) {
    let modules = walk_modules();
    let main_tid = main_id();
    // Before the first hook triggers, we don't know which thread is the renderer,
    // so we log all threads.
    if main_tid == 0 {
        info!("watchdog #{iter} frame={frame}: (render thread not yet identified)");
        for sample in enumerate_thread_samples(0) {
            log_thread_header(&sample, &modules);
        }
        return;
    }
    let Some(s) = sample_thread(main_tid) else {
        info!("watchdog #{iter} frame={frame}: (main thread sample unavailable)");
        return;
    };
    info!(
        "watchdog #{iter} frame={frame}: eip={} esp={:#010x} ebp={}",
        annotate(s.eip, &modules),
        s.esp,
        annotate(s.ebp, &modules),
    );
    // `[esp+4]` is the first arg of the innermost wait wrapper;
    // typically the `HANDLE` for `WaitForSingleObject*`. If it's a `Thread`,
    // then that's what `main` is blocked on, so we stack-walk that thread specifically
    // and emit only headers for the rest.
    let handle_value = s.stack[1];
    let mut wait_target_tid: Option<NonZero<u32>> = None;
    if let Some(handle) = NonZero::new(handle_value)
        && let Some(ty) = lookup_handle_type(handle)
    {
        let raw_tid =
            unsafe { GetThreadId(with_exposed_provenance_mut::<c_void>(handle.get() as usize)) };
        wait_target_tid = NonZero::new(raw_tid);
        if let Some(tid) = wait_target_tid {
            info!("  wait handle: {handle_value:#010x} type={ty} -> tid={tid}");
        } else {
            info!("  wait handle: {handle_value:#010x} type={ty}");
        }
    }
    let main_frames = walk_ebp_frames(s.ebp, s.esp, &modules);
    for (i, frame) in main_frames.iter().enumerate() {
        info!("  frame {i}: {frame}");
    }
    // Resolved-only stack-window context, which covers
    // FPO frames the EBP walk can't traverse and non-return-address values.
    for (i, &w) in s.stack.iter().enumerate() {
        if let Some(label) = annotate_resolved(w, &modules) {
            info!("  [esp+{:#x}] = {label}", i * 4,);
        }
    }
    let others = enumerate_thread_samples(main_tid);
    for sample in others {
        log_thread_header(&sample, &modules);
        // Full chain only for the wait-target thread.
        if Some(sample.tid) == wait_target_tid {
            let frames = walk_ebp_frames(sample.ebp, sample.esp, &modules);
            for (i, frame) in frames.iter().enumerate() {
                info!("    frame {i}: {frame}");
            }
            for (i, &w) in sample.stack.iter().enumerate() {
                if let Some(label) = annotate_resolved(w, &modules) {
                    info!("    [esp+{:#x}] = {label}", i * 4,);
                }
            }
        }
    }
}

/// Logs a summary of a non-main thread sample: tid, eip, start address,
/// and (if applicable) the kernel object the thread is waiting on.
fn log_thread_header(sample: &ThreadSample, modules: &[Module]) {
    let start_label = sample.start_addr.map_or_else(
        || String::from("<unavailable>"),
        |a| annotate(a.get(), modules),
    );
    let handle = sample.stack[1];
    let handle_suffix = NonZero::new(handle)
        .and_then(lookup_handle_type)
        .map(|ty| format!(" wait={handle:#010x} type={ty}"))
        .unwrap_or_default();
    info!(
        "  thread {}: eip={} start={start_label}{handle_suffix}",
        sample.tid,
        annotate(sample.eip, modules),
    );
}

/// Walks the saved-EBP linked list and returns up to `MAX_FRAMES` annotated return addresses.
/// Stops when EBP leaves the heuristic stack range, becomes misaligned,
/// or stops strictly increasing.
fn walk_ebp_frames(initial_ebp: u32, esp: u32, modules: &[Module]) -> Vec<String> {
    const MAX_FRAMES: usize = 32;
    const MAX_STACK_SPAN: u32 = 0x0010_0000;
    if esp == 0 {
        return Vec::new();
    }
    let stack_lo = esp;
    let stack_hi = esp.saturating_add(MAX_STACK_SPAN);
    let mut out = Vec::new();
    let mut ebp = initial_ebp;
    let mut prev = 0u32;
    for _ in 0..MAX_FRAMES {
        if ebp & 0x3 != 0 || ebp < stack_lo || ebp >= stack_hi || ebp <= prev {
            break;
        }
        prev = ebp;
        // The range and alignment heuristic doesn't guarantee `ebp` is on a committed page.
        // We use `safe_read_stack` to return a partial copy at a guard-page boundary
        // instead of AVing.
        let mut pair = [0u32; 2];
        let words = safe_read_stack(ebp, &mut pair);
        if words < 2 {
            break;
        }
        let saved_ebp = pair[0];
        let ret_addr = pair[1];
        out.push(annotate(ret_addr, modules));
        ebp = saved_ebp;
    }
    out
}
