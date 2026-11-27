//! `MoveFileExW` atomic swap, zip extraction and the relaunch handoff.
//!
//! The swap is the only place the installed binary changes. `MOVEFILE_REPLACE_EXISTING`
//! plus `MOVEFILE_WRITE_THROUGH` means Windows performs the replacement as one rename on the
//! same volume, so the executable is either the old file or the new one and never missing.

use crate::PlatformError;
use crate::updater::{ArchiveExtractor, AtomicReplacer};
use std::ffi::OsString;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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

/// Windows implementation of [`AtomicReplacer`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsReplacer;

impl AtomicReplacer for WindowsReplacer {
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
        move_replace(staged, target)
    }
}

/// Extracts `postbox.exe` from the release zip.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZipExtractor;

impl ArchiveExtractor for ZipExtractor {
    fn extract_executable(&self, archive: &Path, dest: &Path) -> Result<PathBuf, PlatformError> {
        let bytes = std::fs::read(archive)?;
        let entry = find_zip_entry(&bytes, crate::updater::EXE_ASSET_NAME)
            .or_else(|| find_zip_entry(&bytes, "postboxctl.exe"))
            .ok_or(PlatformError::OsUnavailable)?;
        let payload = inflate_stored_or_deflate(&bytes, &entry)?;
        std::fs::write(dest, &payload)?;
        Ok(dest.to_path_buf())
    }
}

/// One file inside a zip, located by its central-directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub compressed_size: usize,
    pub uncompressed_size: usize,
    pub local_header_offset: usize,
}

impl ZipEntry {
    /// `true` when the entry is stored uncompressed, so extraction is a plain copy.
    pub fn is_stored(&self) -> bool {
        self.method == 0
    }
}

const END_OF_CENTRAL_DIR: u32 = 0x0605_4b50;
const CENTRAL_DIR: u32 = 0x0201_4b50;
const LOCAL_HEADER: u32 = 0x0403_4b50;

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Finds the newest central-directory entry whose file name ends with `name`.
pub fn find_zip_entry(bytes: &[u8], name: &str) -> Option<ZipEntry> {
    let mut end = None;
    let limit = bytes.len().saturating_sub(22);
    for at in (0..=limit).rev() {
        if read_u32(bytes, at) == Some(END_OF_CENTRAL_DIR) {
            end = Some(at);
            break;
        }
    }
    let end = end?;
    let entries = read_u16(bytes, end + 10)? as usize;
    let mut cursor = read_u32(bytes, end + 16)? as usize;

    for _ in 0..entries {
        if read_u32(bytes, cursor)? != CENTRAL_DIR {
            return None;
        }
        let method = read_u16(bytes, cursor + 10)?;
        let compressed_size = read_u32(bytes, cursor + 20)? as usize;
        let uncompressed_size = read_u32(bytes, cursor + 24)? as usize;
        let name_len = read_u16(bytes, cursor + 28)? as usize;
        let extra_len = read_u16(bytes, cursor + 30)? as usize;
        let comment_len = read_u16(bytes, cursor + 32)? as usize;
        let local_header_offset = read_u32(bytes, cursor + 42)? as usize;
        let name_bytes = bytes.get(cursor + 46..cursor + 46 + name_len)?;
        let entry_name = String::from_utf8_lossy(name_bytes).into_owned();
        if entry_name.eq_ignore_ascii_case(name)
            || entry_name
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|leaf| leaf.eq_ignore_ascii_case(name))
        {
            return Some(ZipEntry {
                name: entry_name,
                method,
                compressed_size,
                uncompressed_size,
                local_header_offset,
            });
        }
        cursor += 46 + name_len + extra_len + comment_len;
    }
    None
}

/// Copies a stored entry out of the archive. Deflated entries are refused, which is why CI
/// packages the bundle with `Compress-Archive -CompressionLevel NoCompression`.
pub fn inflate_stored_or_deflate(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, PlatformError> {
    if !entry.is_stored() {
        return Err(PlatformError::OsUnavailable);
    }
    let header = entry.local_header_offset;
    let truncated = || PlatformError::OsUnavailable;
    if read_u32(bytes, header).ok_or_else(truncated)? != LOCAL_HEADER {
        return Err(truncated());
    }
    let name_len = read_u16(bytes, header + 26).ok_or_else(truncated)? as usize;
    let extra_len = read_u16(bytes, header + 28).ok_or_else(truncated)? as usize;
    let start = header
        .checked_add(30 + name_len + extra_len)
        .ok_or(PlatformError::OsUnavailable)?;
    let end = start
        .checked_add(entry.compressed_size)
        .ok_or(PlatformError::OsUnavailable)?;
    let slice = bytes.get(start..end).ok_or(PlatformError::OsUnavailable)?;
    let mut out = Vec::with_capacity(slice.len());
    out.extend_from_slice(slice);
    if entry.uncompressed_size != 0 && out.len() != entry.uncompressed_size {
        return Err(PlatformError::OsUnavailable);
    }
    Ok(out)
}

/// Reads the whole file into memory with a hard ceiling, for the download path.
pub fn read_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>, PlatformError> {
    let mut file = std::fs::File::open(path)?;
    let mut out = Vec::new();
    file.by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut out)
        .map_err(PlatformError::Io)?;
    if out.len() as u64 > max_bytes {
        return Err(PlatformError::OsUnavailable);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-entry stored zip in memory, the same shape `Compress-Archive` writes with
    /// no compression.
    fn stored_zip(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let crc = 0u32;
        // local file header
        out.extend_from_slice(&LOCAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        let central_offset = out.len();
        out.extend_from_slice(&CENTRAL_DIR.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());

        let central_size = out.len() - central_offset;
        out.extend_from_slice(&END_OF_CENTRAL_DIR.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(central_size as u32).to_le_bytes());
        out.extend_from_slice(&(central_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn a_stored_zip_entry_is_located_and_extracted() {
        let zip = stored_zip("postbox.exe", b"NEW-BUILD");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        assert!(entry.is_stored());
        assert_eq!(entry.uncompressed_size, 9);
        assert_eq!(
            inflate_stored_or_deflate(&zip, &entry).unwrap(),
            b"NEW-BUILD"
        );
    }

    #[test]
    fn a_nested_entry_name_still_matches() {
        let zip = stored_zip("postbox-windows-x64/postbox.exe", b"NEW");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("nested entry found");
        assert_eq!(entry.name, "postbox-windows-x64/postbox.exe");
        assert_eq!(inflate_stored_or_deflate(&zip, &entry).unwrap(), b"NEW");
    }

    #[test]
    fn a_deflated_entry_is_refused_rather_than_misread() {
        let mut zip = stored_zip("postbox.exe", b"NEW");
        // Flip the compression method in both the local and central headers to 8 (deflate).
        zip[8] = 8;
        let central = zip.len() - 22 - 46 - "postbox.exe".len();
        zip[central + 10] = 8;
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        assert!(!entry.is_stored());
        assert!(inflate_stored_or_deflate(&zip, &entry).is_err());
    }

    #[test]
    fn a_zip_without_the_executable_reports_none() {
        let zip = stored_zip("readme.txt", b"hello");
        assert!(find_zip_entry(&zip, "postbox.exe").is_none());
        assert!(find_zip_entry(b"not a zip at all", "postbox.exe").is_none());
    }
}
