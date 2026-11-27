//! Windows Registry helpers for `HKCU\Environment` and `HKCU\Software\Messages`.
//!
//! Mirrors `winhttp.zig:58-167`:
//! - `HKCU\Environment`: reads `REG_SZ` and `REG_EXPAND_SZ` with automatic `%VAR%` expansion via `RegGetValueW`.
//! - `HKCU\Software\Messages`: stores DPAPI-encrypted `REG_BINARY` values and safely migrates
//!   legacy plaintext `REG_SZ` values (failing closed on `MigrationFailed`).

use crate::win::dpapi::{protect_bytes, unprotect_bytes};
use crate::{PlatformError, SecretLoad, SecretLoadOutcome};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

const MESSAGES_SUBKEY: &str = "Software\\Messages";
const ENVIRONMENT_SUBKEY: &str = "Environment";
const MAX_REG_BYTES: u32 = 64 * 1024;

struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was opened by `RegCreateKeyExW` and is closed exactly once on drop.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn decode_utf16_bytes(bytes: &[u8]) -> Result<String, PlatformError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(PlatformError::RegistryError(
            "odd UTF-16 byte length".to_owned(),
        ));
    }
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for &chunk in bytes.as_chunks::<2>().0 {
        let code = u16::from_le_bytes(chunk);
        if code == 0 {
            break;
        }
        units.push(code);
    }
    String::from_utf16(&units).map_err(|_| PlatformError::RegistryError("invalid UTF-16".into()))
}

/// Reads `HKCU\Environment\<name>` (`REG_SZ` or `REG_EXPAND_SZ`, automatically expanded by `RegGetValueW`).
pub fn load_user_environment(name: &str) -> Result<Option<String>, PlatformError> {
    let subkey = to_wide_null(ENVIRONMENT_SUBKEY);
    let value_name = to_wide_null(name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;

    let mut size: u32 = 0;
    // SAFETY: Querying required buffer size for an immutable NUL-terminated wide string key/value.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            flags,
            None,
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW size query failed: {}",
            status.0
        )));
    }
    if size == 0 || size > MAX_REG_BYTES || !size.is_multiple_of(2) {
        return Ok(None);
    }

    let mut buf = vec![0u8; size as usize];
    let mut actual = size;
    // SAFETY: `buf` has capacity `actual` bytes for `RegGetValueW` to populate.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut actual),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW read failed: {}",
            status.0
        )));
    }
    buf.truncate((actual as usize).min(buf.len()));
    let text = decode_utf16_bytes(&buf)?;
    let trimmed = text.trim_ascii();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

pub fn reg_get_binary(subkey: &str, name: &str) -> Result<Option<Vec<u8>>, PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut size: u32 = 0;

    // SAFETY: Querying size for a `REG_BINARY` value under `HKEY_CURRENT_USER`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            PCWSTR(wide_name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW binary size query failed: {}",
            status.0
        )));
    }
    if size == 0 || size > MAX_REG_BYTES {
        return Ok(None);
    }

    let mut buf = vec![0u8; size as usize];
    let mut actual = size;
    // SAFETY: `buf` is valid for `actual` bytes written by `RegGetValueW`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            PCWSTR(wide_name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut actual),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW binary read failed: {}",
            status.0
        )));
    }
    buf.truncate((actual as usize).min(buf.len()));
    Ok(Some(buf))
}

pub fn reg_get_string(subkey: &str, name: &str) -> Result<Option<String>, PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut size: u32 = 0;

    // SAFETY: Querying size for a `REG_SZ` value under `HKEY_CURRENT_USER`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            PCWSTR(wide_name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW string size query failed: {}",
            status.0
        )));
    }
    if size == 0 || size > MAX_REG_BYTES || !size.is_multiple_of(2) {
        return Ok(None);
    }

    let mut buf = vec![0u8; size as usize];
    let mut actual = size;
    // SAFETY: `buf` is valid for `actual` bytes written by `RegGetValueW`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            PCWSTR(wide_name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut actual),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW string read failed: {}",
            status.0
        )));
    }
    buf.truncate((actual as usize).min(buf.len()));
    let text = decode_utf16_bytes(&buf)?;
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

pub fn reg_set_binary(subkey: &str, name: &str, data: &[u8]) -> Result<(), PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut hkey = HKEY::default();

    // SAFETY: Creating/opening `HKCU\<subkey>` with `KEY_SET_VALUE`; `RegKeyGuard` closes `hkey`.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegCreateKeyExW failed: {}",
            status.0
        )));
    }
    let _guard = RegKeyGuard(hkey);

    // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `data` is a valid byte slice.
    let write_status = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            REG_BINARY,
            Some(data),
        )
    };
    if write_status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegSetValueExW failed: {}",
            write_status.0
        )));
    }
    Ok(())
}

pub fn reg_get_dword(subkey: &str, name: &str) -> Result<Option<u32>, PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut buf = [0u8; 4];
    let mut size: u32 = 4;

    // SAFETY: `buf` is a 4-byte stack buffer matching `REG_DWORD` size (`size = 4`).
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            PCWSTR(wide_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_UNSUPPORTED_TYPE {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegGetValueW dword read failed: {}",
            status.0
        )));
    }
    if size != 4 {
        return Ok(None);
    }
    Ok(Some(u32::from_le_bytes(buf)))
}

pub fn reg_set_dword(subkey: &str, name: &str, value: u32) -> Result<(), PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut hkey = HKEY::default();

    // SAFETY: Creating/opening `HKCU\<subkey>` with `KEY_SET_VALUE`; `RegKeyGuard` closes `hkey`.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegCreateKeyExW failed: {}",
            status.0
        )));
    }
    let _guard = RegKeyGuard(hkey);

    let data = value.to_le_bytes();
    // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `data` is a 4-byte little-endian DWORD.
    let write_status = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            REG_DWORD,
            Some(&data),
        )
    };
    if write_status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegSetValueExW dword failed: {}",
            write_status.0
        )));
    }
    Ok(())
}

/// Writes a NUL-terminated UTF-16LE `REG_SZ` value under `HKCU\<subkey>\<name>`.
/// Explicitly permits empty string `""` (e.g., clearing `Microphone` to System default).
pub fn reg_set_string(subkey: &str, name: &str, value: &str) -> Result<(), PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let wide_val = to_wide_null(value);
    let bytes: Vec<u8> = wide_val.iter().flat_map(|u| u.to_le_bytes()).collect();
    let mut hkey = HKEY::default();

    // SAFETY: Creating/opening `HKCU\<subkey>` with `KEY_SET_VALUE`; `RegKeyGuard` closes `hkey`.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegCreateKeyExW failed: {}",
            status.0
        )));
    }
    let _guard = RegKeyGuard(hkey);

    // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `bytes` is a valid NUL-terminated UTF-16LE slice.
    let write_status =
        unsafe { RegSetValueExW(hkey, PCWSTR(wide_name.as_ptr()), None, REG_SZ, Some(&bytes)) };
    if write_status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegSetValueExW string failed: {}",
            write_status.0
        )));
    }
    Ok(())
}

pub fn load_messages_dword(name: &str) -> Result<Option<u32>, PlatformError> {
    reg_get_dword(MESSAGES_SUBKEY, name)
}

pub fn save_messages_dword(name: &str, value: u32) -> Result<(), PlatformError> {
    reg_set_dword(MESSAGES_SUBKEY, name, value)
}

pub fn load_messages_string(name: &str) -> Result<Option<String>, PlatformError> {
    reg_get_string(MESSAGES_SUBKEY, name)
}

pub fn save_messages_string(name: &str, value: &str) -> Result<(), PlatformError> {
    reg_set_string(MESSAGES_SUBKEY, name, value)
}

pub fn reg_delete_value(subkey: &str, name: &str) -> Result<(), PlatformError> {
    let wide_subkey = to_wide_null(subkey);
    let wide_name = to_wide_null(name);
    let mut hkey = HKEY::default();

    // SAFETY: Opening `HKCU\<subkey>` with `KEY_SET_VALUE`; `RegKeyGuard` closes `hkey`.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(PlatformError::RegistryError(format!(
            "RegCreateKeyExW failed: {}",
            status.0
        )));
    }
    let _guard = RegKeyGuard(hkey);

    // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `wide_name` is NUL-terminated.
    let del_status = unsafe { RegDeleteValueW(hkey, PCWSTR(wide_name.as_ptr())) };
    if del_status != ERROR_SUCCESS && del_status != ERROR_FILE_NOT_FOUND {
        return Err(PlatformError::RegistryError(format!(
            "RegDeleteValueW failed: {}",
            del_status.0
        )));
    }
    Ok(())
}

/// Encrypts `value` with DPAPI and stores it as `REG_BINARY` at `HKCU\Software\Messages\<name>`.
pub fn save_dpapi_secret(name: &str, value: &str) -> Result<(), PlatformError> {
    let trimmed = value.trim_ascii();
    if trimmed.is_empty() {
        return Err(PlatformError::InvalidSecret);
    }
    let blob = protect_bytes(trimmed.as_bytes())?;
    reg_set_binary(MESSAGES_SUBKEY, name, &blob)
}

/// Loads `HKCU\Software\Messages\<name>`, trying DPAPI `REG_BINARY` first, then migrating
/// any legacy plaintext `REG_SZ` with round-trip verification (failing closed on error).
pub fn load_dpapi_secret_with_outcome(name: &str) -> Result<Option<SecretLoad>, PlatformError> {
    if let Some(blob) = reg_get_binary(MESSAGES_SUBKEY, name)? {
        let plain_bytes = unprotect_bytes(&blob)?;
        let value = String::from_utf8(plain_bytes)
            .map_err(|_| PlatformError::DpapiError("decrypted secret is not valid UTF-8".into()))?;
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(SecretLoad {
            value,
            outcome: SecretLoadOutcome::Encrypted,
        }));
    }

    let Some(legacy_plain) = reg_get_string(MESSAGES_SUBKEY, name)? else {
        return Ok(None);
    };
    if legacy_plain.is_empty() {
        return Ok(None);
    }

    // Attempt legacy plaintext -> DPAPI REG_BINARY migration with round-trip verification.
    let migration = (|| -> Result<(), PlatformError> {
        let blob = protect_bytes(legacy_plain.as_bytes())?;
        let verified = unprotect_bytes(&blob)?;
        if verified.as_slice() != legacy_plain.as_bytes() {
            return Err(PlatformError::DpapiError(
                "DPAPI round-trip verification mismatch".into(),
            ));
        }
        reg_set_binary(MESSAGES_SUBKEY, name, &blob)
    })();

    match migration {
        Ok(()) => Ok(Some(SecretLoad {
            value: legacy_plain,
            outcome: SecretLoadOutcome::Migrated,
        })),
        Err(_) => Ok(Some(SecretLoad {
            value: String::new(),
            outcome: SecretLoadOutcome::MigrationFailed,
        })),
    }
}

pub fn delete_dpapi_secret(name: &str) -> Result<(), PlatformError> {
    reg_delete_value(MESSAGES_SUBKEY, name)
}
