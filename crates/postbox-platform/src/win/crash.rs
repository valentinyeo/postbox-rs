//! Last-chance SEH reporting without using the heap in a damaged process.

use crate::exit_records::PENDING_CRASH_FILE;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use windows::Win32::Foundation::{GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_WRITE_THROUGH,
};
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};

struct CrashTarget {
    path: Vec<u16>,
    prefix: Vec<u8>,
}

static TARGET: OnceLock<CrashTarget> = OnceLock::new();

pub fn install_unhandled_exception_filter(data_dir: &Path, version: &str) {
    let _ = std::fs::create_dir_all(data_dir);
    TARGET.get_or_init(|| CrashTarget {
        path: data_dir
            .join(PENDING_CRASH_FILE)
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect(),
        prefix: format!(
            "{{\"kind\":\"seh_exception\",\"version\":{},\"os\":\"windows-x86_64\",",
            serde_json::to_string(version).unwrap_or_else(|_| "\"unknown\"".to_owned())
        )
        .into_bytes(),
    });
    // SAFETY: the callback and its immutable target remain valid for the process lifetime.
    let _ = unsafe { SetUnhandledExceptionFilter(Some(exception_filter)) };
}

// Raw calls avoid the windows crate's allocating error conversion on failed I/O.
windows::core::link!("kernel32.dll" "system" fn CreateFileW(
    name: *const u16, access: u32, share: u32, security: *const core::ffi::c_void,
    disposition: u32, flags: u32, template: HANDLE,
) -> HANDLE);
windows::core::link!("kernel32.dll" "system" fn WriteFile(
    file: HANDLE, buffer: *const u8, len: u32, written: *mut u32,
    overlapped: *mut core::ffi::c_void,
) -> windows::core::BOOL);
windows::core::link!("kernel32.dll" "system" fn CloseHandle(file: HANDLE) -> windows::core::BOOL);

unsafe fn write_all(file: HANDLE, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let mut written = 0;
        // SAFETY: the synchronous call borrows a live slice and stack byte count.
        let ok = unsafe {
            WriteFile(
                file,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if !ok.as_bool() || written == 0 || written as usize > bytes.len() {
            return;
        }
        bytes = &bytes[written as usize..];
    }
}

fn hex(mut value: u64, output: &mut [u8]) {
    for byte in output.iter_mut().rev() {
        *byte = b"0123456789abcdef"[(value & 15) as usize];
        value >>= 4;
    }
}

unsafe extern "system" fn exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let Some(target) = TARGET.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    // SAFETY: Windows supplies these records for the duration of the callback.
    let Some(info) = (unsafe { info.as_ref() }) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let Some(record) = (unsafe { info.ExceptionRecord.as_ref() }) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let mut code = [b'0'; 8];
    let mut address = [b'0'; 16];
    hex(record.ExceptionCode.0 as u32 as u64, &mut code);
    hex(record.ExceptionAddress as usize as u64, &mut address);
    // SAFETY: the UTF-16 path is NUL-terminated and lives in the startup OnceLock.
    let file = unsafe {
        CreateFileW(
            target.path.as_ptr(),
            GENERIC_WRITE.0,
            0,
            std::ptr::null(),
            CREATE_ALWAYS.0,
            FILE_ATTRIBUTE_NORMAL.0 | FILE_FLAG_WRITE_THROUGH.0,
            HANDLE::default(),
        )
    };
    if !file.is_invalid() {
        // SAFETY: the handle is exclusively owned here; all buffers stay live until written.
        unsafe {
            write_all(file, &target.prefix);
            write_all(file, b"\"exception_code\":\"0x");
            write_all(file, &code);
            write_all(file, b"\",\"exception_address\":\"0x");
            write_all(file, &address);
            write_all(file, b"\"}");
            let _ = CloseHandle(file);
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::NTSTATUS;
    use windows::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD;

    #[test]
    fn seh_filter_writes_valid_json_and_continues_exception_search() {
        let dir = std::env::temp_dir().join(format!("postbox-seh-{}", std::process::id()));
        install_unhandled_exception_filter(&dir, "0.1.0\"test\\version");
        let mut record = EXCEPTION_RECORD {
            ExceptionCode: NTSTATUS(0xc0000005_u32 as i32),
            ExceptionAddress: 0x1234_usize as *mut core::ffi::c_void,
            ..Default::default()
        };
        let info = EXCEPTION_POINTERS {
            ExceptionRecord: &mut record,
            ContextRecord: std::ptr::null_mut(),
        };
        // SAFETY: the synthetic exception record stays live for the callback.
        assert_eq!(
            unsafe { exception_filter(&info) },
            EXCEPTION_CONTINUE_SEARCH
        );
        let text = std::fs::read_to_string(dir.join(PENDING_CRASH_FILE)).unwrap();
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["kind"], "seh_exception");
        assert_eq!(report["version"], "0.1.0\"test\\version");
        assert_eq!(report["os"], "windows-x86_64");
        assert_eq!(report["exception_code"], "0xc0000005");
        assert_eq!(report["exception_address"], "0x0000000000001234");
        let _ = std::fs::remove_dir_all(dir);
    }
}
