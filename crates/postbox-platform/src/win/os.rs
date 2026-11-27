//! `postbox://` protocol registration, App User Model ID registration and the
//! single-instance mutex.
//!
//! All `HKCU` writes go through `RegCreateKeyExW` + `RegSetValueExW`, so no elevation is ever
//! needed and an account without admin rights can register the handler.

use crate::PlatformError;
use crate::os_integration::{LinkPipe, SingleInstance, UrlHandler};
use crate::win::ipc;
use postbox_core::os_integration::{
    APP_USER_MODEL_SUBKEY, URL_PROTOCOL_COMMAND_SUBKEY, app_user_model_values, url_protocol_values,
};
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PCWSTR;

fn wide(value: &str) -> Vec<u16> {
    OsString::from(value).encode_wide().chain([0]).collect()
}

struct KeyGuard(HKEY);

impl Drop for KeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the key was opened by `RegCreateKeyExW` and is closed exactly once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

fn open_key(subkey: &str) -> Result<KeyGuard, PlatformError> {
    let wide_subkey = wide(subkey);
    let mut hkey = HKEY::default();
    // SAFETY: `wide_subkey` is NUL-terminated UTF-16 and outlives the call; the guard closes
    // the returned key.
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
    if status.is_err() {
        return Err(PlatformError::RegistryError(format!(
            "RegCreateKeyExW({subkey}) failed"
        )));
    }
    Ok(KeyGuard(hkey))
}

fn write_value(subkey: &str, name: Option<&str>, value: &str) -> Result<(), PlatformError> {
    let key = open_key(subkey)?;
    let wide_name = name.map(wide);
    let wide_value = wide(value);
    let bytes: Vec<u8> = wide_value
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let name_ptr = wide_name
        .as_ref()
        .map(|n| PCWSTR(n.as_ptr()))
        .unwrap_or(PCWSTR::null());
    // SAFETY: `key` is open with `KEY_SET_VALUE`; `bytes` is a NUL-terminated UTF-16LE buffer
    // of exactly the length passed, matching `REG_SZ`.
    let status = unsafe { RegSetValueExW(key.0, name_ptr, None, REG_SZ, Some(&bytes)) };
    if status.is_err() {
        return Err(PlatformError::RegistryError(format!(
            "RegSetValueExW({subkey}\\{}) failed",
            name.unwrap_or("(default)")
        )));
    }
    Ok(())
}

/// Writes `HKCU\Software\Classes\postbox` for the current user.
#[derive(Debug, Default)]
pub struct WindowsUrlHandler;

impl WindowsUrlHandler {
    pub fn new() -> Self {
        Self
    }
}

impl UrlHandler for WindowsUrlHandler {
    fn register(&self, exe_path: &str) -> Result<bool, PlatformError> {
        // App User Model ID first: without it an unpackaged toast never appears.
        for value in app_user_model_values() {
            write_value(&value.subkey, value.name.as_deref(), &value.value)?;
        }
        let values = url_protocol_values(exe_path);
        let already = matches!(
            crate::win::registry::reg_get_string(URL_PROTOCOL_COMMAND_SUBKEY, "")?,
            Some(stored) if stored.trim() == values[3].value.trim()
        );
        if already {
            return Ok(false);
        }
        for value in &values {
            write_value(&value.subkey, value.name.as_deref(), &value.value)?;
        }
        Ok(true)
    }

    fn registered_command(&self) -> Result<Option<String>, PlatformError> {
        crate::win::registry::reg_get_string(URL_PROTOCOL_COMMAND_SUBKEY, "")
    }

    fn unregister(&self) -> Result<(), PlatformError> {
        crate::win::registry::reg_delete_tree(postbox_core::os_integration::URL_PROTOCOL_SUBKEY)?;
        crate::win::registry::reg_delete_tree(APP_USER_MODEL_SUBKEY)
    }
}

/// Creates `Local\postbox-single-<user>` and keeps it open for the life of the process.
#[derive(Debug, Default)]
pub struct WindowsSingleInstance {
    held: std::sync::Mutex<Option<usize>>,
}

impl WindowsSingleInstance {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SingleInstance for WindowsSingleInstance {
    fn try_acquire(&self, user: &str) -> Result<bool, PlatformError> {
        let mut guard = self.held.lock().map_err(|_| PlatformError::OsUnavailable)?;
        if guard.is_some() {
            return Ok(true);
        }
        let name = wide(&postbox_core::os_integration::single_instance_mutex_name(
            user,
        ));
        // SAFETY: `name` is NUL-terminated UTF-16 and outlives the call. The returned handle is
        // kept alive in `guard` until the process exits.
        let handle: HANDLE = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .map_err(|_| PlatformError::OsUnavailable)?;
        if handle.is_invalid() {
            return Err(PlatformError::OsUnavailable);
        }
        // SAFETY: reading the calling thread's last-error value after `CreateMutexW`.
        let already = unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
        if already {
            // SAFETY: the handle is ours and this process is not the owner.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(false);
        }
        *guard = Some(handle.0 as usize);
        Ok(true)
    }
}

impl Drop for WindowsSingleInstance {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.held.lock()
            && let Some(raw) = guard.take()
        {
            // SAFETY: the handle was created by `CreateMutexW` in this struct and is closed
            // exactly once, here.
            unsafe {
                let _ = CloseHandle(HANDLE(raw as *mut _));
            }
        }
    }
}

/// Sends one line to `\\.\pipe\postbox-link-<user>`.
#[derive(Debug, Default)]
pub struct WindowsLinkPipe;

impl WindowsLinkPipe {
    pub fn new() -> Self {
        Self
    }
}

impl LinkPipe for WindowsLinkPipe {
    fn forward(&self, user: &str, payload: &str) -> Result<bool, PlatformError> {
        let name = postbox_core::os_integration::normalize_user(user);
        match ipc::pipe_round_trip(&name_for_link(&name), payload, Duration::from_millis(400)) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

fn name_for_link(user: &str) -> String {
    let full = postbox_core::os_integration::link_pipe_name(user);
    full.rsplit('\\').next().unwrap_or(&full).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::os_integration::{
        handler_command_line, link_pipe_name, normalize_user, url_protocol_values,
    };
    use std::path::PathBuf;

    #[test]
    fn forwarding_names_the_same_pipe_the_window_serves() {
        // A second launch forwards to the link pipe the running window listens on. Both
        // sides now tolerate the `\\.\pipe\` prefix, so neither can name a pipe twice.
        let user = "valentin";
        let served = name_for_link(user);
        assert_eq!(served, "postbox-link-valentin");
        assert_eq!(
            ipc::pipe_path(&served),
            PathBuf::from(r"\\.\pipe\postbox-link-valentin")
        );
        assert_eq!(
            ipc::pipe_path(&link_pipe_name(user)),
            ipc::pipe_path(&served)
        );
    }

    #[test]
    fn the_link_pipe_name_strips_to_the_kernel_name() {
        assert_eq!(name_for_link("valentin"), "postbox-link-valentin");
        assert_eq!(
            link_pipe_name(&normalize_user("Valentin Yeo")),
            r"\\.\pipe\postbox-link-Valentin-Yeo"
        );
        assert_eq!(name_for_link("Valentin-Yeo"), "postbox-link-Valentin-Yeo");
    }

    #[test]
    fn the_registration_values_match_the_contract() {
        let exe = r"C:\Program Files\Postbox\postbox.exe";
        let values = url_protocol_values(exe);
        assert_eq!(values[3].value, handler_command_line(exe));
        assert_eq!(values[1].name.as_deref(), Some("URL Protocol"));
    }
}
