//! Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`) wrapper.
//!
//! Uses exact Zig Postbox parameters (`dwFlags = 0`, no entropy, no description) so
//! `HKCU\Software\Messages\HubToken` written by Zig Postbox decrypts seamlessly in `postbox-rs`.

use crate::PlatformError;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

struct LocalFreeGuard(*mut u8);

impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by Windows DPAPI (`CryptProtectData` or
            // `CryptUnprotectData`) and is freed exactly once on drop via `LocalFree`.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.cast())));
            }
        }
    }
}

/// Encrypts `plain` bytes for the current Windows user with DPAPI (`dwFlags = 0`).
pub fn protect_bytes(plain: &[u8]) -> Result<Vec<u8>, PlatformError> {
    if plain.is_empty() || plain.len() > u32::MAX as usize {
        return Err(PlatformError::InvalidSecret);
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr().cast_mut(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `in_blob` points to valid `plain` memory for the duration of the FFI call;
    // `out_blob` receives a Windows-allocated buffer freed by `LocalFreeGuard`.
    unsafe {
        CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, 0, &mut out_blob)
            .map_err(|e| PlatformError::DpapiError(e.to_string()))?;
    }
    let _guard = LocalFreeGuard(out_blob.pbData);
    if out_blob.pbData.is_null() || out_blob.cbData == 0 {
        return Err(PlatformError::DpapiError(
            "CryptProtectData returned empty blob".to_owned(),
        ));
    }
    // SAFETY: `CryptProtectData` succeeded and initialized `cbData` bytes at `pbData`.
    let bytes = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) };
    Ok(bytes.to_vec())
}

/// Decrypts a DPAPI blob for the current Windows user (`dwFlags = 0`).
pub fn unprotect_bytes(blob: &[u8]) -> Result<Vec<u8>, PlatformError> {
    if blob.is_empty() || blob.len() > u32::MAX as usize {
        return Err(PlatformError::InvalidSecret);
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr().cast_mut(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `in_blob` points to valid `blob` memory for the duration of the FFI call;
    // `out_blob` receives a Windows-allocated buffer freed by `LocalFreeGuard`.
    unsafe {
        CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
            .map_err(|e| PlatformError::DpapiError(e.to_string()))?;
    }
    let _guard = LocalFreeGuard(out_blob.pbData);
    if out_blob.pbData.is_null() {
        return Err(PlatformError::DpapiError(
            "CryptUnprotectData returned null buffer".to_owned(),
        ));
    }
    // SAFETY: `CryptUnprotectData` succeeded and initialized `cbData` bytes at `pbData`.
    let bytes = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) };
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_crypt_protect_unprotect_round_trip() {
        let secret = b"postbox-dpapi-roundtrip-token-1234";
        let protected = protect_bytes(secret).expect("CryptProtectData should succeed");
        assert_ne!(protected.as_slice(), secret);
        let recovered = unprotect_bytes(&protected).expect("CryptUnprotectData should succeed");
        assert_eq!(recovered.as_slice(), secret);
    }
}
