//! Address-to-module resolution.

use std::ptr::{null, null_mut, without_provenance};
use std::sync::OnceLock;
use tracing::warn;
use windows_sys::Win32::Foundation::{HMODULE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW, GetModuleHandleW,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Half-open range `[base, end)` of a loaded module.
#[derive(Clone, Copy)]
pub(crate) struct ModuleRange {
    pub(crate) base: u32,
    pub(crate) end: u32,
}

impl ModuleRange {
    pub(crate) fn contains(&self, addr: u32) -> bool {
        addr >= self.base && addr < self.end
    }

    /// Returns whether the entirety of the half-open span `[addr, addr + len)` lies inside this range.
    /// A `len` of 0 or an `addr + len` that overflows is not contained.
    fn contains_span(&self, addr: u32, len: usize) -> bool {
        let Ok(len) = u32::try_from(len) else {
            return false;
        };
        let Some(last) = addr.checked_add(len) else {
            return false;
        };
        len != 0 && addr >= self.base && last <= self.end
    }
}

/// Returns the host executable's image range, or `None` if it can't be resolved.
fn host_range() -> Option<ModuleRange> {
    static HOST_RANGE: OnceLock<Option<ModuleRange>> = OnceLock::new();
    *HOST_RANGE.get_or_init(|| {
        let range = module_info(unsafe { GetModuleHandleW(null()) });
        if range.is_none() {
            warn!(kind = "host_range_unresolved");
        }
        range
    })
}

/// Returns whether `[addr, addr + len)` is within the host executable's image range.
pub(crate) fn in_host_image(addr: usize, len: usize) -> bool {
    host_range().is_some_and(|r| u32::try_from(addr).is_ok_and(|addr| r.contains_span(addr, len)))
}

/// Returns `None` if the handle is null or `GetModuleInformation` fails.
pub(crate) fn module_info(h: HMODULE) -> Option<ModuleRange> {
    #[allow(clippy::cast_possible_truncation)]
    const MODULEINFO_SIZE: u32 = size_of::<MODULEINFO>() as u32;

    if h.is_null() {
        return None;
    }

    let mut info = MODULEINFO {
        lpBaseOfDll: null_mut(),
        SizeOfImage: 0,
        EntryPoint: null_mut(),
    };
    if unsafe { GetModuleInformation(GetCurrentProcess(), h, &raw mut info, MODULEINFO_SIZE) } == 0
    {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let base = info.lpBaseOfDll.addr() as u32;
    Some(ModuleRange {
        base,
        end: base.wrapping_add(info.SizeOfImage),
    })
}

/// Returns the image range of the module containing `addr`, or `None` if there is none.
pub(crate) fn module_containing(addr: usize) -> Option<ModuleRange> {
    module_info(module_handle_from_addr(addr)?)
}

/// Returns the module containing `addr` as a `module+offset` label, or `None` when `addr` is 0 or doesn't belong to a loaded module.
pub(crate) fn annotate_addr(addr: u32) -> Option<String> {
    if addr == 0 {
        return None;
    }

    let module = module_handle_from_addr(addr as usize)?;
    let range = module_info(module)?;
    let process = unsafe { GetCurrentProcess() };
    let mut name_buf = [0u16; MAX_PATH as usize];
    let name_len =
        unsafe { GetModuleFileNameExW(process, module, name_buf.as_mut_ptr(), MAX_PATH) };
    let mut name = if name_len == 0 {
        String::from("<unknown>")
    } else {
        String::from_utf16_lossy(&name_buf[..name_len as usize])
    };
    if let Some(slash) = name.rfind('\\') {
        name.drain(..=slash);
    }
    Some(format!(
        "{:#010x} ({}+{:#x})",
        addr,
        name,
        addr - range.base,
    ))
}

fn module_handle_from_addr(addr: usize) -> Option<HMODULE> {
    let mut module = null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            without_provenance(addr),
            &raw mut module,
        )
    };
    (ok != 0).then_some(module)
}
