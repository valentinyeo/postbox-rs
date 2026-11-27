//! Windows shell file opening strictly quarantined in `crates/postbox-platform/src/win/`.

use crate::PlatformError;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

pub fn open_external_file(path: &Path) -> Result<(), PlatformError> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated UTF-16 and `w!("open")` is a static UTF-16 literal.
    // Caller (`open_external_image`) has already enforced the strict 12-extension image allowlist.
    let res = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if (res.0 as isize) > 32 {
        Ok(())
    } else {
        Err(PlatformError::OsUnavailable)
    }
}
