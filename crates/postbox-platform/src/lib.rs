use postbox_core::ChatKey;
use std::collections::HashMap;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("secret_not_found")]
    SecretNotFound,
    #[error("os_unavailable")]
    OsUnavailable,
    #[error("io_error")]
    Io(#[from] std::io::Error),
}

/// Encrypted per-user secret storage (Windows DPAPI + HKCU\Environment seeding on Windows).
pub trait SecretStore: Send + Sync {
    fn load_secret(&self, name: &str) -> Result<Option<String>, PlatformError>;
    fn save_secret(&self, name: &str, value: &str) -> Result<(), PlatformError>;
}

/// OS desktop notification sink (Windows Toast XML on Windows).
pub trait Notifier: Send + Sync {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError>;
}

/// Portable in-memory mock platform used for fast Linux & headless unit tests.
#[derive(Default)]
pub struct MemoryPlatform {
    secrets: Mutex<HashMap<String, String>>,
    notifications: Mutex<Vec<(ChatKey, String, String)>>,
}

impl MemoryPlatform {
    pub fn notification_count(&self) -> usize {
        self.notifications.lock().map(|n| n.len()).unwrap_or(0)
    }
}

impl SecretStore for MemoryPlatform {
    fn load_secret(&self, name: &str) -> Result<Option<String>, PlatformError> {
        let guard = self
            .secrets
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.get(name).cloned())
    }

    fn save_secret(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        let mut guard = self
            .secrets
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.insert(name.to_owned(), value.trim().to_owned());
        Ok(())
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

#[cfg(windows)]
pub mod win {
    //! Milestone 4–7 Windows-native implementations:
    //! - DPAPI secret storage + `HKCU\Environment\DEEPGRAM_API_KEY` seeding
    //! - WASAPI capture & render
    //! - Media Foundation + Intel Arc DXVA (`IMFDXGIDeviceManager`) video decode
    //! - Windows Toast XML notifications
    //! - `postbox://` registry handler + named pipes (`postbox-link-*`, `postbox-control-*`)
    //! - Atomic `MoveFileExW` self-updater
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
}
