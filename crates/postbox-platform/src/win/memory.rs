//! Windows working-set trim and `GetProcessMemoryInfo` telemetry.
//!
//! All `unsafe` for the M5.5 RAM diet lives here, inside `crates/postbox-platform/src/win/`
//! where `#[allow(unsafe_code)]` is scoped; the rest of the workspace keeps
//! `unsafe_code = "deny"` from the workspace lints.

use crate::{PlatformError, ProcessMemoryStats};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    SETPROCESSWORKINGSETSIZEEX_FLAGS, SetProcessWorkingSetSizeEx,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

/// No flags: trim the working set now without touching the process' hard working-set quota
/// limits, so a later render can grow the set again on demand.
const TRIM_FLAGS: SETPROCESSWORKINGSETSIZEEX_FLAGS = SETPROCESSWORKINGSETSIZEEX_FLAGS(0);

/// `SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0)`, the documented
/// "trim now" call. `usize::MAX` for both bounds asks Windows to move every reclaimable page out
/// of the working set.
pub fn trim_working_set() -> Result<(), PlatformError> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle owned by the OS that needs no close,
    // and `SetProcessWorkingSetSizeEx` only reads the two `usize` bounds and the flag bits.
    unsafe {
        SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, TRIM_FLAGS)
            .map_err(|_| PlatformError::OsUnavailable)
    }
}

/// Reads `PROCESS_MEMORY_COUNTERS_EX` for the current process.
pub fn process_memory_info() -> Result<ProcessMemoryStats, PlatformError> {
    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb,
        ..Default::default()
    };
    // SAFETY: `counters` is a fully initialized `PROCESS_MEMORY_COUNTERS_EX` whose `cb` field
    // states its exact size, `GetCurrentProcess` is a stable pseudo-handle, and the struct
    // outlives the call. `PROCESS_MEMORY_COUNTERS_EX` begins with the `PROCESS_MEMORY_COUNTERS`
    // layout the API declares, so the cast is a documented widening.
    unsafe {
        let handle: HANDLE = GetCurrentProcess();
        GetProcessMemoryInfo(handle, std::ptr::addr_of_mut!(counters).cast(), cb)
            .map_err(|_| PlatformError::OsUnavailable)?;
    }
    Ok(ProcessMemoryStats {
        working_set_bytes: counters.WorkingSetSize as u64,
        private_usage_bytes: counters.PrivateUsage as u64,
    })
}
