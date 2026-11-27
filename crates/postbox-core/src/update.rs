//! `update.json` manifest parsing, semantic version comparison and SHA-256 verification.
//!
//! The published manifest comes from `.github/workflows/ci.yml` on every `v*` tag, so the
//! shape here is the shape CI writes: `version`, `tag`, `tag_name`, `asset`, `sha256`,
//! `size` and `assets[].browser_download_url`. A GitHub Release JSON payload also parses,
//! because it carries `tag_name` and `assets[]` with the same keys.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where `postbox` looks for the latest manifest.
pub const MANIFEST_URL: &str =
    "https://github.com/valentinyeo/postbox-rs/releases/latest/download/update.json";
/// Manifest filename written by CI next to the release zip.
pub const MANIFEST_FILE: &str = "update.json";
/// The Windows bundle CI publishes.
pub const WINDOWS_ASSET: &str = "postbox-windows-x64.zip";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `1.2.3`, `v1.2.3` or a tag that only adds a leading `v`. Pre-release and
    /// build suffixes are rejected: the updater only ever offers numbered releases.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.trim().strip_prefix('v').unwrap_or(text.trim());
        let mut parts = rest.split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parse_component(parts.next()?)?;
        let patch = parse_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }

    pub fn tag(&self) -> String {
        format!("v{self}")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn parse_component(raw: &str) -> Option<u32> {
    if raw.is_empty() || raw.len() > 9 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// `true` when `latest` is a strictly newer numbered release than `current`.
pub fn is_newer(latest: &str, current: Version) -> bool {
    match Version::parse(latest) {
        Some(latest) => latest > current,
        None => false,
    }
}

pub fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in out {
        hex.push(char::from(HEX[(byte >> 4) as usize]));
        hex.push(char::from(HEX[(byte & 0x0F) as usize]));
    }
    hex
}

/// `true` when `field` (`sha256:<64 hex>`) matches `bytes`.
pub fn digest_matches(bytes: &[u8], field: &str) -> bool {
    match expected_digest(field) {
        Some(expected) => digest_hex(bytes).eq_ignore_ascii_case(&expected),
        None => false,
    }
}

/// Accepts `sha256:<hex>` (the digest form GitHub writes) and a bare 64-character hex digest
/// (the form `update.json` writes in its top-level `sha256` field).
pub fn expected_digest(field: &str) -> Option<String> {
    let hex = field
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(field.trim())
        .trim_start_matches("sha256-");
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ManifestAsset {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub browser_download_url: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub digest: Option<String>,
}

/// `update.json` from CI, or a GitHub Release payload. Every field is optional so an older
/// manifest still parses.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub tag_name: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ManifestAsset>,
}

/// Everything the downloader and the installer need for one update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAsset {
    pub tag: String,
    pub version: Version,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub notes: String,
}

impl UpdateManifest {
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    /// Release tag, preferring the explicit tag fields over the bare version.
    pub fn tag(&self) -> Option<String> {
        let raw = self
            .tag
            .as_deref()
            .or(self.tag_name.as_deref())
            .or(self.version.as_deref())?
            .trim();
        let version = Version::parse(raw)?;
        Some(version.tag())
    }

    pub fn version(&self) -> Option<Version> {
        Version::parse(&self.tag()?)
    }

    /// SHA-256 the installer verifies before it touches the live executable.
    pub fn sha256(&self) -> Option<String> {
        if let Some(top) = self.sha256.as_deref()
            && let Some(hex) = expected_digest(top)
        {
            return Some(hex);
        }
        self.assets
            .iter()
            .find_map(|asset| asset.digest.as_deref())
            .and_then(expected_digest)
    }

    fn asset_name(&self) -> &str {
        self.asset.as_deref().unwrap_or(WINDOWS_ASSET).trim()
    }

    /// The release asset this build installs, or `None` when the manifest carries no usable
    /// download for the current platform.
    pub fn select_asset(&self) -> Option<UpdateAsset> {
        if self.draft || self.prerelease {
            return None;
        }
        let tag = self.tag()?;
        let version = Version::parse(&tag)?;
        let wanted = self.asset_name();
        let asset = self
            .assets
            .iter()
            .find(|asset| asset.name.trim().eq_ignore_ascii_case(wanted))?;
        let url = asset.browser_download_url.as_deref()?.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return None;
        }
        let sha256 = self.sha256()?;
        Some(UpdateAsset {
            tag,
            version,
            url: url.to_owned(),
            sha256,
            size: asset.size.or(self.size).unwrap_or(0),
            notes: self.body.clone(),
        })
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Nothing to do; carries the installed version.
    UpToDate { current: String },
    /// A verified asset newer than the installed build.
    Available { asset: Box<UpdateAsset> },
}

/// Compares the manifest against the installed version.
pub fn evaluate(manifest: &UpdateManifest, current: Version) -> CheckOutcome {
    match manifest.select_asset() {
        Some(asset) if asset.version > current => CheckOutcome::Available {
            asset: Box::new(asset),
        },
        _ => CheckOutcome::UpToDate {
            current: current.to_string(),
        },
    }
}

/// `true` when `name` is the Windows bundle CI publishes for this build.
pub fn is_windows_asset(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case(WINDOWS_ASSET)
        || (name.to_ascii_lowercase().starts_with("postbox")
            && name.to_ascii_lowercase().ends_with(".zip"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI_MANIFEST: &str = r#"{
        "version": "0.2.0",
        "tag": "v0.2.0",
        "tag_name": "v0.2.0",
        "asset": "postbox-windows-x64.zip",
        "sha256": "9f2c1b6fbb0f1c4bbd5a1f1c0c1e2f3a4b5c6d7e8f90a1b2c3d4e5f60718293a",
        "size": 24012345,
        "body": "- Toasts link straight to the chat",
        "assets": [
            {
                "name": "postbox-windows-x64.zip",
                "browser_download_url": "https://github.com/valentinyeo/postbox-rs/releases/download/v0.2.0/postbox-windows-x64.zip",
                "size": 24012345,
                "digest": "sha256:9f2c1b6fbb0f1c4bbd5a1f1c0c1e2f3a4b5c6d7e8f90a1b2c3d4e5f60718293a"
            }
        ]
    }"#;

    #[test]
    fn ci_manifest_selects_the_windows_asset_with_its_digest() {
        let manifest = UpdateManifest::parse(CI_MANIFEST).expect("manifest parses");
        assert_eq!(manifest.version(), Some(Version::new(0, 2, 0)));
        let asset = manifest.select_asset().expect("asset selected");
        assert_eq!(asset.tag, "v0.2.0");
        assert_eq!(
            asset.url,
            "https://github.com/valentinyeo/postbox-rs/releases/download/v0.2.0/postbox-windows-x64.zip"
        );
        assert_eq!(
            asset.sha256,
            "9f2c1b6fbb0f1c4bbd5a1f1c0c1e2f3a4b5c6d7e8f90a1b2c3d4e5f60718293a"
        );
        assert_eq!(asset.size, 24_012_345);
        assert!(asset.notes.contains("Toasts"));
    }

    #[test]
    fn github_release_payload_parses_with_the_asset_digest() {
        let release = r#"{
            "tag_name": "v0.3.1",
            "draft": false,
            "prerelease": false,
            "body": "- Notes",
            "assets": [{
                "name": "postbox-windows-x64.zip",
                "browser_download_url": "https://example.invalid/postbox-windows-x64.zip",
                "size": 10,
                "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            }]
        }"#;
        let manifest = UpdateManifest::parse(release).expect("release parses");
        let asset = manifest.select_asset().expect("asset selected");
        assert_eq!(asset.version, Version::new(0, 3, 1));
        assert_eq!(
            asset.sha256,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn manifests_without_a_digest_or_matching_asset_are_refused() {
        let no_digest = r#"{"tag":"v1.0.0","assets":[{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#;
        assert!(
            UpdateManifest::parse(no_digest)
                .unwrap()
                .select_asset()
                .is_none()
        );

        let wrong_asset = r#"{"tag":"v1.0.0","sha256":"0000000000000000000000000000000000000000000000000000000000000000","assets":[{"name":"macos.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#;
        assert!(
            UpdateManifest::parse(wrong_asset)
                .unwrap()
                .select_asset()
                .is_none()
        );

        let short_digest = r#"{"tag":"v1.0.0","sha256":"abc","assets":[{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#;
        assert!(
            UpdateManifest::parse(short_digest)
                .unwrap()
                .select_asset()
                .is_none()
        );

        let draft = r#"{"tag":"v1.0.0","draft":true,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","assets":[{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#;
        assert!(
            UpdateManifest::parse(draft)
                .unwrap()
                .select_asset()
                .is_none()
        );
    }

    #[test]
    fn versions_compare_and_reject_malformed_tags() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0"), Some(Version::new(0, 10, 0)));
        assert_eq!(Version::parse("v1.2"), None);
        assert_eq!(Version::parse("v1.2.3-rc1"), None);
        assert_eq!(Version::parse("latest"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::new(0, 2, 0).tag(), "v0.2.0");

        let current = Version::new(0, 1, 0);
        assert!(is_newer("v0.1.1", current));
        assert!(is_newer("v0.2.0", current));
        assert!(is_newer("v1.0.0", current));
        assert!(!is_newer("v0.1.0", current));
        assert!(!is_newer("v0.0.9", current));
        assert!(!is_newer("garbage", current));
    }

    #[test]
    fn digests_match_only_on_the_exact_bytes() {
        let bytes = b"hello world";
        let hex = digest_hex(bytes);
        assert!(digest_matches(bytes, &format!("sha256:{hex}")));
        assert!(digest_matches(bytes, &hex));
        assert!(digest_matches(
            bytes,
            &format!("sha256:{}", hex.to_uppercase())
        ));
        assert!(!digest_matches(b"hello worle", &format!("sha256:{hex}")));
        assert!(!digest_matches(bytes, "sha256:00"));
        assert!(!digest_matches(bytes, "md5:"));
        assert!(!digest_matches(bytes, ""));
    }

    #[test]
    fn evaluate_offers_only_strictly_newer_releases() {
        let manifest = UpdateManifest::parse(CI_MANIFEST).unwrap();
        assert!(matches!(
            evaluate(&manifest, Version::new(0, 1, 0)),
            CheckOutcome::Available { .. }
        ));
        assert_eq!(
            evaluate(&manifest, Version::new(0, 2, 0)),
            CheckOutcome::UpToDate {
                current: "0.2.0".into()
            }
        );
    }

    #[test]
    fn windows_asset_names_are_recognised() {
        assert!(is_windows_asset("postbox-windows-x64.zip"));
        assert!(is_windows_asset("Postbox-windows-x64.zip"));
        assert!(!is_windows_asset("postbox.exe"));
    }
}
