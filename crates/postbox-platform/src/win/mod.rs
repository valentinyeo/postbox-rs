//! Windows-native platform implementations strictly quarantined under `#[cfg(windows)]`.

pub mod dpapi;
pub mod registry;

use crate::{EnvRegistry, Notifier, PlatformError, SecretLoad, SecretLoadOutcome, SecretStore};
use postbox_core::ChatKey;

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
