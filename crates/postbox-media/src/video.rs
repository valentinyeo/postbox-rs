//! Disk-cached video thumbnails (Milestone 6, Feature #33).
//!
//! A video message never decodes twice. The first time a video finishes auto-downloading (or
//! the first time a chat with an already-cached video is opened) the backend extracts one still
//! frame at `THUMBNAIL_SEEK_MS`, fits it into `PREVIEW_MAX_W` x `PREVIEW_MAX_H`, encodes it as
//! PNG, and stores it in the content-addressed media cache keyed by the **video's** SHA-256.
//! Reopening the chat - or reopening the app - only reads that PNG.
//!
//! The `media_cache` row gives the thumbnail the same 14-day LRU eviction the rest of the media
//! cache uses, and the small JSON sidecar carries the duration chip and the hardware badge
//! without re-probing the container.

use crate::{MediaError, PREVIEW_MAX_H, PREVIEW_MAX_W};
use postbox_platform::{VideoBackend, VideoFrame};
use postbox_store::{MediaCacheEntry, Store};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// `media_cache` key prefix for video thumbnails (`videothumb:<video sha256>`).
pub const VIDEO_THUMB_CACHE_PREFIX: &str = "videothumb:";
/// Sub-directory inside the Hub media cache root that holds video thumbnails.
pub const VIDEO_THUMB_DIR: &str = "video-thumbs";
/// Encoded thumbnail format (lossless, tiny at preview size).
pub const VIDEO_THUMB_MIME: &str = "image/png";

/// Stable `media_cache` key for one video's thumbnail.
pub fn video_thumbnail_cache_key(video_sha256: &str) -> String {
    format!("{VIDEO_THUMB_CACHE_PREFIX}{video_sha256}")
}

/// Cache-relative PNG path, sharded on the first two hex characters so one busy inbox does not
/// put thousands of files in a single directory.
pub fn video_thumbnail_rel_path(video_sha256: &str) -> String {
    let shard = video_sha256.get(0..2).unwrap_or("00");
    format!("{VIDEO_THUMB_DIR}/{shard}/{video_sha256}.png")
}

/// Cache-relative sidecar path carrying duration + hardware metadata.
pub fn video_thumbnail_meta_rel_path(video_sha256: &str) -> String {
    let shard = video_sha256.get(0..2).unwrap_or("00");
    format!("{VIDEO_THUMB_DIR}/{shard}/{video_sha256}.json")
}

/// Whether a media MIME type is a video (drives thumbnail extraction after auto-download).
pub fn is_video_mime(mime: &str) -> bool {
    let lower = mime.trim().to_ascii_lowercase();
    lower.starts_with("video/")
}

/// `.f4v` / `.f4p` are MP4-shaped but Media Foundation's byte-stream handler rejects them, so
/// they need an `.mp4` alias (`video_alias.zig:9-20`).
pub fn needs_mp4_alias(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("f4v") | Some("f4p")
    )
}

/// `clip.f4v` -> `clip.f4v.mp4` (appended; the original bytes are never modified).
pub fn mp4_alias_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".mp4");
    PathBuf::from(os)
}

/// `m:ss` under an hour, `h:mm:ss` above (`media_layout.zig` clock shape).
pub fn format_duration_chip(duration_ms: u64) -> String {
    let total_secs = duration_ms / 1_000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3_600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

/// Duration + decode properties cached beside the thumbnail (no re-probe on reopen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoThumbMeta {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub hardware_accelerated: bool,
}

/// A thumbnail that is ready to feed into the GPU texture pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVideoThumbnail {
    pub path: PathBuf,
    pub meta: VideoThumbMeta,
    /// True when the PNG came from the disk cache (no decode happened this call).
    pub from_cache: bool,
}

impl CachedVideoThumbnail {
    /// Duration chip text for the video bubble (`1:04`).
    pub fn duration_label(&self) -> String {
        format_duration_chip(self.meta.duration_ms)
    }
}

/// A decoded thumbnail awaiting the (fast, main-thread) disk cache write. Decoding and PNG
/// encoding happen off the UI thread; only the small file write happens on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedVideoThumb {
    pub frame: VideoFrame,
    pub meta: VideoThumbMeta,
}

/// Decodes and fits one thumbnail without touching the disk (safe to run off the UI thread).
pub fn prepare_video_thumbnail(
    backend: &dyn VideoBackend,
    video_path: &Path,
    max_w: u32,
    max_h: u32,
) -> Result<Option<PreparedVideoThumb>, MediaError> {
    let metadata = backend
        .probe_metadata(video_path)
        .map_err(|e| MediaError::DecodeFailed(format!("probe_metadata: {e}")))?;
    let Some(frame) = backend
        .extract_thumbnail(video_path, max_w, max_h)
        .map_err(|e| MediaError::DecodeFailed(format!("extract_thumbnail: {e}")))?
    else {
        return Ok(None);
    };
    let meta = VideoThumbMeta {
        width: frame.width,
        height: frame.height,
        duration_ms: metadata.duration_ms,
        hardware_accelerated: metadata.hardware_accelerated,
    };
    Ok(Some(PreparedVideoThumb { frame, meta }))
}

/// Content-addressed on-disk thumbnail cache rooted at the Hub media cache directory.
#[derive(Debug, Clone)]
pub struct VideoThumbnailCache {
    root: PathBuf,
}

impl VideoThumbnailCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cached thumbnail for `video_sha256`, touching its 14-day LRU row and file mtime.
    pub fn lookup(&self, store: &Store, video_sha256: &str) -> Option<CachedVideoThumbnail> {
        let key = video_thumbnail_cache_key(video_sha256);
        let entry = store.get_media_cache(&key).ok().flatten()?;
        let path = self.root.join(&entry.rel_path);
        if !path.is_file() {
            let _ = store.delete_media_cache(&key);
            return None;
        }
        let meta = self.read_meta(video_sha256).unwrap_or(VideoThumbMeta {
            width: 0,
            height: 0,
            duration_ms: 0,
            hardware_accelerated: false,
        });
        let _ = store.touch_media_cache(&key, crate::age::now_unix_secs());
        crate::age::touch_file_mtime(&path);
        Some(CachedVideoThumbnail {
            path,
            meta,
            from_cache: true,
        })
    }

    /// Returns the cached thumbnail, or extracts, encodes and caches it exactly once.
    pub fn ensure(
        &self,
        store: &Store,
        backend: &dyn VideoBackend,
        video_sha256: &str,
        video_path: &Path,
    ) -> Result<Option<CachedVideoThumbnail>, MediaError> {
        self.ensure_with_max(
            store,
            backend,
            video_sha256,
            video_path,
            PREVIEW_MAX_W,
            PREVIEW_MAX_H,
        )
    }

    /// Same as `ensure` with explicit preview bounds (tests assert the fit box).
    pub fn ensure_with_max(
        &self,
        store: &Store,
        backend: &dyn VideoBackend,
        video_sha256: &str,
        video_path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<CachedVideoThumbnail>, MediaError> {
        if video_sha256.is_empty() {
            return Err(MediaError::InvalidRequest("empty_video_sha256".into()));
        }
        if let Some(hit) = self.lookup(store, video_sha256) {
            return Ok(Some(hit));
        }

        let Some(prepared) = prepare_video_thumbnail(backend, video_path, max_w, max_h)? else {
            return Ok(None);
        };
        self.store_prepared(store, video_sha256, &prepared)
            .map(Some)
    }

    /// Writes an already decoded thumbnail (PNG + sidecar + LRU row). Cheap enough for the UI
    /// thread; the decode itself belongs on the background pool.
    pub fn store_prepared(
        &self,
        store: &Store,
        video_sha256: &str,
        prepared: &PreparedVideoThumb,
    ) -> Result<CachedVideoThumbnail, MediaError> {
        if video_sha256.is_empty() {
            return Err(MediaError::InvalidRequest("empty_video_sha256".into()));
        }
        let bytes = encode_png(&prepared.frame)?;
        let rel_path = video_thumbnail_rel_path(video_sha256);
        let full_path = self.root.join(&rel_path);
        write_atomically(&full_path, &bytes)?;

        let meta = prepared.meta;
        if let Ok(json) = serde_json::to_vec(&meta) {
            let meta_path = self.root.join(video_thumbnail_meta_rel_path(video_sha256));
            let _ = write_atomically(&meta_path, &json);
        }

        store
            .upsert_media_cache(&MediaCacheEntry {
                cache_key: video_thumbnail_cache_key(video_sha256),
                sha256: video_sha256.to_owned(),
                rel_path,
                mime_type: VIDEO_THUMB_MIME.to_owned(),
                byte_size: bytes.len() as u64,
                last_accessed_at: crate::age::now_unix_secs(),
            })
            .map_err(MediaError::from)?;

        Ok(CachedVideoThumbnail {
            path: full_path,
            meta,
            from_cache: false,
        })
    }

    fn read_meta(&self, video_sha256: &str) -> Option<VideoThumbMeta> {
        let path = self.root.join(video_thumbnail_meta_rel_path(video_sha256));
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Encodes one RGBA frame as PNG.
pub fn encode_png(frame: &VideoFrame) -> Result<Vec<u8>, MediaError> {
    let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| MediaError::DecodeFailed("thumbnail_rgba_shape".into()))?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| MediaError::DecodeFailed(format!("png_encode: {e}")))?;
    Ok(out)
}

/// Writes `bytes` to `path` through a sibling temp file + rename, so a crash never leaves a
/// half-written PNG that the cache would serve forever.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), MediaError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "thumb".to_owned());
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_platform::{MockVideoBackend, MockVideoConfig};

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-video-thumbs-{tag}-{}-{}",
            std::process::id(),
            crate::age::now_unix_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_keys_and_paths_are_content_addressed() {
        let sha = "abcd1234ef567890";
        assert_eq!(video_thumbnail_cache_key(sha), format!("videothumb:{sha}"));
        assert_eq!(
            video_thumbnail_rel_path(sha),
            "video-thumbs/ab/abcd1234ef567890.png"
        );
        assert_eq!(
            video_thumbnail_meta_rel_path(sha),
            "video-thumbs/ab/abcd1234ef567890.json"
        );
    }

    #[test]
    fn duration_chip_matches_clock_shape() {
        assert_eq!(format_duration_chip(0), "0:00");
        assert_eq!(format_duration_chip(999), "0:00");
        assert_eq!(format_duration_chip(42_000), "0:42");
        assert_eq!(format_duration_chip(64_000), "1:04");
        assert_eq!(format_duration_chip(3_723_000), "1:02:03");
    }

    #[test]
    fn video_mime_and_mp4_alias_rules() {
        assert!(is_video_mime("video/mp4"));
        assert!(is_video_mime("  VIDEO/QUICKTIME "));
        assert!(!is_video_mime("image/png"));
        assert!(!is_video_mime("audio/ogg"));
        assert!(needs_mp4_alias(Path::new("a.f4v")));
        assert!(needs_mp4_alias(Path::new("a.F4P")));
        assert!(!needs_mp4_alias(Path::new("a.mp4")));
        assert_eq!(
            mp4_alias_path(Path::new("C:/m/a.f4v")),
            PathBuf::from("C:/m/a.f4v.mp4")
        );
    }

    #[test]
    fn first_call_extracts_and_caches_second_call_reads_disk() {
        let root = temp_root("ensure");
        let store = Store::open_in_memory().unwrap();
        let cache = VideoThumbnailCache::new(&root);
        let mock = MockVideoBackend::with_config(MockVideoConfig {
            width: 1280,
            height: 720,
            duration_ms: 65_000,
            ..MockVideoConfig::default()
        });
        let sha = "0011223344556677";

        let first = cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        assert!(!first.from_cache);
        assert_eq!(mock.extract_count(), 1);
        assert!(first.path.is_file());
        assert_eq!(first.duration_label(), "1:05");
        assert!(first.meta.hardware_accelerated);
        // Fitted into the 420x250 preview box, never upscaled.
        assert_eq!((first.meta.width, first.meta.height), (420, 236));

        // Second call is a pure disk hit: no second extraction, no probe.
        let second = cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        assert!(second.from_cache);
        assert_eq!(mock.extract_count(), 1);
        assert_eq!(second.path, first.path);
        assert_eq!(second.meta.duration_ms, 65_000);

        // The PNG decodes back to the fitted size (proves it is a real image).
        let decoded = image::open(&first.path).unwrap().to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (420, 236));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleted_png_re_extracts_instead_of_serving_a_stale_row() {
        let root = temp_root("restore");
        let store = Store::open_in_memory().unwrap();
        let cache = VideoThumbnailCache::new(&root);
        let mock = MockVideoBackend::new();
        let sha = "ffeeddccbbaa9988";

        let first = cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        assert_eq!(mock.extract_count(), 1);
        std::fs::remove_file(&first.path).unwrap();
        assert!(cache.lookup(&store, sha).is_none());

        let again = cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        assert!(!again.from_cache);
        assert_eq!(mock.extract_count(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expiring_the_lru_row_forces_one_fresh_extraction() {
        let root = temp_root("lru");
        let store = Store::open_in_memory().unwrap();
        let cache = VideoThumbnailCache::new(&root);
        let mock = MockVideoBackend::new();
        let sha = "0123456789abcdef";

        cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        let key = video_thumbnail_cache_key(sha);
        assert!(store.touch_media_cache(&key, 1).unwrap());
        assert!(store.evict_expired_media_older_than(2).unwrap().len() == 1);
        assert!(cache.lookup(&store, sha).is_none());
        cache
            .ensure(&store, &mock, sha, Path::new("clip.mp4"))
            .unwrap()
            .unwrap();
        assert_eq!(mock.extract_count(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failing_backends_and_empty_sha_surface_errors() {
        let root = temp_root("errors");
        let store = Store::open_in_memory().unwrap();
        let cache = VideoThumbnailCache::new(&root);
        let mock = MockVideoBackend::with_config(MockVideoConfig {
            fail_extract: true,
            ..MockVideoConfig::default()
        });
        assert!(matches!(
            cache.ensure(&store, &mock, "", Path::new("clip.mp4")),
            Err(MediaError::InvalidRequest(_))
        ));
        assert!(
            cache
                .ensure(&store, &mock, "aabbccdd", Path::new("clip.mp4"))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
