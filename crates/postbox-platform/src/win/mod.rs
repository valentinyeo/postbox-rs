//! Windows-native platform implementations strictly quarantined under `#[cfg(windows)]`.

pub mod clipboard;
pub mod dpapi;
pub mod registry;
pub mod shell;
pub mod wasapi;

use crate::audio::{AudioBackend, AudioCaptureStream, AudioRenderStream};
use crate::clipboard::{ClipboardBackend, ClipboardPayload, ZoneMarker, mark_downloaded_file_zone};
use crate::{
    EnvRegistry, Notifier, OsRegistry, PlatformError, SecretLoad, SecretLoadOutcome, SecretStore,
};
use postbox_core::ChatKey;
use postbox_core::voice::AudioDeviceInfo;
use std::path::Path;

#[derive(Debug, Default)]
pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        Self
    }
}

impl SecretStore for WindowsPlatform {
    fn load_secret(&self, name: &str) -> Result<Option<String>, PlatformError> {
        Ok(self.load_secret_with_outcome(name)?.and_then(|loaded| {
            if loaded.outcome == SecretLoadOutcome::MigrationFailed {
                None
            } else {
                Some(loaded.value)
            }
        }))
    }

    fn save_secret(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        registry::save_dpapi_secret(name, value)
    }

    fn load_secret_with_outcome(&self, name: &str) -> Result<Option<SecretLoad>, PlatformError> {
        registry::load_dpapi_secret_with_outcome(name)
    }

    fn delete_secret(&self, name: &str) -> Result<(), PlatformError> {
        registry::delete_dpapi_secret(name)
    }
}

impl EnvRegistry for WindowsPlatform {
    fn load_user_env(&self, name: &str) -> Result<Option<String>, PlatformError> {
        registry::load_user_environment(name)
    }
}

impl OsRegistry for WindowsPlatform {
    fn load_dword(&self, name: &str) -> Result<Option<u32>, PlatformError> {
        registry::load_messages_dword(name)
    }

    fn save_dword(&self, name: &str, value: u32) -> Result<(), PlatformError> {
        registry::save_messages_dword(name, value)
    }

    fn load_string(&self, name: &str) -> Result<Option<String>, PlatformError> {
        registry::load_messages_string(name)
    }

    fn save_string(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        registry::save_messages_string(name, value)
    }
}

impl Notifier for WindowsPlatform {
    fn notify_message(
        &self,
        _chat: &ChatKey,
        _title: &str,
        _body: &str,
    ) -> Result<(), PlatformError> {
        // WinRT Toast XML implementation lands in Milestone 7.
        Ok(())
    }
}

impl ClipboardBackend for WindowsPlatform {
    fn read_paste_payload(&self) -> Result<ClipboardPayload, PlatformError> {
        clipboard::read_clipboard_payload()
    }
}

impl ZoneMarker for WindowsPlatform {
    fn mark_downloaded_file(&self, path: &Path) -> Result<bool, PlatformError> {
        mark_downloaded_file_zone(path)
    }
}

impl AudioBackend for WindowsPlatform {
    fn list_capture_devices(&self) -> Result<Vec<AudioDeviceInfo>, PlatformError> {
        wasapi::list_capture_devices()
    }

    fn open_render(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioRenderStream>, PlatformError> {
        wasapi::open_render(sample_rate, channels)
    }

    fn open_capture(
        &self,
        preferred_mic_name: Option<&str>,
    ) -> Result<Box<dyn AudioCaptureStream>, PlatformError> {
        wasapi::open_capture(preferred_mic_name)
    }
}
