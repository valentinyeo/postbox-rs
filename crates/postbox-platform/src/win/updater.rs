//! `MoveFileExW` atomic swap and the relaunch handoff.
//!
//! The swap is the only place the installed binary changes. `MOVEFILE_REPLACE_EXISTING`
//! plus `MOVEFILE_WRITE_THROUGH` means Windows performs the replacement as one rename on the
//! same volume, so the executable is either the old file or the new one and never missing.
//!
//! The release zip reader lives in [`crate::zip`] so its deflate path is tested on every
//! OS; `ZipExtractor` is re-exported here for the call sites that already name it.

use crate::PlatformError;
use crate::updater::AtomicReplacer;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

fn wide(path: &Path) -> Vec<u16> {
    OsString::from(path).encode_wide().chain([0]).collect()
}

/// `MoveFileExW(from, to, REPLACE_EXISTING | WRITE_THROUGH)`.
pub fn move_replace(from: &Path, to: &Path) -> Result<(), PlatformError> {
    let wide_from = wide(from);
    let wide_to = wide(to);
    // SAFETY: both paths are NUL-terminated UTF-16 buffers that outlive the call. The flags
    // make the replacement atomic on one volume and flush it before returning.
    unsafe {
        MoveFileExW(
            PCWSTR(wide_from.as_ptr()),
            PCWSTR(wide_to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| PlatformError::OsUnavailable)
}

pub use crate::zip::{ZipEntry, ZipExtractor, extract_entry, find_zip_entry};

/// Windows implementation of [`AtomicReplacer`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsReplacer;

impl AtomicReplacer for WindowsReplacer {
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
        move_replace(staged, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_atomic_swap_is_one_replace_existing_write_through_rename() {
        let dir = std::env::temp_dir().join(format!("postbox-move-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("postbox.exe.staged.exe");
        let live = dir.join("postbox.exe");
        std::fs::write(&staged, b"new build").unwrap();
        std::fs::write(&live, b"old build").unwrap();

        WindowsReplacer
            .replace(&staged, &live)
            .expect("swap succeeds");

        assert_eq!(std::fs::read(&live).unwrap(), b"new build");
        assert!(!staged.exists(), "the staged file is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
