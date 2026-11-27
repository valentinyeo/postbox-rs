//! Windows shell file opening strictly quarantined in `crates/postbox-platform/src/win/`.

use crate::PlatformError;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, PWSTR, w};

pub fn pick_attachment_file() -> Result<Option<PathBuf>, PlatformError> {
    let mut buffer = vec![0u16; 32_768];
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: PWSTR(buffer.as_mut_ptr()),
        nMaxFile: buffer.len() as u32,
        lpstrFilter: w!("All files\0*.*\0"),
        lpstrTitle: w!("Attach file"),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: the dialog and its writable UTF-16 buffer remain valid for the modal call.
    if unsafe { GetOpenFileNameW(&mut dialog) }.as_bool() {
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Ok(Some(PathBuf::from(std::ffi::OsString::from_wide(
            &buffer[..len],
        ))))
    } else {
        // SAFETY: reads this thread's last common-dialog error; zero means cancellation.
        if unsafe { CommDlgExtendedError() }.0 == 0 {
            Ok(None)
        } else {
            Err(PlatformError::OsUnavailable)
        }
    }
}

pub fn open_external_file(path: &Path) -> Result<(), PlatformError> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated UTF-16 and `w!("open")` is a static UTF-16 literal.
    // Callers enforce the image or video extension allowlist before invoking the shell.
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
