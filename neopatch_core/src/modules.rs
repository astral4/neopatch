//! Loaded-module enumeration and address-to-module resolution.

use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use tracing::warn;
use windows_sys::Win32::Foundation::{HMODULE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
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

/// [`ModuleRange`] plus the leaf filename, for `name+0xoffset` annotations.
pub(crate) struct Module {
    pub(crate) range: ModuleRange,
    pub(crate) name: String,
}

/// Returns `None` if the handle is null or `GetModuleInformation` fails.
pub(crate) fn module_info(h: HMODULE) -> Option<ModuleRange> {
    #[allow(clippy::cast_possible_truncation)]
    const MODULEINFO_SIZE: u32 = size_of::<MODULEINFO>() as u32;

    if h.is_null() {
        return None;
    }

    unsafe {
        let mut info = MODULEINFO {
            lpBaseOfDll: null_mut(),
            SizeOfImage: 0,
            EntryPoint: null_mut(),
        };
        if GetModuleInformation(GetCurrentProcess(), h, &raw mut info, MODULEINFO_SIZE) == 0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let base = info.lpBaseOfDll.addr() as u32;
        Some(ModuleRange {
            base,
            end: base.wrapping_add(info.SizeOfImage),
        })
    }
}

/// Enumerates every module loaded into the current process.
/// Each entry carries `base`, `end`, and leaf filename for resolving an address to its module.
pub(crate) fn walk_modules() -> Vec<Module> {
    const HANDLES_CAP: u32 = 512;
    const HANDLES_LEN: usize = HANDLES_CAP as usize;
    #[allow(clippy::cast_possible_truncation)]
    const BUF_BYTES: u32 = HANDLES_CAP * size_of::<HMODULE>() as u32;

    let mut result = Vec::new();
    unsafe {
        let process = GetCurrentProcess();
        let mut handles = [null_mut(); HANDLES_LEN];
        let mut needed = 0;
        if EnumProcessModules(process, handles.as_mut_ptr(), BUF_BYTES, &raw mut needed) == 0 {
            return result;
        }
        let count = (needed as usize / size_of::<HMODULE>()).min(handles.len());
        result.reserve_exact(count);
        for &module in &handles[..count] {
            let Some(range) = module_info(module) else {
                continue;
            };
            let mut name_buf = [0u16; MAX_PATH as usize];
            let name_len = GetModuleFileNameExW(process, module, name_buf.as_mut_ptr(), MAX_PATH);
            let mut name = if name_len == 0 {
                String::from("<unknown>")
            } else {
                String::from_utf16_lossy(&name_buf[..name_len as usize])
            };
            if let Some(slash) = name.rfind('\\') {
                name.drain(..=slash);
            }
            result.push(Module { range, name });
        }
    }
    result
}

/// Resolves `addr` to a `module+offset` label, or `None` for an address in no known module.
pub(crate) fn annotate_resolved(addr: u32, modules: &[Module]) -> Option<String> {
    if addr == 0 {
        return None;
    }
    for m in modules {
        if m.range.contains(addr) {
            return Some(format!(
                "{:#010x} ({}+{:#x})",
                addr,
                m.name,
                addr - m.range.base,
            ));
        }
    }
    None
}
