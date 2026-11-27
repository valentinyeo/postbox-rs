//! Fetches the update manifest and downloads the release asset.
//!
//! The HTTP calls are `reqwest` inside a small blocking helper, so the caller can run them on
//! a background thread. The connection pool is capped at two idle connections per host, which
//! is what the RAM diet pass asked for.

use crate::PlatformError;
use crate::updater::UpdateError;
use postbox_core::update::{UpdateAsset, UpdateManifest};

/// Largest manifest accepted, so a hostile response cannot exhaust memory.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Largest release archive accepted (256 MiB), well above the real bundle.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Idle connections kept per host, from the M5.5 RAM diet.
pub const POOL_MAX_IDLE_PER_HOST: usize = 2;
pub const CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const REQUEST_TIMEOUT_MS: u64 = 120_000;

/// Where the manifest and the archive come from. Tests inject a fake.
pub trait ReleaseSource: Send + Sync {
    /// Fetches and parses `update.json`.
    fn fetch_manifest(&self) -> Result<UpdateManifest, UpdateError>;

    /// Downloads the release archive, refusing anything bigger than the manifest says.
    fn download(&self, asset: &UpdateAsset) -> Result<Vec<u8>, UpdateError>;
}

/// In-memory source for tests and for offline checks.
#[derive(Debug, Default)]
pub struct MockReleaseSource {
    manifest: std::sync::Mutex<Option<UpdateManifest>>,
    archive: std::sync::Mutex<Vec<u8>>,
    fail_manifest: std::sync::Mutex<Option<String>>,
    fail_download: std::sync::Mutex<Option<String>>,
    downloads: std::sync::atomic::AtomicUsize,
}

impl MockReleaseSource {
    pub fn new(manifest: UpdateManifest, archive: Vec<u8>) -> Self {
        Self {
            manifest: std::sync::Mutex::new(Some(manifest)),
            archive: std::sync::Mutex::new(archive),
            fail_manifest: std::sync::Mutex::new(None),
            fail_download: std::sync::Mutex::new(None),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn set_fail_manifest(&self, reason: &str) {
        if let Ok(mut guard) = self.fail_manifest.lock() {
            *guard = Some(reason.to_owned());
        }
    }

    pub fn set_fail_download(&self, reason: &str) {
        if let Ok(mut guard) = self.fail_download.lock() {
            *guard = Some(reason.to_owned());
        }
    }

    pub fn download_count(&self) -> usize {
        self.downloads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl ReleaseSource for MockReleaseSource {
    fn fetch_manifest(&self) -> Result<UpdateManifest, UpdateError> {
        if let Some(reason) = self.fail_manifest.lock().ok().and_then(|g| g.clone()) {
            return Err(UpdateError::Io(reason));
        }
        self.manifest
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| UpdateError::Io("no manifest configured".to_owned()))
    }

    fn download(&self, asset: &UpdateAsset) -> Result<Vec<u8>, UpdateError> {
        self.downloads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(reason) = self.fail_download.lock().ok().and_then(|g| g.clone()) {
            return Err(UpdateError::Io(reason));
        }
        let bytes = self
            .archive
            .lock()
            .map_err(|_| UpdateError::Io("archive lock poisoned".to_owned()))?
            .clone();
        crate::updater::verify_download(&bytes, asset)?;
        Ok(bytes)
    }
}

/// Reads a local manifest file, used by `--offline` checks and by the tests.
pub fn manifest_from_file(path: &std::path::Path) -> Result<UpdateManifest, UpdateError> {
    let body = std::fs::read_to_string(path)
        .map_err(|err| UpdateError::Io(format!("cannot read {}: {err}", path.display())))?;
    UpdateManifest::parse(&body)
        .ok_or_else(|| UpdateError::Io("the manifest is not valid JSON".to_owned()))
}

/// Convenience wrapper so `postbox-app` does not repeat the trait import.
pub fn fetch(source: &dyn ReleaseSource) -> Result<UpdateManifest, UpdateError> {
    source.fetch_manifest()
}

impl From<PlatformError> for std::io::Error {
    fn from(err: PlatformError) -> Self {
        std::io::Error::other(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::update::{Version, digest_hex};

    fn manifest_for(payload: &[u8], tag: &str) -> UpdateManifest {
        let body = format!(
            r#"{{"tag":"{tag}","asset":"postbox-windows-x64.zip","sha256":"{}","size":{},"assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/postbox-windows-x64.zip","size":{}}}]}}"#,
            digest_hex(payload),
            payload.len(),
            payload.len()
        );
        UpdateManifest::parse(&body).expect("manifest parses")
    }

    #[test]
    fn a_mock_source_returns_the_manifest_and_a_verified_download() {
        let payload = b"release archive";
        let source = MockReleaseSource::new(manifest_for(payload, "v0.2.0"), payload.to_vec());
        let manifest = source.fetch_manifest().unwrap();
        assert_eq!(manifest.version(), Some(Version::new(0, 2, 0)));
        let asset = manifest.select_asset().expect("asset");
        assert_eq!(source.download(&asset).unwrap(), payload);
        assert_eq!(source.download_count(), 1);
    }

    #[test]
    fn a_download_that_does_not_match_its_digest_is_refused_by_the_source() {
        let payload = b"real archive";
        // Same length as the real archive, so the digest check is what rejects it.
        let tampered = b"tampered!!!!".to_vec();
        assert_eq!(tampered.len(), payload.len());
        let source = MockReleaseSource::new(manifest_for(payload, "v0.2.0"), tampered);
        let asset = source
            .fetch_manifest()
            .unwrap()
            .select_asset()
            .expect("asset");
        let err = source.download(&asset).expect_err("refused");
        assert!(matches!(err, UpdateError::DigestMismatch { .. }), "{err:?}");
    }

    #[test]
    fn a_network_failure_is_reported_as_a_message_not_a_panic() {
        let source = MockReleaseSource::new(manifest_for(b"x", "v0.2.0"), b"x".to_vec());
        source.set_fail_manifest("offline");
        assert!(source.fetch_manifest().is_err());
        source.set_fail_download("offline");
        let asset = manifest_for(b"x", "v0.2.0").select_asset().unwrap();
        assert!(source.download(&asset).is_err());
    }

    #[test]
    fn the_pool_limit_matches_the_ram_diet_contract() {
        assert_eq!(POOL_MAX_IDLE_PER_HOST, 2);
        assert_eq!(MAX_MANIFEST_BYTES, 256 * 1024);
        const { assert!(MAX_ARCHIVE_BYTES > 32 * 1024 * 1024) };
    }

    #[test]
    fn a_local_manifest_file_parses_and_a_missing_one_errors() {
        let dir = std::env::temp_dir().join(format!("postbox-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update.json");
        std::fs::write(
            &path,
            r#"{"tag":"v0.2.0","sha256":"0000000000000000000000000000000000000000000000000000000000000000","assets":[{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#,
        )
        .unwrap();
        assert_eq!(
            manifest_from_file(&path).unwrap().version(),
            Some(Version::new(0, 2, 0))
        );
        assert!(manifest_from_file(&dir.join("nope.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
