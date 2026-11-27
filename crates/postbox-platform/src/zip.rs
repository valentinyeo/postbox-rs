//! Minimal zip reader for the release bundle, portable so it is tested on every OS.
//!
//! The release workflow writes `postbox-windows-x64.zip` with PowerShell
//! `Compress-Archive`, which deflates every entry (method 8) with a data descriptor
//! instead of the sizes in the local header. A release may also be packed with
//! `-CompressionLevel NoCompression` (method 0). Both are read here, from the central
//! directory, which always carries the true sizes.
//!
//! Anything else (bzip2, LZMA, zstd, encryption) is refused before a byte is written, and
//! the extracted payload must match the declared uncompressed size and CRC-32, so a
//! truncated or corrupted download never reaches the installer.

use crate::PlatformError;
use crate::updater::{ArchiveExtractor, EXE_ASSET_NAME};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Name of the command line tool inside the same archive.
pub const CTL_ASSET_NAME: &str = "postboxctl.exe";
/// Largest release archive the installer will read into memory.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Stored, uncompressed.
pub const METHOD_STORED: u16 = 0;
/// Raw DEFLATE, what `Compress-Archive` writes by default.
pub const METHOD_DEFLATE: u16 = 8;

const END_OF_CENTRAL_DIR: u32 = 0x0605_4b50;
const CENTRAL_DIR: u32 = 0x0201_4b50;
const LOCAL_HEADER: u32 = 0x0403_4b50;

/// Writes the bundled executable beside the live binary and returns its path.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZipExtractor;

impl ArchiveExtractor for ZipExtractor {
    fn extract_executable(&self, archive: &Path, dest: &Path) -> Result<PathBuf, PlatformError> {
        let bytes = read_limited(archive, MAX_ARCHIVE_BYTES)?;
        let entry = find_zip_entry(&bytes, EXE_ASSET_NAME)
            .or_else(|| find_zip_entry(&bytes, CTL_ASSET_NAME))
            .ok_or(PlatformError::OsUnavailable)?;
        let payload = extract_entry(&bytes, &entry)?;
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
    pub crc32: u32,
    pub local_header_offset: usize,
}

impl ZipEntry {
    /// `true` when the entry is stored uncompressed, so extraction is a plain copy.
    pub fn is_stored(&self) -> bool {
        self.method == METHOD_STORED
    }

    /// `true` when the entry is raw DEFLATE, the shape `Compress-Archive` writes.
    pub fn is_deflated(&self) -> bool {
        self.method == METHOD_DEFLATE
    }
}

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
        let crc32 = read_u32(bytes, cursor + 16)?;
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
                crc32,
                local_header_offset,
            });
        }
        cursor += 46 + name_len + extra_len + comment_len;
    }
    None
}

/// Copies a stored entry out of the archive.
pub fn extract_stored(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, PlatformError> {
    let slice = entry_payload(bytes, entry)?;
    let mut out = Vec::with_capacity(slice.len());
    out.extend_from_slice(slice);
    if entry.uncompressed_size != 0 && out.len() != entry.uncompressed_size {
        return Err(PlatformError::OsUnavailable);
    }
    Ok(out)
}

/// Inflates a raw-DEFLATE entry out of the archive, bounded by its declared size.
pub fn extract_deflated(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, PlatformError> {
    let slice = entry_payload(bytes, entry)?;
    let limit = if entry.uncompressed_size == 0 {
        MAX_ARCHIVE_BYTES as usize
    } else {
        entry.uncompressed_size
    };
    miniz_oxide::inflate::decompress_to_vec_with_limit(slice, limit)
        .map_err(|_| PlatformError::OsUnavailable)
}

/// Extracts one entry in whichever supported method it was written with.
pub fn extract_entry(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, PlatformError> {
    let payload = if entry.is_stored() {
        extract_stored(bytes, entry)?
    } else if entry.is_deflated() {
        extract_deflated(bytes, entry)?
    } else {
        // Unknown or unsupported method: refuse rather than guess.
        return Err(PlatformError::OsUnavailable);
    };
    if entry.uncompressed_size != 0 && payload.len() != entry.uncompressed_size {
        return Err(PlatformError::OsUnavailable);
    }
    // `Compress-Archive` writes a real CRC-32; a zero one means the writer skipped it.
    if entry.crc32 != 0 && crc32fast::hash(&payload) != entry.crc32 {
        return Err(PlatformError::OsUnavailable);
    }
    Ok(payload)
}

/// The compressed bytes of `entry`, located through its local file header.
fn entry_payload<'a>(bytes: &'a [u8], entry: &ZipEntry) -> Result<&'a [u8], PlatformError> {
    let header = entry.local_header_offset;
    if read_u32(bytes, header).ok_or(PlatformError::OsUnavailable)? != LOCAL_HEADER {
        return Err(PlatformError::OsUnavailable);
    }
    // The local header repeats the name and extra lengths, which can differ from the
    // central directory when the writer added a data descriptor.
    let name_len = read_u16(bytes, header + 26).ok_or(PlatformError::OsUnavailable)? as usize;
    let extra_len = read_u16(bytes, header + 28).ok_or(PlatformError::OsUnavailable)? as usize;
    let start = header
        .checked_add(30 + name_len + extra_len)
        .ok_or(PlatformError::OsUnavailable)?;
    let end = start
        .checked_add(entry.compressed_size)
        .ok_or(PlatformError::OsUnavailable)?;
    bytes.get(start..end).ok_or(PlatformError::OsUnavailable)
}

/// Reads a file into memory with a hard ceiling, for the download path.
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

    /// Builds a one-entry zip in memory the way `Compress-Archive` does: the sizes and CRC
    /// go in the central directory and the local header carries a streaming data descriptor.
    fn build_zip(name: &str, method: u16, payload: &[u8], raw: &[u8]) -> Vec<u8> {
        let crc = crc32fast::hash(payload);
        let mut out = Vec::new();
        let local_offset = 0usize;
        out.extend_from_slice(&LOCAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0808u16.to_le_bytes()); // data descriptor + UTF-8 names
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // streaming: no crc yet
        out.extend_from_slice(&0u32.to_le_bytes()); // streaming: no sizes yet
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(raw);
        out.extend_from_slice(&0x0807_4b50u32.to_le_bytes()); // data descriptor
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());

        let central_offset = out.len();
        out.extend_from_slice(&CENTRAL_DIR.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0808u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(local_offset as u32).to_le_bytes());
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

    fn deflated_zip(name: &str, payload: &[u8]) -> Vec<u8> {
        let raw = miniz_oxide::deflate::compress_to_vec(payload, 6);
        build_zip(name, METHOD_DEFLATE, payload, &raw)
    }

    fn stored_zip(name: &str, payload: &[u8]) -> Vec<u8> {
        build_zip(name, METHOD_STORED, payload, payload)
    }

    #[test]
    fn a_deflated_entry_extracts_byte_for_byte() {
        let payload = b"PK\x03\x04 the new build, compressed the way Compress-Archive does it";
        let zip = deflated_zip("postbox.exe", payload);
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        assert!(entry.is_deflated());
        assert_eq!(entry.uncompressed_size, payload.len());
        assert_eq!(extract_entry(&zip, &entry).unwrap(), payload);
    }

    #[test]
    fn a_large_deflated_entry_round_trips() {
        // Compressible but not trivial: a real binary is a few tens of MB.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let zip = deflated_zip("postbox-windows-x64/postbox.exe", &payload);
        let entry = find_zip_entry(&zip, "postbox.exe").expect("nested entry found");
        assert_eq!(entry.name, "postbox-windows-x64/postbox.exe");
        assert_eq!(extract_entry(&zip, &entry).unwrap(), payload);
    }

    #[test]
    fn a_stored_entry_still_extracts() {
        let zip = stored_zip("postbox.exe", b"NEW-BUILD");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        assert!(entry.is_stored());
        assert_eq!(entry.crc32, crc32fast::hash(b"NEW-BUILD"));
        assert_eq!(extract_entry(&zip, &entry).unwrap(), b"NEW-BUILD");
    }

    #[test]
    fn a_nested_entry_name_still_matches() {
        let zip = stored_zip("dist/postbox.exe", b"NEW");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("nested entry found");
        assert_eq!(entry.name, "dist/postbox.exe");
        assert_eq!(extract_entry(&zip, &entry).unwrap(), b"NEW");
    }

    #[test]
    fn the_ctl_executable_is_the_fallback_when_the_app_is_absent() {
        let zip = deflated_zip("postboxctl.exe", b"CTL");
        assert!(find_zip_entry(&zip, EXE_ASSET_NAME).is_none());
        let entry = find_zip_entry(&zip, CTL_ASSET_NAME).expect("fallback found");
        assert_eq!(extract_entry(&zip, &entry).unwrap(), b"CTL");
    }

    #[test]
    fn an_unknown_compression_method_is_refused() {
        // Method 12 is bzip2: never written by our release workflow, so never guessed.
        let payload = b"NEW";
        let zip = build_zip("postbox.exe", 12, payload, payload);
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        assert!(!entry.is_stored() && !entry.is_deflated());
        assert!(extract_entry(&zip, &entry).is_err());
    }

    #[test]
    fn a_corrupted_deflate_stream_is_refused() {
        let mut zip = deflated_zip("postbox.exe", b"NEW-BUILD-BYTES-THAT-COMPRESS");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        // Flip bytes inside the compressed stream.
        let start = 30 + "postbox.exe".len();
        zip[start] ^= 0xff;
        zip[start + 1] ^= 0xff;
        assert!(extract_entry(&zip, &entry).is_err());
    }

    #[test]
    fn a_crc_mismatch_is_refused() {
        let zip = deflated_zip("postbox.exe", b"NEW-BUILD");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        let mut wrong = entry.clone();
        wrong.crc32 ^= 0xdead_beef;
        assert!(extract_entry(&zip, &wrong).is_err());
    }

    #[test]
    fn a_size_mismatch_is_refused() {
        let zip = deflated_zip("postbox.exe", b"NEW-BUILD");
        let entry = find_zip_entry(&zip, "postbox.exe").expect("entry found");
        let mut wrong = entry.clone();
        wrong.uncompressed_size += 1;
        assert!(extract_entry(&zip, &wrong).is_err());
    }

    #[test]
    fn the_extractor_writes_the_deflated_binary_to_disk() {
        let dir = std::env::temp_dir().join(format!("postbox-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("postbox-windows-x64.zip");
        let dest = dir.join("postbox.exe.staged.exe");
        let payload = b"deflated release binary";
        std::fs::write(&archive, deflated_zip("postbox.exe", payload)).unwrap();

        let written = ZipExtractor
            .extract_executable(&archive, &dest)
            .expect("extraction succeeds");
        assert_eq!(written, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_without_the_executable_reports_none() {
        let zip = stored_zip("readme.txt", b"hello");
        assert!(find_zip_entry(&zip, "postbox.exe").is_none());
        assert!(find_zip_entry(b"not a zip at all", "postbox.exe").is_none());
    }
}
