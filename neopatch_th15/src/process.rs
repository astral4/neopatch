//! Process-level tunables applied once at `DllMain`.

use crate::config::{PriorityClass, ProcessCfg};
use tracing::info;
use windows::core::w;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, AvSetMmThreadCharacteristicsW, BELOW_NORMAL_PRIORITY_CLASS,
    GetCurrentProcess, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_CREATION_FLAGS, SetPriorityClass, SetProcessAffinityMask,
};

pub(crate) fn apply(cfg: &ProcessCfg) {
    if let Some(pc) = priority_class(cfg.priority) {
        let ok = unsafe { SetPriorityClass(GetCurrentProcess(), pc) };
        let os_error = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        info!(
            kind = "set_priority_class",
            priority = %cfg.priority,
            pc = format_args!("{pc:#x}"),
            ok = ok != 0,
            os_error = format_args!("{os_error:#x}"),
        );
    }
    if let Some(mask) = cfg.affinity_mask {
        let ok = unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask.get() as usize) };
        let os_error = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        info!(
            kind = "set_affinity_mask",
            mask = format_args!("{:#x}", mask.get()),
            ok = ok != 0,
            os_error = format_args!("{os_error:#x}"),
        );
    }
    apply_mmcss();
}

/// Registers the main thread with the MMCSS "Games" task class.
fn apply_mmcss() {
    let mut task_idx: u32 = 0;
    let h = unsafe { AvSetMmThreadCharacteristicsW(w!("Games").as_ptr(), &raw mut task_idx) };
    let os_error = if h.is_null() {
        unsafe { GetLastError() }
    } else {
        0
    };
    info!(
        kind = "mmcss_register",
        ok = !h.is_null(),
        os_error = format_args!("{os_error:#x}"),
    );
}

fn priority_class(p: PriorityClass) -> Option<PROCESS_CREATION_FLAGS> {
    match p {
        PriorityClass::Unchanged => None,
        PriorityClass::Idle => Some(IDLE_PRIORITY_CLASS),
        PriorityClass::BelowNormal => Some(BELOW_NORMAL_PRIORITY_CLASS),
        PriorityClass::Normal => Some(NORMAL_PRIORITY_CLASS),
        PriorityClass::AboveNormal => Some(ABOVE_NORMAL_PRIORITY_CLASS),
        PriorityClass::High => Some(HIGH_PRIORITY_CLASS),
    }
}
