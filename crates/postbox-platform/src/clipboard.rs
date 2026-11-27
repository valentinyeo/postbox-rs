use crate::PlatformError;
use std::path::{Path, PathBuf};

/// Exact NTFS `Zone.Identifier` Alternate Data Stream body marking internet-downloaded files
/// (`attachments.zig:14-22`, `winhttp.zig:262-273`).
pub const ZONE_IDENTIFIER_CONTENT: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

/// Maximum clipboard byte payload copied from global memory (64 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;

/// Maximum pixel count allowed when extracting `CF_BITMAP` from Windows clipboard (`24 MiB / 4`).
pub const MAX_CLIPBOARD_PIXELS: u64 = 6_291_456;

/// Exact 12-extension allowlist for external OS image preview fallback (`attachments.zig:3-12`).
pub const ALLOWED_EXTERNAL_IMAGE_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tif", ".tiff", ".ico", ".avif", ".heic",
    ".heif",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    /// Clipboard holds text (`CF_UNICODETEXT` / `CF_TEXT`) — composer should paste text rather
    /// than intercepting as an image attachment (`main.zig:10472-10473`).
    Text,
    /// One or more file paths from `CF_HDROP`.
    Files(Vec<PathBuf>),
    /// Raw PNG bytes from the registered `"PNG"` clipboard format.
    PngBytes(Vec<u8>),
    /// Raw DIB/DIBV5 bytes from `CF_DIBV5` or `CF_DIB`.
    DibBytes(Vec<u8>),
    /// Decoded top-down RGBA8888 pixels (e.g., from `CF_BITMAP` `GetDIBits` fallback or mock).
    RgbaImage {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Clipboard has no supported pasteable media.
    Empty,
}

/// Safe OS clipboard paste-payload boundary.
pub trait ClipboardBackend: Send + Sync {
    fn read_paste_payload(&self) -> Result<ClipboardPayload, PlatformError>;
}

/// Mockable NTFS `Zone.Identifier` ADS marker for downloaded attachments.
pub trait ZoneMarker: Send + Sync {
    fn mark_downloaded_file(&self, path: &Path) -> Result<bool, PlatformError>;
}

/// Strict external image extension allowlist (`attachments.zig:3-12, 24-36`).
/// Accepts ONLY the 12 explicit ASCII image extensions at the very end of the path, rejecting
/// executables, scripts, double extensions (`.png.exe`), NTFS ADS (`:`), and trailing separators.
pub fn external_image_extension_allowed(path: &Path) -> bool {
    let Some(raw) = path.to_str() else {
        return false;
    };
    if raw.is_empty() || raw.as_bytes().contains(&0) {
        return false;
    }
    let filename = match raw.rfind(['/', '\\']) {
        Some(idx) => &raw[idx + 1..],
        None => raw,
    };
    if filename.is_empty() || filename.contains(':') {
        return false;
    }
    let bytes = filename.as_bytes();
    for &ext in ALLOWED_EXTERNAL_IMAGE_EXTENSIONS {
        let ext_bytes = ext.as_bytes();
        if bytes.len() <= ext_bytes.len() {
            continue;
        }
        let tail = &bytes[bytes.len() - ext_bytes.len()..];
        if tail.eq_ignore_ascii_case(ext_bytes) {
            return true;
        }
    }
    false
}

/// Writes the NTFS `Zone.Identifier` Alternate Data Stream (`[ZoneTransfer]\r\nZoneId=3\r\n`)
/// onto a downloaded media file on Windows using 100% safe Rust `std::fs::write`.
/// Gracefully returns `Ok(false)` on non-NTFS volumes (FAT32/exFAT) or non-Windows platforms.
pub fn mark_downloaded_file_zone(path: &Path) -> Result<bool, PlatformError> {
    #[cfg(windows)]
    {
        let mut ads = path.as_os_str().to_os_string();
        ads.push(":Zone.Identifier");
        match std::fs::write(PathBuf::from(ads), ZONE_IDENTIFIER_CONTENT) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Safe symlink / Windows NTFS reparse-point check used during 14-day cache pruning so eviction
/// NEVER follows or deletes through symlinks or junctions (`main.zig:7986-8039`).
pub fn is_reparse_point_or_symlink(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    let is_reparse = {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    };
    #[cfg(not(windows))]
    let is_reparse = false;
    meta.file_type().is_symlink() || is_reparse
}

/// Opens an image file in the OS external viewer ONLY if `external_image_extension_allowed(path)`
/// returns `true`. Strictly rejects executables, double extensions, and NTFS ADS streams.
pub fn open_external_image(path: &Path) -> Result<(), PlatformError> {
    if !external_image_extension_allowed(path) {
        return Err(PlatformError::InvalidPath);
    }
    #[cfg(windows)]
    {
        crate::win::shell::open_external_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_image_allowlist_accepts_only_12_extensions_and_rejects_tricks() {
        for ext in ALLOWED_EXTERNAL_IMAGE_EXTENSIONS {
            let p = PathBuf::from(format!("media/photo{ext}"));
            assert!(external_image_extension_allowed(&p), "should allow {ext}");
            let upper = PathBuf::from(format!("media/PHOTO{}", ext.to_ascii_uppercase()));
            assert!(
                external_image_extension_allowed(&upper),
                "should allow uppercase {ext}"
            );
        }

        for blocked in [
            "",
            ".png",
            "media/.png",
            "media\\document.cmd",
            "media\\payload.exe",
            "media\\script.ps1",
            "media\\page.html",
            "media\\vector.svg",
            "media\\photo.png.exe",
            "media\\photo.png:extra",
            "media\\photo.png:evil.png",
            "C:\\Users\\me\\payload.exe:fake.jpg",
            "doc.cmd:hidden.gif",
            "photo.png:Zone.Identifier",
            "photo.png:stream.webp",
            "evil.exe:fake.png",
            "media\\photo.png\\document",
        ] {
            assert!(
                !external_image_extension_allowed(Path::new(blocked)),
                "should reject {blocked}"
            );
        }
        for allowed in [
            "C:\\Users\\me\\photo.png",
            "\\\\?\\C:\\dir\\image.JPG",
            "media/Übung.png",
        ] {
            assert!(
                external_image_extension_allowed(Path::new(allowed)),
                "should allow {allowed}"
            );
        }
    }

    #[test]
    fn reparse_point_check_detects_symlinks_safely() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_plat_sym_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.txt");
        std::fs::write(&file, b"ok").unwrap();
        assert!(!is_reparse_point_or_symlink(&file));

        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(is_reparse_point_or_symlink(&link));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
