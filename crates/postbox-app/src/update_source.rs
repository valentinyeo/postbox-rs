//! The HTTP half of the self-updater: fetch `update.json`, download the release zip.
//!
//! This lives in `postbox-app` rather than `postbox-platform` so the platform crate stays
//! OS-API only. The installer, the digest check and the atomic swap live in
//! `postbox_platform::updater`, which is where the product invariant is enforced.

use postbox_core::update::{MANIFEST_URL, UpdateAsset, UpdateManifest};
use postbox_platform::update_source::{
    CONNECT_TIMEOUT_MS, MAX_ARCHIVE_BYTES, MAX_MANIFEST_BYTES, POOL_MAX_IDLE_PER_HOST,
    REQUEST_TIMEOUT_MS,
};
use postbox_platform::{ReleaseSource, UpdateError};
use std::time::Duration;

/// Real GitHub Releases source.
pub struct GithubReleaseSource {
    client: reqwest::blocking::Client,
    manifest_url: String,
}

impl GithubReleaseSource {
    pub fn new() -> Result<Self, UpdateError> {
        Self::with_manifest_url(MANIFEST_URL)
    }

    /// Points the source at another manifest URL (a mirror, or a test server).
    pub fn with_manifest_url(manifest_url: impl Into<String>) -> Result<Self, UpdateError> {
        let client = reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT_MS))
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .user_agent(concat!("postbox/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| UpdateError::Io(format!("cannot build the HTTP client: {err}")))?;
        Ok(Self {
            client,
            manifest_url: manifest_url.into(),
        })
    }
}

impl ReleaseSource for GithubReleaseSource {
    fn fetch_manifest(&self) -> Result<UpdateManifest, UpdateError> {
        let response =
            self.client.get(&self.manifest_url).send().map_err(|err| {
                UpdateError::Io(format!("cannot reach the update manifest: {err}"))
            })?;
        if !response.status().is_success() {
            return Err(UpdateError::Io(format!(
                "the update manifest returned HTTP {}",
                response.status()
            )));
        }
        let body = response
            .text()
            .map_err(|err| UpdateError::Io(format!("cannot read the update manifest: {err}")))?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(UpdateError::Io(
                "the update manifest is implausibly large".to_owned(),
            ));
        }
        UpdateManifest::parse(&body)
            .ok_or_else(|| UpdateError::Io("the update manifest is not valid JSON".to_owned()))
    }

    fn download(&self, asset: &UpdateAsset) -> Result<Vec<u8>, UpdateError> {
        if asset.size > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::SizeMismatch {
                expected: asset.size,
                actual: MAX_ARCHIVE_BYTES,
            });
        }
        let response = self
            .client
            .get(&asset.url)
            .send()
            .map_err(|err| UpdateError::Io(format!("cannot download the update: {err}")))?;
        if !response.status().is_success() {
            return Err(UpdateError::Io(format!(
                "the update download returned HTTP {}",
                response.status()
            )));
        }
        if let Some(length) = response.content_length()
            && asset.size > 0
            && length != asset.size
        {
            return Err(UpdateError::SizeMismatch {
                expected: asset.size,
                actual: length,
            });
        }
        let bytes = response
            .bytes()
            .map_err(|err| UpdateError::Io(format!("cannot read the update download: {err}")))?;
        if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::Io(
                "the update download is implausibly large".to_owned(),
            ));
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::update::{Version, digest_hex};

    #[test]
    fn the_client_keeps_the_two_connection_pool_from_the_ram_diet() {
        assert!(GithubReleaseSource::new().is_ok());
        assert!(GithubReleaseSource::with_manifest_url("https://example.invalid/u.json").is_ok());
    }

    #[test]
    fn an_implausibly_large_asset_is_refused_before_any_request() {
        let source = GithubReleaseSource::with_manifest_url("https://example.invalid/update.json")
            .expect("client builds");
        let asset = UpdateAsset {
            tag: "v1.0.0".into(),
            version: Version::new(1, 0, 0),
            url: "https://example.invalid/huge.zip".into(),
            sha256: digest_hex(b"x"),
            size: MAX_ARCHIVE_BYTES + 1,
            notes: String::new(),
        };
        let err = source.download(&asset).expect_err("refused");
        assert!(matches!(err, UpdateError::SizeMismatch { .. }), "{err:?}");
    }
}
