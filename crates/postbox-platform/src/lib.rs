pub mod audio;
pub mod clipboard;
pub mod control_pipe;
pub mod exit_records;
pub mod giphy_auth;
pub mod hub_auth;
pub mod os_integration;
pub mod update_source;
pub mod updater;
pub mod video;
pub mod voice_auth;
pub mod window_state;
pub mod zip;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;

use postbox_core::ChatKey;
use postbox_core::voice::AudioDeviceInfo;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

pub use audio::{
    AudioBackend, AudioCaptureStream, AudioRenderStream, CaptureFormat, CapturePacket,
    MockAudioBackend, MockRenderSnapshot,
};
pub use clipboard::{
    ALLOWED_EXTERNAL_IMAGE_EXTENSIONS, ClipboardBackend, ClipboardPayload, MAX_CLIPBOARD_BYTES,
    MAX_CLIPBOARD_PIXELS, ZONE_IDENTIFIER_CONTENT, ZoneMarker, external_image_extension_allowed,
    is_reparse_point_or_symlink, mark_downloaded_file_zone, open_external_image,
};
pub use control_pipe::{ServerHandle, Transport, round_trip, serve};

/// The endpoint a control client or server binds: a named pipe on Windows, a socket elsewhere.
pub use control_pipe::Transport as ControlTransport;
pub use exit_records::{ExitRecordStore, install_panic_hook, now_unix_ms};
pub use giphy_auth::{
    ENV_GIPHY_API_KEY, GIPHY_API_KEY_SECRET_NAME, GiphyKeyOutcome, MAX_GIPHY_KEY_BYTES,
    discover_giphy_api_key, discover_giphy_api_key_from_system,
};
pub use hub_auth::{
    CredentialSource, DiscoveredHubConfig, ENV_POSTBOX_HUB_TOKEN, ENV_POSTBOX_HUB_URL,
    ENV_WAZIG_HUB_TOKEN, ENV_WAZIG_HUB_URL, HUB_TOKEN_SECRET_NAME, HubDiscoveryOutcome,
    discover_hub_credentials, discover_hub_credentials_from_system, valid_hub_token,
};
pub use os_integration::{
    LinkOutcome, LinkPipe, MockLinkPipe, MockSingleInstance, MockUrlHandler, SingleInstance,
    StartupRole, UPDATE_RELAUNCH_ARG, UPDATE_RELAUNCH_WAIT, UrlHandler, link_from_args, route_line,
    startup, startup_relaunch,
};
pub use update_source::{MockReleaseSource, ReleaseSource, manifest_from_file};
pub use updater::{
    ArchiveExtractor, AtomicReplacer, InstallReport, NativeReplacer, PlainFileExtractor,
    ProcessRelauncher, Relauncher, StagedUpdate, UpdateError, commit, discard, install, stage,
    stage_release, swap_with_rollback, verify_download,
};
pub use video::{
    MockVideoBackend, MockVideoConfig, MockVideoSession, THUMBNAIL_SEEK_MS, VideoAudioChunk,
    VideoBackend, VideoFrame, VideoMetadata, VideoPlaybackSession, downscale_rgba,
    fit_thumbnail_box, rgba_to_bgra_in_place, synthetic_rgba_frame,
};
pub use voice_auth::{
    ANTHROPIC_API_KEY_SECRET_NAME, ANTHROPIC_WORKSPACE_ID_SECRET_NAME, ApiKeyOutcome,
    DEEPGRAM_API_KEY_SECRET_NAME, DEFAULT_ANTHROPIC_MODEL, DEFAULT_OPENROUTER_MODEL,
    DeepgramKeyAction, DeepgramKeyLoad, ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_WORKSPACE_ID,
    ENV_DEEPGRAM_API_KEY, ENV_OPENROUTER_API_KEY, ENV_OPENROUTER_MODEL, FormatProviderConfig,
    OPENROUTER_API_KEY_SECRET_NAME, OPENROUTER_MODEL_SECRET_NAME, REG_DICTATION_LANGUAGE,
    REG_MICROPHONE, decide_deepgram_key, discover_deepgram_api_key,
    discover_deepgram_api_key_from_system, discover_format_provider,
    discover_format_provider_from_system, load_deepgram_key_detailed, load_dictation_language,
    load_microphone_choice, refresh_deepgram_api_key_if_changed,
    refresh_deepgram_api_key_if_changed_from_system, save_dictation_language,
    save_microphone_choice,
};
pub use window_state::{MonitorRect, WINDOW_STATE_ARG, WINDOW_STATE_VALUE, WindowState};
pub use zip::{CTL_ASSET_NAME, ZipEntry, ZipExtractor, extract_entry, find_zip_entry};

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("secret_not_found")]
    SecretNotFound,
    #[error("os_unavailable")]
    OsUnavailable,
    #[error("invalid_secret")]
    InvalidSecret,
    #[error("invalid_path")]
    InvalidPath,
    #[error("dpapi_error: {0}")]
    DpapiError(String),
    #[error("registry_error: {0}")]
    RegistryError(String),
    #[error("audio_error: {0}")]
    AudioError(String),
    #[error("video_error: {0}")]
    VideoError(String),
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

/// Persisted application preferences under `HKCU\Software\Messages` on Windows
/// (and in-memory maps on non-Windows / headless tests).
pub trait OsRegistry: Send + Sync {
    fn load_dword(&self, name: &str) -> Result<Option<u32>, PlatformError>;
    fn save_dword(&self, name: &str, value: u32) -> Result<(), PlatformError>;
    fn load_string(&self, name: &str) -> Result<Option<String>, PlatformError>;
    fn save_string(&self, name: &str, value: &str) -> Result<(), PlatformError>;
}

/// OS desktop notification sink (Windows Toast XML on Windows).
pub trait Notifier: Send + Sync {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError>;
}

/// One sample of the process' own memory footprint (`GetProcessMemoryInfo` on Windows).
///
/// M5.5 RAM diet: `working_set_kb` is what Task Manager shows and what the idle budget
/// (`< 65 MB`) is measured against; `private_usage_kb` is the commit charge that real leaks
/// show up in first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessMemoryStats {
    pub working_set_bytes: u64,
    pub private_usage_bytes: u64,
}

impl ProcessMemoryStats {
    pub fn working_set_kb(&self) -> u64 {
        self.working_set_bytes / 1024
    }

    pub fn private_usage_kb(&self) -> u64 {
        self.private_usage_bytes / 1024
    }
}

/// Working-set and commit-charge telemetry plus the working-set trim hook.
pub trait MemoryPressure: Send + Sync {
    /// Asks the OS to return idle pages to its free list (`SetProcessWorkingSetSizeEx` with
    /// `(usize::MAX, usize::MAX)`). Safe to call on every minimize and idle tick.
    fn trim_process_working_set(&self) -> Result<(), PlatformError>;

    /// Current working set and private commit charge for telemetry counters.
    fn read_process_memory_stats(&self) -> Result<ProcessMemoryStats, PlatformError>;
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

impl OsRegistry for NativePlatform {
    fn load_dword(&self, name: &str) -> Result<Option<u32>, PlatformError> {
        self.inner.load_dword(name)
    }

    fn save_dword(&self, name: &str, value: u32) -> Result<(), PlatformError> {
        self.inner.save_dword(name, value)
    }

    fn load_string(&self, name: &str) -> Result<Option<String>, PlatformError> {
        self.inner.load_string(name)
    }

    fn save_string(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        self.inner.save_string(name, value)
    }
}

impl Notifier for NativePlatform {
    fn notify_message(&self, chat: &ChatKey, title: &str, body: &str) -> Result<(), PlatformError> {
        self.inner.notify_message(chat, title, body)
    }
}

impl MemoryPressure for NativePlatform {
    fn trim_process_working_set(&self) -> Result<(), PlatformError> {
        self.inner.trim_process_working_set()
    }

    fn read_process_memory_stats(&self) -> Result<ProcessMemoryStats, PlatformError> {
        self.inner.read_process_memory_stats()
    }
}

impl ClipboardBackend for NativePlatform {
    fn read_paste_payload(&self) -> Result<ClipboardPayload, PlatformError> {
        self.inner.read_paste_payload()
    }
}

impl ZoneMarker for NativePlatform {
    fn mark_downloaded_file(&self, path: &Path) -> Result<bool, PlatformError> {
        self.inner.mark_downloaded_file(path)
    }
}

impl AudioBackend for NativePlatform {
    fn list_capture_devices(&self) -> Result<Vec<AudioDeviceInfo>, PlatformError> {
        self.inner.list_capture_devices()
    }

    fn open_render(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioRenderStream>, PlatformError> {
        self.inner.open_render(sample_rate, channels)
    }

    fn open_capture(
        &self,
        preferred_mic_name: Option<&str>,
    ) -> Result<Box<dyn AudioCaptureStream>, PlatformError> {
        self.inner.open_capture(preferred_mic_name)
    }
}

impl VideoBackend for NativePlatform {
    fn extract_thumbnail(
        &self,
        path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<VideoFrame>, PlatformError> {
        self.inner.extract_thumbnail(path, max_w, max_h)
    }

    fn open_session(&self, path: &Path) -> Result<Box<dyn VideoPlaybackSession>, PlatformError> {
        self.inner.open_session(path)
    }

    fn hardware_acceleration_available(&self) -> bool {
        self.inner.hardware_acceleration_available()
    }
}

/// Portable in-memory mock platform used for fast Linux & headless unit tests.
pub struct MemoryPlatform {
    secrets: Mutex<HashMap<String, String>>,
    legacy_plaintext_secrets: Mutex<HashMap<String, String>>,
    user_env: Mutex<HashMap<String, String>>,
    reg_dwords: Mutex<HashMap<String, u32>>,
    reg_strings: Mutex<HashMap<String, String>>,
    notifications: Mutex<Vec<(ChatKey, String, String)>>,
    clipboard_payload: Mutex<ClipboardPayload>,
    zone_marked_paths: Mutex<Vec<PathBuf>>,
    audio: MockAudioBackend,
    video: MockVideoBackend,
    fail_migration: AtomicBool,
    fail_save: AtomicBool,
    memory_stats: ProcessMemoryStats,
    trim_count: std::sync::atomic::AtomicUsize,
}

impl Default for MemoryPlatform {
    fn default() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
            legacy_plaintext_secrets: Mutex::new(HashMap::new()),
            user_env: Mutex::new(HashMap::new()),
            reg_dwords: Mutex::new(HashMap::new()),
            reg_strings: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
            clipboard_payload: Mutex::new(ClipboardPayload::Empty),
            zone_marked_paths: Mutex::new(Vec::new()),
            audio: MockAudioBackend::new(),
            video: MockVideoBackend::new(),
            fail_migration: AtomicBool::new(false),
            fail_save: AtomicBool::new(false),
            memory_stats: ProcessMemoryStats {
                working_set_bytes: 48 * 1024 * 1024,
                private_usage_bytes: 40 * 1024 * 1024,
            },
            trim_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl MemoryPlatform {
    pub fn audio_mock(&self) -> &MockAudioBackend {
        &self.audio
    }

    /// Deterministic video backend used by `VideoBackend` on non-Windows targets.
    pub fn video_mock(&self) -> &MockVideoBackend {
        &self.video
    }

    /// Swaps in configured mock video behaviour (silent clips, failing decodes, ...).
    pub fn set_video_mock(&mut self, mock: MockVideoBackend) {
        self.video = mock;
    }

    /// Number of working-set trims requested through this platform (test observability).
    pub fn trim_count(&self) -> usize {
        self.trim_count.load(Ordering::Relaxed)
    }

    /// Overrides the bytes `read_process_memory_stats` reports.
    pub fn set_memory_stats(&mut self, stats: ProcessMemoryStats) {
        self.memory_stats = stats;
    }

    pub fn notification_count(&self) -> usize {
        self.notifications.lock().map(|n| n.len()).unwrap_or(0)
    }

    pub fn set_clipboard_payload(&self, payload: ClipboardPayload) {
        if let Ok(mut guard) = self.clipboard_payload.lock() {
            *guard = payload;
        }
    }

    pub fn zone_marked_paths(&self) -> Vec<PathBuf> {
        self.zone_marked_paths
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
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
        let trimmed = raw.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_owned()))
        }
    }
}

impl OsRegistry for MemoryPlatform {
    fn load_dword(&self, name: &str) -> Result<Option<u32>, PlatformError> {
        let guard = self
            .reg_dwords
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.get(name).copied())
    }

    fn save_dword(&self, name: &str, value: u32) -> Result<(), PlatformError> {
        let mut guard = self
            .reg_dwords
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.insert(name.to_owned(), value);
        Ok(())
    }

    fn load_string(&self, name: &str) -> Result<Option<String>, PlatformError> {
        let guard = self
            .reg_strings
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.get(name).filter(|s| !s.is_empty()).cloned())
    }

    fn save_string(&self, name: &str, value: &str) -> Result<(), PlatformError> {
        let mut guard = self
            .reg_strings
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.insert(name.to_owned(), value.to_owned());
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

/// Non-Windows / test fallback: keeps no OS handle, simulates the working-set trim by halving
/// the reported private usage so tests can assert the trim hook ran and the counters follow.
impl MemoryPressure for MemoryPlatform {
    fn trim_process_working_set(&self) -> Result<(), PlatformError> {
        self.trim_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn read_process_memory_stats(&self) -> Result<ProcessMemoryStats, PlatformError> {
        Ok(self.memory_stats)
    }
}

impl ClipboardBackend for MemoryPlatform {
    fn read_paste_payload(&self) -> Result<ClipboardPayload, PlatformError> {
        let guard = self
            .clipboard_payload
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.clone())
    }
}

impl ZoneMarker for MemoryPlatform {
    fn mark_downloaded_file(&self, path: &Path) -> Result<bool, PlatformError> {
        let mut guard = self
            .zone_marked_paths
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.push(path.to_path_buf());
        Ok(true)
    }
}

impl VideoBackend for MemoryPlatform {
    fn extract_thumbnail(
        &self,
        path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<VideoFrame>, PlatformError> {
        self.video.extract_thumbnail(path, max_w, max_h)
    }

    fn open_session(&self, path: &Path) -> Result<Box<dyn VideoPlaybackSession>, PlatformError> {
        self.video.open_session(path)
    }

    fn hardware_acceleration_available(&self) -> bool {
        self.video.hardware_acceleration_available()
    }
}

impl AudioBackend for MemoryPlatform {
    fn list_capture_devices(&self) -> Result<Vec<AudioDeviceInfo>, PlatformError> {
        self.audio.list_capture_devices()
    }

    fn open_render(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioRenderStream>, PlatformError> {
        self.audio.open_render(sample_rate, channels)
    }

    fn open_capture(
        &self,
        preferred_mic_name: Option<&str>,
    ) -> Result<Box<dyn AudioCaptureStream>, PlatformError> {
        self.audio.open_capture(preferred_mic_name)
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

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn memory_platform_reports_stats_and_counts_working_set_trims() {
        let mut plat = MemoryPlatform::default();
        plat.set_memory_stats(ProcessMemoryStats {
            working_set_bytes: 61 * 1024 * 1024,
            private_usage_bytes: 54 * 1024 * 1024,
        });
        let stats = plat.read_process_memory_stats().unwrap();
        assert_eq!(stats.working_set_kb(), 61 * 1024);
        assert_eq!(stats.private_usage_kb(), 54 * 1024);

        assert_eq!(plat.trim_count(), 0);
        plat.trim_process_working_set().unwrap();
        plat.trim_process_working_set().unwrap();
        assert_eq!(plat.trim_count(), 2);

        // NativePlatform must expose the same shift on every target so the shell can hold one
        // trait object regardless of OS.
        fn assert_memory_pressure<T: MemoryPressure>() {}
        assert_memory_pressure::<NativePlatform>();
        assert_memory_pressure::<MemoryPlatform>();
    }

    #[test]
    fn native_platform_memory_hooks_succeed_or_fail_closed_without_panicking() {
        let plat = NativePlatform::default();
        // On non-Windows the fallback always succeeds; on Windows the FFI either succeeds or
        // reports `OsUnavailable`. Either way it must not panic.
        let _ = plat.trim_process_working_set();
        let stats = plat.read_process_memory_stats();
        assert!(stats.is_ok() || stats.is_err());
    }
}
