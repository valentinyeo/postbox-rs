pub mod age;
pub mod avatar;
pub mod decode;
pub mod drain;
pub mod emoji;
pub mod giphy;
pub mod lightbox;
pub mod mention;
pub mod paste;
pub mod unfurl;
pub mod video;

use thiserror::Error;

pub use video::{
    CachedVideoThumbnail, PreparedVideoThumb, VIDEO_THUMB_CACHE_PREFIX, VIDEO_THUMB_DIR,
    VIDEO_THUMB_MIME, VideoThumbMeta, VideoThumbnailCache, encode_png, format_duration_chip,
    is_video_mime, mp4_alias_path, needs_mp4_alias, prepare_video_thumbnail,
    video_thumbnail_cache_key, video_thumbnail_meta_rel_path, video_thumbnail_rel_path,
};

/// Maximum automatic media download size (100 MiB, `media_drain.zig:7`).
/// Files `<= 104_857_600` auto-download on chat open; `>= 104_857_601` require manual click.
pub const MAX_AUTO_BYTES: u64 = 100 * 1024 * 1024;
/// Maximum concurrent Hub media downloads (`media_drain.zig:6`).
pub const MAX_HUB_CONCURRENT: usize = 3;
/// Stall timeout for Hub media downloads (`media_drain.zig:179-183`).
pub const HUB_MEDIA_TIMEOUT_MS: u64 = 150_000;
/// 14-day media disk cache eviction horizon (`main.zig:633`).
pub const MEDIA_EVICTION_DAYS: i64 = 14;
/// Inline chat bubble preview fit box (`webp.zig:12-24`).
pub const PREVIEW_MAX_W: u32 = 420;
pub const PREVIEW_MAX_H: u32 = 250;
/// Maximum pixel count allowed before image allocation (`24 MiB / 4 = 6_291_456`).
pub const MAX_DECODE_PIXELS: u64 = 6_291_456;
/// Decoded CPU bitmap LRU budget (12 MiB, well under the 48 MiB hard RAM cap).
pub const RAM_BITMAP_BUDGET_BYTES: usize = 12 * 1024 * 1024;
/// Maximum RAM cap for decoded bitmaps across large image workloads.
pub const MAX_RAM_BITMAP_CAP_BYTES: usize = 48 * 1024 * 1024;
/// Intel Arc GPU texture atlas LRU cap (16 MiB VRAM, was 64 MiB before the M5.5 RAM diet).
pub const VRAM_TEXTURE_BUDGET_BYTES: usize = 16 * 1024 * 1024;
/// Maximum off-UI-thread decode jobs in flight at once (M5.5 concurrency cap).
pub const MAX_DECODE_WORKERS: usize = 2;
/// Fixed animated GIF tick interval (`main.zig:20377`).
pub const GIF_FRAME_INTERVAL_MS: u64 = 120;
/// M5.5 RAM diet: at most two idle keep-alive sockets per host on every HTTP client, so long
/// sync sessions cannot hold a pool of pooled TLS buffers.
pub const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 2;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("pixel_budget_exceeded")]
    PixelBudgetExceeded,
    #[error("file_too_large")]
    FileTooLarge,
    #[error("unsupported_format")]
    UnsupportedFormat,
    #[error("decode_failed: {0}")]
    DecodeFailed(String),
    #[error("invalid_dib")]
    InvalidDib,
    #[error("capacity_full")]
    CapacityFull,
    #[error("ssrf_blocked")]
    SsrfBlocked,
    #[error("invalid_url")]
    InvalidUrl,
    #[error("invalid_request: {0}")]
    InvalidRequest(String),
    #[error("needs_api_key")]
    NeedsApiKey,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("rate_limited")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("not_gif")]
    NotGif,
    #[error("cancelled")]
    Cancelled,
    #[error("http_status: {0}")]
    HttpStatus(u16),
    #[error("transport: {0}")]
    Transport(String),
    #[error("hub_error: {0}")]
    Hub(#[from] postbox_hub::HubError),
    #[error("store_error: {0}")]
    Store(#[from] postbox_store::StoreError),
    #[error("io_error: {0}")]
    Io(#[from] std::io::Error),
}
