//! Loaded-module enumeration and address symbolication.

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HMODULE, MAX_PATH};
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Half-open range `[base, end)` of a loaded module.
///
/// `Copy` is implemented so it can be stored in a `OnceLock`
/// and be passed by value to the patcher.
#[derive(Clone, Copy)]
pub(crate) struct ModuleRange {
    pub base: u32,
    pub end: u32,
}

impl ModuleRange {
    pub(crate) fn contains(&self, addr: u32) -> bool {
        addr >= self.base && addr < self.end
    }
}

/// `ModuleRange` plus the leaf filename, for `name+0xoffset` annotations.
pub(crate) struct Module {
    pub(crate) range: ModuleRange,
    pub(crate) name: String,
}

/// Returns `None` for a null handle or when `GetModuleInformation` fails.
pub(crate) fn module_info(h: HMODULE) -> Option<ModuleRange> {
    if h.is_null() {
        return None;
    }
    unsafe {
        let mut info = MODULEINFO {
            lpBaseOfDll: null_mut(),
            SizeOfImage: 0,
            EntryPoint: null_mut(),
        };
        let info_size = u32::try_from(size_of::<MODULEINFO>()).unwrap_or(0);
        if GetModuleInformation(GetCurrentProcess(), h, &raw mut info, info_size) == 0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let base = info.lpBaseOfDll as u32;
        Some(ModuleRange {
            base,
            end: base.wrapping_add(info.SizeOfImage),
        })
    }
}

/// Enumerates every module loaded into the current process.
/// Each entry carries `base`, `end`, and leaf filename for symbolication.
pub(crate) fn walk_modules() -> Vec<Module> {
    // Probably fine for th15.
    const HANDLES_CAP: u32 = 512;
    const HANDLES_LEN: usize = HANDLES_CAP as usize;
    #[allow(clippy::cast_possible_truncation)]
    const BUF_BYTES: u32 = HANDLES_CAP * size_of::<HMODULE>() as u32;

    let mut result: Vec<Module> = Vec::new();
    unsafe {
        let process = GetCurrentProcess();
        let mut handles: [HMODULE; HANDLES_LEN] = [null_mut(); HANDLES_LEN];
        let mut needed: u32 = 0;
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

pub(crate) fn annotate(addr: u32, modules: &[Module]) -> String {
    annotate_resolved(addr, modules).unwrap_or_else(|| format!("{addr:#010x}"))
}

/// Like `annotate`, but returns `None` for unresolved addresses
/// so callers can skip them entirely.
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
