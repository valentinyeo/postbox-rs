//! Windows unhandled-exception filter: records native crashes the Rust panic hook never sees.
//!
//! `SetUnhandledExceptionFilter` runs on the crashing thread while the process is dying, so this
//! module does almost nothing: it formats a fixed ASCII line into a stack buffer and appends it
//! with `CreateFileW`/`WriteFile`. No heap allocation, no locks, no logging framework.
//!
//! A fast-fail (`0xC0000409`) or `TerminateProcess` bypasses this filter entirely. The
//! dirty-session sentinel in `exit_records` covers those; this file only writes the concrete
//! exception facts when Windows gives us the chance.
//!
//! All `unsafe` for crash capture lives here, inside `crates/postbox-platform/src/win/`.

use postbox_core::crash::NATIVE_EXCEPTION_FILE;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS, WriteFile,
};
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::PCWSTR;

/// Log path, allocated once at install time so the filter itself never allocates.
static CRASH_LOG_PATH: OnceLock<Vec<u16>> = OnceLock::new();
static APP_VERSION: OnceLock<&'static str> = OnceLock::new();

/// Installs the filter. Best effort: a host that refuses must not stop the app from starting.
pub fn install(dir: &Path, version: &'static str) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(NATIVE_EXCEPTION_FILE);
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let _ = CRASH_LOG_PATH.set(wide);
    let _ = APP_VERSION.set(version);
    // SAFETY: `crash_filter` has the exact `LPTOP_LEVEL_EXCEPTION_FILTER` signature and only
    // reads process state that outlives the process.
    unsafe {
        SetUnhandledExceptionFilter(Some(crash_filter));
    }
}

/// Process id of this run, for the dirty-session sentinel.
pub fn current_pid() -> u32 {
    // SAFETY: `GetCurrentProcessId` reads a kernel-owned constant; no pointers involved.
    unsafe { GetCurrentProcessId() }
}

/// "Now" in Unix milliseconds. `SystemTime::now` is a syscall pair and allocates nothing.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bounded ASCII formatter so the filter never allocates.
struct Buf {
    bytes: [u8; 384],
    len: usize,
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

impl Buf {
    fn new() -> Self {
        Self {
            bytes: [0u8; 384],
            len: 0,
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn push_str(&mut self, text: &str) {
        for byte in text.as_bytes() {
            self.push_byte(if *byte < 0x80 { *byte } else { b'?' });
        }
    }

    fn push_dec(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut rest = value;
        loop {
            digits[count] = b'0' + (rest % 10) as u8;
            rest /= 10;
            count += 1;
            if rest == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            self.push_byte(digits[count]);
        }
    }

    /// Writes `value` as `0x` plus exactly eight upper-case hex digits.
    fn push_hex32(&mut self, value: u32) {
        self.push_str("0x");
        for shift in (0..8).rev() {
            let nibble = ((value >> (shift * 4)) & 0xF) as usize;
            self.push_byte(HEX_UPPER[nibble]);
        }
    }

    /// Writes `value` as lower-case hex without padding, for a module offset.
    fn push_hex_usize(&mut self, value: usize) {
        self.push_str("0x");
        if value == 0 {
            self.push_byte(b'0');
            return;
        }
        let mut started = false;
        for shift in (0..16).rev() {
            let nibble = (value >> (shift * 4)) & 0xF;
            if nibble == 0 && !started {
                continue;
            }
            started = true;
            self.push_byte(HEX_LOWER[nibble]);
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Copies the module file name (not the path) out of a UTF-16 buffer as plain ASCII.
fn module_name(out: &mut [u8], path_utf16: &[u16]) -> usize {
    let start = path_utf16
        .iter()
        .rposition(|unit| *unit == u16::from(b'\\'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut count = 0;
    for unit in &path_utf16[start..] {
        if count >= out.len() || *unit == 0 {
            break;
        }
        let byte = if (0x20..0x7F).contains(unit) {
            *unit as u8
        } else {
            b'?'
        };
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out[count] = byte;
            count += 1;
        }
    }
    count
}

/// Module file name and offset for the crashing address, when the loader can name it.
fn module_and_offset(address: usize) -> (String, usize) {
    if address == 0 {
        return (String::new(), 0);
    }
    let mut module = HMODULE(std::ptr::null_mut());
    // SAFETY: `GetModuleHandleExW` with `FROM_ADDRESS` only reads the loader's module list and
    // writes one module handle; the unchanged-refcount flag keeps it from pinning the module.
    let located = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut module,
        )
    };
    if located.is_err() {
        return (String::new(), 0);
    }
    let mut path = [0u16; 260];
    // SAFETY: `path` is a plain array of 260 UTF-16 units, the documented maximum the API fills.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut path) } as usize;
    let mut name = [0u8; 40];
    let count = module_name(&mut name, &path[..len.min(path.len())]);
    let name = String::from_utf8_lossy(&name[..count]).into_owned();
    // SAFETY: the module handle we hold is the base address the loader reported.
    let base = module.0 as usize;
    let offset = address.saturating_sub(base);
    (name, offset)
}

/// The unhandled-exception filter itself.
///
/// # Safety
/// Called by Windows on the crashing thread with either a valid `EXCEPTION_POINTERS` or null.
unsafe extern "system" fn crash_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let mut code = 0u32;
    let mut address = 0usize;
    if !info.is_null() {
        // SAFETY: `info` is non-null and owned by the OS for the duration of the call.
        let record = unsafe { (*info).ExceptionRecord };
        if !record.is_null() {
            // SAFETY: the exception record belongs to the same OS-owned structure.
            let record = unsafe { &*record };
            code = record.ExceptionCode.0 as u32;
            address = record.ExceptionAddress as usize;
        }
    }

    let Some(path) = CRASH_LOG_PATH.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let version = APP_VERSION.get().copied().unwrap_or("0.0.0");

    let (module, offset) = module_and_offset(address);
    let mut line = Buf::new();
    line.push_str("native-exception ts=");
    line.push_dec(now_unix_ms());
    line.push_str(" code=");
    line.push_hex32(code);
    line.push_str(" module=");
    line.push_str(if module.is_empty() { "?" } else { &module });
    line.push_str(" offset=");
    line.push_hex_usize(offset);
    line.push_str(" version=");
    line.push_str(version);
    line.push_str("\r\n");

    // SAFETY: `path` is a NUL-terminated UTF-16 path built at install time, the access mode is
    // append-only, and the handle is closed exactly once below.
    unsafe {
        let Ok(handle) = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_APPEND_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        let mut written = 0u32;
        let _ = WriteFile(handle, Some(line.as_bytes()), Some(&mut written), None);
        let _ = CloseHandle(handle);
    }
    EXCEPTION_CONTINUE_SEARCH
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::crash::ExitRecord;

    #[test]
    fn the_line_round_trips_through_the_shared_parser() {
        let mut line = Buf::new();
        line.push_str("native-exception ts=");
        line.push_dec(1_700_000_000_123);
        line.push_str(" code=");
        line.push_hex32(0xC000_0005);
        line.push_str(" module=");
        line.push_str("postbox.exe");
        line.push_str(" offset=");
        line.push_hex_usize(0x1a2b);
        line.push_str(" version=");
        line.push_str("0.1.1");
        let text = std::str::from_utf8(line.as_bytes()).unwrap();
        let record = ExitRecord::from_native_line(text).expect("the line parses");
        assert_eq!(
            record.exception_code.as_deref(),
            Some("0xC0000005"),
            "{text}"
        );
        assert_eq!(record.location.as_deref(), Some("postbox.exe+0x1a2b"));
        assert_eq!(record.timestamp_unix_ms, 1_700_000_000_123);
        assert_eq!(record.version, "0.1.1");
    }

    #[test]
    fn dec_and_hex_formatting_handle_the_edges() {
        let mut line = Buf::new();
        line.push_dec(0);
        line.push_str(" ");
        line.push_dec(u64::MAX);
        line.push_str(" ");
        line.push_hex32(0);
        line.push_str(" ");
        line.push_hex_usize(0);
        line.push_str(" ");
        line.push_hex_usize(0xDEAD_BEEF);
        assert_eq!(
            std::str::from_utf8(line.as_bytes()).unwrap(),
            "0 18446744073709551615 0x00000000 0x0 0xdeadbeef"
        );
    }

    #[test]
    fn module_names_keep_only_the_file_name() {
        let path: Vec<u16> = "C:\\Program Files\\Postbox\\postbox.exe"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut out = [0u8; 40];
        let count = module_name(&mut out, &path);
        assert_eq!(&out[..count], b"postbox.exe");
    }

    #[test]
    fn a_missing_address_reports_no_module() {
        assert_eq!(module_and_offset(0), (String::new(), 0));
    }
}
