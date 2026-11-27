pub mod hub_auth;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;

use postbox_core::ChatKey;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

pub use hub_auth::{
    CredentialSource, DiscoveredHubConfig, ENV_POSTBOX_HUB_TOKEN, ENV_POSTBOX_HUB_URL,
    ENV_WAZIG_HUB_TOKEN, ENV_WAZIG_HUB_URL, HUB_TOKEN_SECRET_NAME, HubDiscoveryOutcome,
    discover_hub_credentials, discover_hub_credentials_from_system, valid_hub_token,
};

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("secret_not_found")]
    SecretNotFound,
    #[error("os_unavailable")]
    OsUnavailable,
    #[error("invalid_secret")]
    InvalidSecret,
    #[error("dpapi_error: {0}")]
    DpapiError(String),
    #[error("registry_error: {0}")]
    RegistryError(String),
    #[error("io_error")]
    Io(#[from] std::io::Error),
}

/// Outcome of loading a secret from persistent storage, distinguishing encrypted DPAPI
/// storage from legacy plaintext `REG_SZ` migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretLoadOutcome {
    /// Loaded and decrypted from DPAPI `REG_BINARY` storage.
    Encrypted,
    /// Loaded from legacy plaintext `REG_SZ`, verified round-trip, and migrated to `REG_BINARY`.
    Migrated,
    /// Found legacy plaintext `REG_SZ`, but DPAPI encryption/verification/write failed.
    /// Callers MUST fail closed and reject using the secret.
    MigrationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLoad {
    pub value: String,
    pub outcome: SecretLoadOutcome,
}

/// Encrypted per-user secret storage (Windows DPAPI + `HKCU\Software\Messages` on Windows).
pub trait SecretStore: Send + Sync {
    fn load_secret(&self, name: &str) -> Result<Option<String>, PlatformError>;
    fn save_secret(&self, name: &str, value: &str) -> Result<(), PlatformError>;

    fn load_secret_with_outcome(&self, name: &str) -> Result<Option<SecretLoad>, PlatformError> {
        Ok(self.load_secret(name)?.map(|value| SecretLoad {
            value,
            outcome: SecretLoadOutcome::Encrypted,
        }))
    }

    fn delete_secret(&self, _name: &str) -> Result<(), PlatformError> {
        Ok(())
    }
}

/// Reads persisted per-user environment variables (`HKCU\Environment` on Windows).
/// Per `ISSUE-517`, non-empty `HKCU\Environment` entries take precedence over inherited
/// process environment variables.
pub trait EnvRegistry: Send + Sync {
    fn load_user_env(&self, name: &str) -> Result<Option<String>, PlatformError>;
}

/// OS desktop notification sink (Windows Toast XML on Windows).
pub trait Notifier: Send + Sync {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError>;
}

/// Concrete default platform for the current OS target:
/// - On Windows: real DPAPI + `HKCU\Software\Messages` + `HKCU\Environment`.
/// - On non-Windows: portable in-memory store with empty `EnvRegistry`.
#[derive(Default)]
pub struct NativePlatform {
    #[cfg(windows)]
    inner: win::WindowsPlatform,
    #[cfg(not(windows))]
    inner: MemoryPlatform,
}

impl SecretStore for NativePlatform {
    fn load_secret(&self, name: &str) -> Result<Option<String>, PlatformError> {
        self.inner.load_secret(name)
    }

    fn save_secret(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        self.inner.save_secret(name, value)
    }

    fn load_secret_with_outcome(&self, name: &str) -> Result<Option<SecretLoad>, PlatformError> {
        self.inner.load_secret_with_outcome(name)
    }

    fn delete_secret(&self, name: &str) -> Result<(), PlatformError> {
        self.inner.delete_secret(name)
    }
}

impl EnvRegistry for NativePlatform {
    fn load_user_env(&self, name: &str) -> Result<Option<String>, PlatformError> {
        self.inner.load_user_env(name)
    }
}

impl Notifier for NativePlatform {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError> {
        self.inner.notify_message(chat, title, body)
    }
}

/// Portable in-memory mock platform used for fast Linux & headless unit tests.
#[derive(Default)]
pub struct MemoryPlatform {
    secrets: Mutex<HashMap<String, String>>,
    legacy_plaintext_secrets: Mutex<HashMap<String, String>>,
    user_env: Mutex<HashMap<String, String>>,
    notifications: Mutex<Vec<(ChatKey, String, String)>>,
    fail_migration: AtomicBool,
    fail_save: AtomicBool,
}

impl MemoryPlatform {
    pub fn notification_count(&self) -> usize {
        self.notifications.lock().map(|n| n.len()).unwrap_or(0)
    }

    pub fn set_user_env(&self, name: impl Into<String>, value: impl Into<String>) {
        if let Ok(mut guard) = self.user_env.lock() {
            guard.insert(name.into(), value.into());
        }
    }

    pub fn remove_user_env(&self, name: &str) {
        if let Ok(mut guard) = self.user_env.lock() {
            guard.remove(name);
        }
    }

    pub fn set_legacy_plaintext_secret(&self, name: impl Into<String>, value: impl Into<String>) {
        if let Ok(mut guard) = self.legacy_plaintext_secrets.lock() {
            guard.insert(name.into(), value.into());
        }
    }

    pub fn set_fail_migration(&self, fail: bool) {
        self.fail_migration.store(fail, Ordering::Relaxed);
    }

    pub fn set_fail_save(&self, fail: bool) {
        self.fail_save.store(fail, Ordering::Relaxed);
    }

    pub fn has_encrypted_secret(&self, name: &str) -> bool {
        self.secrets
            .lock()
            .map(|g| g.contains_key(name))
            .unwrap_or(false)
    }

    pub fn has_legacy_secret(&self, name: &str) -> bool {
        self.legacy_plaintext_secrets
            .lock()
            .map(|g| g.contains_key(name))
            .unwrap_or(false)
    }
}

impl SecretStore for MemoryPlatform {
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
        if self.fail_save.load(Ordering::Relaxed) {
            return Err(PlatformError::DpapiError("simulated save failure".into()));
        }
        let mut guard = self
            .secrets
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.insert(name.to_owned(), value.trim().to_owned());
        if let Ok(mut legacy) = self.legacy_plaintext_secrets.lock() {
            legacy.remove(name);
        }
        Ok(())
    }

    fn load_secret_with_outcome(&self, name: &str) -> Result<Option<SecretLoad>, PlatformError> {
        let encrypted = {
            let guard = self
                .secrets
                .lock()
                .map_err(|_| PlatformError::OsUnavailable)?;
            guard.get(name).cloned()
        };
        if let Some(value) = encrypted {
            if value.is_empty() {
                return Ok(None);
            }
            return Ok(Some(SecretLoad {
                value,
                outcome: SecretLoadOutcome::Encrypted,
            }));
        }

        let legacy_val = {
            let guard = self
                .legacy_plaintext_secrets
                .lock()
                .map_err(|_| PlatformError::OsUnavailable)?;
            guard.get(name).cloned()
        };
        let Some(plain) = legacy_val else {
            return Ok(None);
        };
        if plain.is_empty() {
            return Ok(None);
        }

        if self.fail_migration.load(Ordering::Relaxed) || self.fail_save.load(Ordering::Relaxed) {
            return Ok(Some(SecretLoad {
                value: String::new(),
                outcome: SecretLoadOutcome::MigrationFailed,
            }));
        }

        {
            let mut secrets = self
                .secrets
                .lock()
                .map_err(|_| PlatformError::OsUnavailable)?;
            secrets.insert(name.to_owned(), plain.clone());
        }
        {
            let mut legacy = self
                .legacy_plaintext_secrets
                .lock()
                .map_err(|_| PlatformError::OsUnavailable)?;
            legacy.remove(name);
        }
        Ok(Some(SecretLoad {
            value: plain,
            outcome: SecretLoadOutcome::Migrated,
        }))
    }

    fn delete_secret(&self, name: &str) -> Result<(), PlatformError> {
        let mut secrets = self
            .secrets
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        secrets.remove(name);
        let mut legacy = self
            .legacy_plaintext_secrets
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        legacy.remove(name);
        Ok(())
    }
}

impl EnvRegistry for MemoryPlatform {
    fn load_user_env(&self, name: &str) -> Result<Option<String>, PlatformError> {
        let guard = self
            .user_env
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        let Some(raw) = guard.get(name) else {
            return Ok(None);
        };
        let trimmed = raw.trim_ascii();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_owned()))
        }
    }
}

impl Notifier for MemoryPlatform {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError> {
        let mut guard = self
            .notifications
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.push((chat.clone(), title.to_owned(), body.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    #[test]
    fn memory_platform_round_trips_secrets_and_notifications() {
        let plat = MemoryPlatform::default();
        plat.save_secret("DEEPGRAM_API_KEY", "  demo-key \n")
            .unwrap();
        assert_eq!(
            plat.load_secret("DEEPGRAM_API_KEY").unwrap().as_deref(),
            Some("demo-key")
        );

        let key = ChatKey::new(Provider::Signal, "main", "uuid-1");
        plat.notify_message(&key, "Ada", "Ping").unwrap();
        assert_eq!(plat.notification_count(), 1);
    }

    #[test]
    fn memory_platform_migrates_legacy_plaintext_secret_and_fails_closed() {
        let plat = MemoryPlatform::default();
        plat.set_legacy_plaintext_secret(HUB_TOKEN_SECRET_NAME, "legacy-tok");
        assert!(plat.has_legacy_secret(HUB_TOKEN_SECRET_NAME));
        assert!(!plat.has_encrypted_secret(HUB_TOKEN_SECRET_NAME));

        let loaded = plat
            .load_secret_with_outcome(HUB_TOKEN_SECRET_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.outcome, SecretLoadOutcome::Migrated);
        assert_eq!(loaded.value, "legacy-tok");
        assert!(plat.has_encrypted_secret(HUB_TOKEN_SECRET_NAME));
        assert!(!plat.has_legacy_secret(HUB_TOKEN_SECRET_NAME));

        // Second load reports Encrypted.
        let second = plat
            .load_secret_with_outcome(HUB_TOKEN_SECRET_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(second.outcome, SecretLoadOutcome::Encrypted);
        assert_eq!(second.value, "legacy-tok");

        // Simulated migration failure fails closed.
        let broken = MemoryPlatform::default();
        broken.set_legacy_plaintext_secret(HUB_TOKEN_SECRET_NAME, "legacy-tok");
        broken.set_fail_migration(true);
        let res = broken
            .load_secret_with_outcome(HUB_TOKEN_SECRET_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(res.outcome, SecretLoadOutcome::MigrationFailed);
        assert!(broken.load_secret(HUB_TOKEN_SECRET_NAME).unwrap().is_none());
    }
}
