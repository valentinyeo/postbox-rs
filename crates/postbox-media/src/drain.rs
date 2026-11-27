use crate::age::{now_unix_secs, within_days};
use crate::{
    HUB_MEDIA_TIMEOUT_MS, MAX_AUTO_BYTES, MAX_HUB_CONCURRENT, MEDIA_EVICTION_DAYS, MediaError,
};
use postbox_core::{ChatKey, MediaKind, Message};
use postbox_hub::{
    DEFAULT_MEDIA_POLL_INTERVAL, DEFAULT_MEDIA_POLL_MAX_WAIT, HubClient, HubError, MediaFetchState,
};
use postbox_platform::{ZoneMarker, mark_downloaded_file_zone};
use postbox_store::{MediaCacheEntry, Store};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

pub const MAX_BURST_FILES: usize = 4;
pub const BURST_BUDGET_MS: u64 = 20_000;
pub const GONE_STATUS_CODE: u16 = 410;
pub const GONE_REPAINT_GAP_MS: u64 = 20_000;
pub const BACKGROUND_MEDIA_LIMIT: usize = 16;
pub const MAX_BACKGROUND_READ_FAILURES: u8 = 3;
pub const MAX_MEDIA_RETRIES: u8 = 3;
pub const MEDIA_RETRY_BASE_MS: u64 = 30_000;
pub const MAX_TRANSIENT_MEDIA_RETRIES: u8 = 8;
pub const MEDIA_TRANSIENT_BASE_MS: u64 = 5_000;
pub const MEDIA_TRANSIENT_CAP_MS: u64 = 60_000;
pub const MAX_MEDIA_FAILURES_TABLE: usize = 16;

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Returns `true` for all 7 downloadable media types (case-insensitive, `media_drain.zig:23-28`).
pub fn is_downloadable_type_str(raw: &str) -> bool {
    MediaKind::parse(raw).is_some_and(MediaKind::is_downloadable)
}

/// Product invariant (`main.zig:7382-7384`): non-revoked messages carrying any of the 7
/// downloadable media types auto-download on chat open (unless oversized).
pub fn is_downloadable_message(msg: &Message) -> bool {
    !msg.deleted && msg.media_kind.is_some_and(MediaKind::is_downloadable)
}

/// Exact port of `media_drain.zig:30-35`: suppresses transport placeholder captions like
/// `"Sent document"` or `"[Image]"` while preserving genuine captions.
pub fn is_placeholder(kind: Option<MediaKind>, text: &str) -> bool {
    let Some(k) = kind else {
        return false;
    };
    if !k.is_downloadable() {
        return false;
    }
    let kind_str = k.as_str();
    if let Some(rest) = text.strip_prefix("Sent ")
        && rest.eq_ignore_ascii_case(kind_str)
    {
        return true;
    }
    text.len() > 2
        && text.starts_with('[')
        && text.ends_with(']')
        && text[1..text.len() - 1].eq_ignore_ascii_case(kind_str)
}

/// Strictly greater than `100 MiB` (`104_857_600` auto-downloads; `104_857_601` requires click).
pub const fn is_oversized(size_bytes: u64) -> bool {
    size_bytes > MAX_AUTO_BYTES
}

pub const fn is_transient_http_status(status: u16) -> bool {
    status == 429 || (status >= 500 && status < 600)
}

/// Exact port of `main.zig:7307-7380` retry delay math (`count` is 1-based failure count).
/// Returns `None` once retries are exhausted (meaning mark unavailable).
pub fn media_retry_delay_ms(http_status: u16, failure_count: u8) -> Option<u64> {
    if http_status == GONE_STATUS_CODE {
        return None;
    }
    if is_transient_http_status(http_status) {
        if failure_count >= MAX_TRANSIENT_MEDIA_RETRIES {
            return None;
        }
        let shift = failure_count.saturating_sub(1).min(4);
        Some(MEDIA_TRANSIENT_CAP_MS.min(MEDIA_TRANSIENT_BASE_MS << shift))
    } else {
        if failure_count >= MAX_MEDIA_RETRIES {
            return None;
        }
        Some(MEDIA_RETRY_BASE_MS.saturating_mul(u64::from(failure_count)))
    }
}

/// Returns whether a background recent-chat drain should consider this chat due
/// (`media_drain.zig:51-73`).
pub fn background_chat_due(
    is_open: bool,
    timestamp: &str,
    swept_timestamp: Option<&str>,
    now_unix: i64,
) -> bool {
    !is_open
        && !timestamp.is_empty()
        && swept_timestamp != Some(timestamp)
        && within_days(timestamp, now_unix, MEDIA_EVICTION_DAYS)
}

/// Yielding rule for background drain (`media_drain.zig:60-73`): yields immediately when any
/// slot is busy, an open-chat candidate is waiting, or burst budget (4 files / 20s) is reached.
pub fn background_may_start(
    active_downloads: usize,
    open_chat_candidate_ready: bool,
    burst_files_started: usize,
    now_ms: u64,
    burst_started_ms: u64,
) -> bool {
    active_downloads == 0
        && !open_chat_candidate_ready
        && burst_files_started < MAX_BURST_FILES
        && now_ms.saturating_sub(burst_started_ms) < BURST_BUDGET_MS
}

/// Refill step gate (`media_drain.zig:77-83`): allows up to 3 concurrent Hub starts per tick.
pub const fn may_drain_step(steps_done: usize, last_step_started: bool) -> bool {
    steps_done < MAX_HUB_CONCURRENT && (steps_done == 0 || last_step_started)
}

/// Privacy-safe opaque media reference hash (`main.zig:7319-7331`) — never logs raw JID/msg_id.
pub fn privacy_safe_media_ref(chat: &ChatKey, msg_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [
        chat.account.as_bytes(),
        chat.id.as_bytes(),
        msg_id.as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hex_encode(&hasher.finalize())
}

/// Deterministic content-addressed cache key scoped by full `ChatKey` + `msg_id` so identical
/// `msg_id` values across different chats or accounts never collide.
pub fn media_cache_key(chat: &ChatKey, msg_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [
        chat.provider.as_str().as_bytes(),
        b"\0".as_slice(),
        chat.account.as_bytes(),
        b"\0".as_slice(),
        chat.id.as_bytes(),
        b"\0".as_slice(),
        msg_id.as_bytes(),
    ] {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let hex = hex_encode(&digest);
    format!("{}-{}", &hex[..16], &hex[16..32])
}

/// Exact port of `wa_hub.zig:258-278` extension resolution.
pub fn resolve_media_extension(
    filename: Option<&str>,
    mime: Option<&str>,
    media_kind: Option<MediaKind>,
) -> String {
    if let Some(m) = mime {
        let base = m.split(';').next().unwrap_or(m).trim();
        let mapped = match base {
            "video/mp4" => Some(".mp4"),
            "video/quicktime" => Some(".mov"),
            "image/png" => Some(".png"),
            "image/jpeg" => Some(".jpg"),
            "image/webp" => Some(".webp"),
            "image/gif" => Some(".gif"),
            "audio/ogg" | "audio/opus" => Some(".ogg"),
            "audio/mpeg" => Some(".mp3"),
            "application/pdf" => Some(".pdf"),
            _ => None,
        };
        if let Some(ext) = mapped {
            return ext.to_owned();
        }
    }
    if let Some(fname) = filename
        && let Some(ext) = Path::new(fname).extension().and_then(|e| e.to_str())
    {
        let dotted_len = ext.len() + 1;
        if dotted_len > 1 && dotted_len <= 12 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return format!(".{ext}");
        }
    }
    if media_kind == Some(MediaKind::Video) {
        ".mp4".to_owned()
    } else {
        ".bin".to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentState {
    Local,
    Downloading,
    Oversized,
    Unavailable,
    Retry,
}

/// Exact port of `media_drain.zig:191-196`.
pub const fn voice_state(
    local: bool,
    in_flight: bool,
    failed: bool,
    size_bytes: u64,
) -> AttachmentState {
    if local {
        AttachmentState::Local
    } else if in_flight {
        AttachmentState::Downloading
    } else if failed {
        AttachmentState::Retry
    } else if is_oversized(size_bytes) {
        AttachmentState::Oversized
    } else {
        AttachmentState::Downloading
    }
}

/// Exact port of `media_drain.zig:206-219`.
pub fn attachment_label(name: &str, size_bytes: u64, state: AttachmentState) -> String {
    let display_name = if name.is_empty() { "Attachment" } else { name };
    let state_str = match state {
        AttachmentState::Retry => "Tap to retry",
        AttachmentState::Local => "Open",
        AttachmentState::Downloading => "Downloading...",
        AttachmentState::Oversized => "Click to download",
        AttachmentState::Unavailable => "Not available, ask the sender to send it again",
    };
    if size_bytes == 0 {
        format!("{display_name} \u{00B7} {state_str}")
    } else if size_bytes < 1024 {
        format!("{display_name} \u{00B7} {size_bytes} B \u{00B7} {state_str}")
    } else if size_bytes < 1_048_576 {
        let kb = size_bytes.div_ceil(1024);
        format!("{display_name} \u{00B7} {kb} KB \u{00B7} {state_str}")
    } else {
        let mb = size_bytes / 1_048_576;
        format!("{display_name} \u{00B7} {mb} MB \u{00B7} {state_str}")
    }
}

/// Per-message media download lifecycle status surfaced to `PostboxViewModel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaItemStatus {
    Idle,
    Downloading,
    Ready(PathBuf),
    Oversized,
    RetryPending { attempts: u8, next_retry_ms: u64 },
    Unavailable,
    Gone,
}

impl MediaItemStatus {
    pub fn to_attachment_state(&self) -> AttachmentState {
        match self {
            Self::Ready(_) => AttachmentState::Local,
            Self::Idle | Self::Downloading => AttachmentState::Downloading,
            Self::Oversized => AttachmentState::Oversized,
            Self::RetryPending { .. } => AttachmentState::Retry,
            Self::Unavailable | Self::Gone => AttachmentState::Unavailable,
        }
    }
}

/// Throttles UI repaints during bursts of HTTP `410 Gone` responses to at most once every
/// `20_000 ms` (`<= 3` repaints/minute, `media_drain.zig:111-115`).
#[derive(Debug, Default, Clone)]
pub struct GoneRepaintThrottle {
    dirty: bool,
    last_repaint_ms: Option<u64>,
}

impl GoneRepaintThrottle {
    pub fn note_gone(&mut self) {
        self.dirty = true;
    }

    pub fn should_repaint(&mut self, now_ms: u64) -> bool {
        let gap_elapsed = match self.last_repaint_ms {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= GONE_REPAINT_GAP_MS,
        };
        if self.dirty && gap_elapsed {
            self.dirty = false;
            self.last_repaint_ms = Some(now_ms);
            true
        } else {
            false
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Earliest monotonic timestamp (`ms`) at which a suppressed dirty `410 Gone` burst may flush,
    /// or `None` when clean (guaranteeing 0% idle timer wakeups).
    pub fn next_flush_due_ms(&self) -> Option<u64> {
        if !self.dirty {
            return None;
        }
        match self.last_repaint_ms {
            None => Some(0),
            Some(last) => Some(last.saturating_add(GONE_REPAINT_GAP_MS)),
        }
    }
}

#[derive(Debug, Clone)]
struct FailureRecord {
    attempts: u8,
    retry_after_ms: u64,
}

/// Tracks open-chat newest-first download scheduling, retry backoff, `410` gone suppression,
/// and cache-hit attempt forgetting (`main.zig:7307-7466`).
#[derive(Debug, Default)]
pub struct MediaDrainTracker {
    in_flight: HashSet<String>,
    attempted: HashSet<String>,
    unavailable: HashSet<String>,
    failures: HashMap<String, FailureRecord>,
    gone_throttle: GoneRepaintThrottle,
}

impl MediaDrainTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_in_flight(&self, key: &str) -> bool {
        self.in_flight.contains(key)
    }

    pub fn mark_in_flight(&mut self, key: String) {
        self.in_flight.insert(key);
    }

    pub fn clear_in_flight(&mut self, key: &str) {
        self.in_flight.remove(key);
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn is_gone_dirty(&self) -> bool {
        self.gone_throttle.is_dirty()
    }

    pub fn next_gone_flush_due_ms(&self) -> Option<u64> {
        self.gone_throttle.next_flush_due_ms()
    }

    /// Returns `true` when an automatic download for `key` must be suppressed because it is
    /// already in flight, already attempted/succeeded, permanently unavailable/gone, or still
    /// inside its exponential (`5s*2^k`, max 8) / linear (`30s*n`, max 3) backoff window.
    pub fn should_skip_auto(&self, key: &str, now_ms: u64) -> bool {
        if self.in_flight.contains(key)
            || self.attempted.contains(key)
            || self.unavailable.contains(key)
        {
            return true;
        }
        self.failures
            .get(key)
            .is_some_and(|fail| now_ms < fail.retry_after_ms)
    }

    /// Earliest `retry_after_ms` among non-in-flight `RetryPending` entries (`None` when idle or
    /// all retries are already running -> prevents tight `select!` spin loops).
    pub fn next_retry_due_ms(&self) -> Option<u64> {
        self.failures
            .iter()
            .filter(|(k, _)| {
                !self.in_flight.contains(k.as_str())
                    && !self.attempted.contains(k.as_str())
                    && !self.unavailable.contains(k.as_str())
            })
            .map(|(_, rec)| rec.retry_after_ms)
            .min()
    }

    /// Returns all `RetryPending` cache keys whose backoff window has elapsed
    /// (`retry_after_ms <= now_ms`) and marks them `in_flight` while preserving their
    /// `FailureRecord` attempt counters so subsequent failures advance cleanly toward exhaustion.
    pub fn take_due_retries(&mut self, now_ms: u64) -> Vec<String> {
        let mut due: Vec<String> = self
            .failures
            .iter()
            .filter(|(k, rec)| {
                rec.retry_after_ms <= now_ms
                    && !self.in_flight.contains(k.as_str())
                    && !self.attempted.contains(k.as_str())
                    && !self.unavailable.contains(k.as_str())
            })
            .map(|(k, _)| k.clone())
            .collect();
        due.sort();
        for key in &due {
            self.in_flight.insert(key.clone());
        }
        due
    }

    /// Cache-hit suppression clear (`media_drain.zig:85-93`): when a file resolves on disk,
    /// forget prior attempt so if it's later evicted after 14d it auto-downloads cleanly.
    pub fn forget_attempt(&mut self, key: &str) {
        self.attempted.remove(key);
        self.unavailable.remove(key);
        self.failures.remove(key);
    }

    pub fn note_gone(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.failures.remove(key);
        self.attempted.insert(key.to_owned());
        self.unavailable.insert(key.to_owned());
        self.gone_throttle.note_gone();
    }

    pub fn should_repaint_gone(&mut self, now_ms: u64) -> bool {
        self.gone_throttle.should_repaint(now_ms)
    }

    pub fn note_success(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.failures.remove(key);
        self.unavailable.remove(key);
        self.attempted.insert(key.to_owned());
    }

    /// Records a failure and returns the resulting `MediaItemStatus` (`Gone`, `Unavailable`, or
    /// `RetryPending { attempts, next_retry_ms }`).
    pub fn note_failure(&mut self, key: &str, http_status: u16, now_ms: u64) -> MediaItemStatus {
        self.in_flight.remove(key);
        if http_status == GONE_STATUS_CODE {
            self.note_gone(key);
            return MediaItemStatus::Gone;
        }
        if !self.failures.contains_key(key) && self.failures.len() >= MAX_MEDIA_FAILURES_TABLE {
            self.unavailable.insert(key.to_owned());
            self.attempted.insert(key.to_owned());
            return MediaItemStatus::Unavailable;
        }
        let rec = self
            .failures
            .entry(key.to_owned())
            .or_insert(FailureRecord {
                attempts: 0,
                retry_after_ms: 0,
            });
        rec.attempts = rec.attempts.saturating_add(1);
        match media_retry_delay_ms(http_status, rec.attempts) {
            Some(delay) => {
                rec.retry_after_ms = now_ms.saturating_add(delay);
                MediaItemStatus::RetryPending {
                    attempts: rec.attempts,
                    next_retry_ms: rec.retry_after_ms,
                }
            }
            None => {
                self.failures.remove(key);
                self.unavailable.insert(key.to_owned());
                self.attempted.insert(key.to_owned());
                MediaItemStatus::Unavailable
            }
        }
    }

    /// Selects open-chat auto-download candidates in **newest-first** order (`main.zig:7416-7446`).
    pub fn select_open_chat_candidates<'a>(
        &self,
        messages_chronological: &'a [Message],
        cached_keys: &HashSet<String>,
        gone_msg_ids: &HashSet<String>,
        now_ms: u64,
    ) -> Vec<&'a Message> {
        let mut out = Vec::new();
        for msg in messages_chronological.iter().rev() {
            if !is_downloadable_message(msg) || msg.msg_id.is_empty() {
                continue;
            }
            if gone_msg_ids.contains(&msg.msg_id) {
                continue;
            }
            if let Some(sz) = msg.file_size
                && is_oversized(sz)
            {
                continue;
            }
            let key = media_cache_key(&msg.chat, &msg.msg_id);
            if cached_keys.contains(&key) || self.should_skip_auto(&key, now_ms) {
                continue;
            }
            out.push(msg);
        }
        out
    }
}

/// Cache hit returned by `MediaDownloader::resolve_cached_entry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCacheHit {
    pub path: PathBuf,
    pub mime_type: String,
}

/// Resolves the best MIME type on a disk cache hit, preferring non-empty caller metadata, then
/// the persisted SQLite `media_cache.mime_type`, then `"application/octet-stream"`.
pub fn effective_cached_mime(caller_mime: Option<&str>, cached_mime: &str) -> String {
    caller_mime
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .or_else(|| (!cached_mime.is_empty()).then(|| cached_mime.to_owned()))
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

/// Owned download request for `Send + 'static` spawned downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDownloadSpec {
    pub chat: ChatKey,
    pub msg_id: String,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub media_kind: Option<MediaKind>,
}

/// Outcome of the pure network + disk phase (`Send`-safe, zero `Store` access).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedDownloadResult {
    Ready {
        path: PathBuf,
        cache_entry: MediaCacheEntry,
    },
    Gone,
    Unavailable,
    TransientFailure {
        http_status: u16,
    },
    OtherFailure {
        http_status: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    Ready { path: PathBuf, mime_type: String },
    Gone,
    Unavailable,
    TransientFailure { http_status: u16 },
    OtherFailure { http_status: u16 },
}

/// Async Hub media downloader bounded to `MAX_HUB_CONCURRENT = 3` concurrent slots, performing
/// atomic disk cache writes, extension aliasing, NTFS `Zone.Identifier` ADS tagging, SQLite
/// `media_cache` + `gone_media` persistence, and `202` polling (`1s` up to `120s`).
pub struct MediaDownloader {
    cache_dir: PathBuf,
    slots: Arc<Semaphore>,
    poll_interval: Duration,
    poll_max_wait: Duration,
    stall_timeout: Duration,
}

impl MediaDownloader {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            slots: Arc::new(Semaphore::new(MAX_HUB_CONCURRENT)),
            poll_interval: DEFAULT_MEDIA_POLL_INTERVAL,
            poll_max_wait: DEFAULT_MEDIA_POLL_MAX_WAIT,
            stall_timeout: Duration::from_millis(HUB_MEDIA_TIMEOUT_MS),
        }
    }

    pub fn with_timeouts(
        mut self,
        poll_interval: Duration,
        poll_max_wait: Duration,
        stall_timeout: Duration,
    ) -> Self {
        self.poll_interval = poll_interval;
        self.poll_max_wait = poll_max_wait;
        self.stall_timeout = stall_timeout;
        self
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn max_concurrent_slots(&self) -> usize {
        MAX_HUB_CONCURRENT
    }

    pub fn available_slots(&self) -> usize {
        self.slots.available_permits()
    }

    /// Phase 1 (sync): resolves an already-cached local path + persisted MIME type if the file
    /// still exists on disk, touching `last_accessed_at` or pruning stale SQLite rows.
    pub fn resolve_cached_entry(
        &self,
        store: &Store,
        chat: &ChatKey,
        msg_id: &str,
    ) -> Option<ResolvedCacheHit> {
        let key = media_cache_key(chat, msg_id);
        let entry = store.get_media_cache(&key).ok().flatten()?;
        let full = self.cache_dir.join(&entry.rel_path);
        if full.is_file() {
            let _ = store.touch_media_cache(&key, now_unix_secs());
            crate::age::touch_file_mtime(&full);
            Some(ResolvedCacheHit {
                path: full,
                mime_type: entry.mime_type,
            })
        } else {
            let _ = store.delete_media_cache(&key);
            None
        }
    }

    /// Resolves an already-cached local path for `(chat, msg_id)` if the file still exists on disk.
    pub fn resolve_cached_path(
        &self,
        store: &Store,
        chat: &ChatKey,
        msg_id: &str,
    ) -> Option<PathBuf> {
        self.resolve_cached_entry(store, chat, msg_id)
            .map(|h| h.path)
    }

    /// Phase 2 (`Send + 'static` async): acquires one of the 3 Hub concurrency slots, fetches media
    /// from Hub with `202` polling + stall timeout, and writes bytes atomically to disk via
    /// `spawn_blocking` with zero SQLite access.
    pub async fn download_to_cache_owned(
        self: Arc<Self>,
        client: Arc<HubClient>,
        zone_marker: Option<Arc<dyn ZoneMarker>>,
        spec: MediaDownloadSpec,
    ) -> Result<CachedDownloadResult, MediaError> {
        let _permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Cancelled)?;

        let fetch_fut = client.fetch_media_detailed_polling(
            &spec.chat.id,
            &spec.msg_id,
            self.poll_interval,
            self.poll_max_wait,
        );
        let result = match tokio::time::timeout(self.stall_timeout, fetch_fut).await {
            Ok(res) => res,
            Err(_) => {
                return Ok(CachedDownloadResult::OtherFailure { http_status: 0 });
            }
        };

        match result {
            Ok(detailed) => match detailed.state {
                MediaFetchState::Ready(bytes) => {
                    let effective_mime = detailed
                        .content_type
                        .filter(|m| !m.is_empty())
                        .or_else(|| spec.mime_type.clone().filter(|m| !m.is_empty()))
                        .unwrap_or_else(|| "application/octet-stream".to_owned());
                    let ext = resolve_media_extension(
                        spec.filename.as_deref(),
                        Some(&effective_mime),
                        spec.media_kind,
                    );
                    let byte_size = bytes.len() as u64;
                    let cache_dir = self.cache_dir.clone();
                    let chat = spec.chat.clone();
                    let msg_id = spec.msg_id.clone();
                    let (final_path, rel_path, sha256) = tokio::task::spawn_blocking(move || {
                        let (base_path, final_path, rel_path, sha256) =
                            write_cached_bytes_to_dir(&cache_dir, &chat, &msg_id, &bytes, &ext)?;
                        apply_zone_mark(&base_path, zone_marker.as_deref());
                        if final_path != base_path {
                            apply_zone_mark(&final_path, zone_marker.as_deref());
                        }
                        Ok::<_, MediaError>((final_path, rel_path, sha256))
                    })
                    .await
                    .map_err(|e| MediaError::Io(std::io::Error::other(e.to_string())))??;

                    let cache_entry = MediaCacheEntry {
                        cache_key: media_cache_key(&spec.chat, &spec.msg_id),
                        sha256,
                        rel_path,
                        mime_type: effective_mime,
                        byte_size,
                        last_accessed_at: now_unix_secs(),
                    };
                    Ok(CachedDownloadResult::Ready {
                        path: final_path,
                        cache_entry,
                    })
                }
                MediaFetchState::Pending => {
                    Ok(CachedDownloadResult::OtherFailure { http_status: 202 })
                }
                MediaFetchState::Unavailable => Ok(CachedDownloadResult::Unavailable),
                MediaFetchState::Gone => Ok(CachedDownloadResult::Gone),
            },
            Err(HubError::PartialMedia) => {
                Ok(CachedDownloadResult::OtherFailure { http_status: 206 })
            }
            Err(HubError::HttpStatus { status, .. }) => {
                if status == GONE_STATUS_CODE {
                    Ok(CachedDownloadResult::Gone)
                } else if is_transient_http_status(status) {
                    Ok(CachedDownloadResult::TransientFailure {
                        http_status: status,
                    })
                } else {
                    Ok(CachedDownloadResult::OtherFailure {
                        http_status: status,
                    })
                }
            }
            Err(HubError::Transport(_)) => {
                Ok(CachedDownloadResult::OtherFailure { http_status: 0 })
            }
            Err(other) => Err(MediaError::Hub(other)),
        }
    }

    /// Borrowed `Send` Phase 2 variant (no `Store` held across `.await`).
    pub async fn download_to_cache(
        &self,
        client: &HubClient,
        zone_marker: Option<&dyn ZoneMarker>,
        spec: &MediaDownloadSpec,
    ) -> Result<CachedDownloadResult, MediaError> {
        let _permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Cancelled)?;

        let fetch_fut = client.fetch_media_detailed_polling(
            &spec.chat.id,
            &spec.msg_id,
            self.poll_interval,
            self.poll_max_wait,
        );
        let result = match tokio::time::timeout(self.stall_timeout, fetch_fut).await {
            Ok(res) => res,
            Err(_) => {
                return Ok(CachedDownloadResult::OtherFailure { http_status: 0 });
            }
        };

        match result {
            Ok(detailed) => match detailed.state {
                MediaFetchState::Ready(bytes) => {
                    let effective_mime = detailed
                        .content_type
                        .filter(|m| !m.is_empty())
                        .or_else(|| spec.mime_type.clone().filter(|m| !m.is_empty()))
                        .unwrap_or_else(|| "application/octet-stream".to_owned());
                    let ext = resolve_media_extension(
                        spec.filename.as_deref(),
                        Some(&effective_mime),
                        spec.media_kind,
                    );
                    let byte_size = bytes.len() as u64;
                    let cache_dir = self.cache_dir.clone();
                    let chat = spec.chat.clone();
                    let msg_id = spec.msg_id.clone();
                    let (base_path, final_path, rel_path, sha256) =
                        tokio::task::spawn_blocking(move || {
                            write_cached_bytes_to_dir(&cache_dir, &chat, &msg_id, &bytes, &ext)
                        })
                        .await
                        .map_err(|e| MediaError::Io(std::io::Error::other(e.to_string())))??;
                    apply_zone_mark(&base_path, zone_marker);
                    if final_path != base_path {
                        apply_zone_mark(&final_path, zone_marker);
                    }
                    let cache_entry = MediaCacheEntry {
                        cache_key: media_cache_key(&spec.chat, &spec.msg_id),
                        sha256,
                        rel_path,
                        mime_type: effective_mime,
                        byte_size,
                        last_accessed_at: now_unix_secs(),
                    };
                    Ok(CachedDownloadResult::Ready {
                        path: final_path,
                        cache_entry,
                    })
                }
                MediaFetchState::Pending => {
                    Ok(CachedDownloadResult::OtherFailure { http_status: 202 })
                }
                MediaFetchState::Unavailable => Ok(CachedDownloadResult::Unavailable),
                MediaFetchState::Gone => Ok(CachedDownloadResult::Gone),
            },
            Err(HubError::PartialMedia) => {
                Ok(CachedDownloadResult::OtherFailure { http_status: 206 })
            }
            Err(HubError::HttpStatus { status, .. }) => {
                if status == GONE_STATUS_CODE {
                    Ok(CachedDownloadResult::Gone)
                } else if is_transient_http_status(status) {
                    Ok(CachedDownloadResult::TransientFailure {
                        http_status: status,
                    })
                } else {
                    Ok(CachedDownloadResult::OtherFailure {
                        http_status: status,
                    })
                }
            }
            Err(HubError::Transport(_)) => {
                Ok(CachedDownloadResult::OtherFailure { http_status: 0 })
            }
            Err(other) => Err(MediaError::Hub(other)),
        }
    }

    /// Phase 3 (sync): commits a `CachedDownloadResult` into SQLite (`media_cache` / `gone_media`)
    /// and maps it to a `DownloadOutcome`.
    pub fn commit_download_result(
        store: &Store,
        chat: &ChatKey,
        msg_id: &str,
        result: CachedDownloadResult,
    ) -> Result<DownloadOutcome, MediaError> {
        match result {
            CachedDownloadResult::Ready { path, cache_entry } => {
                store.upsert_media_cache(&cache_entry)?;
                Ok(DownloadOutcome::Ready {
                    path,
                    mime_type: cache_entry.mime_type,
                })
            }
            CachedDownloadResult::Gone => {
                store.mark_media_gone(chat, msg_id, now_unix_secs())?;
                Ok(DownloadOutcome::Gone)
            }
            CachedDownloadResult::Unavailable => Ok(DownloadOutcome::Unavailable),
            CachedDownloadResult::TransientFailure { http_status } => {
                Ok(DownloadOutcome::TransientFailure { http_status })
            }
            CachedDownloadResult::OtherFailure { http_status } => {
                Ok(DownloadOutcome::OtherFailure { http_status })
            }
        }
    }

    /// Downloads a single message's media through the 3-slot semaphore, handling `200`, `202`
    /// polling, `206` rejection, `404`, `410` permanent SQLite persistence, and `429`/`5xx`.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_one(
        &self,
        client: &HubClient,
        store: &Store,
        zone_marker: Option<&dyn ZoneMarker>,
        chat: &ChatKey,
        msg_id: &str,
        filename: Option<&str>,
        mime_type: Option<&str>,
        media_kind: Option<MediaKind>,
        automatic: bool,
    ) -> Result<DownloadOutcome, MediaError> {
        if automatic && store.is_media_gone(chat, msg_id)? {
            return Ok(DownloadOutcome::Gone);
        }
        if let Some(hit) = self.resolve_cached_entry(store, chat, msg_id) {
            return Ok(DownloadOutcome::Ready {
                path: hit.path,
                mime_type: effective_cached_mime(mime_type, &hit.mime_type),
            });
        }
        let spec = MediaDownloadSpec {
            chat: chat.clone(),
            msg_id: msg_id.to_owned(),
            filename: filename.map(str::to_owned),
            mime_type: mime_type.map(str::to_owned),
            media_kind,
        };
        let cached = self.download_to_cache(client, zone_marker, &spec).await?;
        Self::commit_download_result(store, chat, msg_id, cached)
    }
}

fn write_cached_bytes_to_dir(
    cache_dir: &Path,
    chat: &ChatKey,
    msg_id: &str,
    bytes: &[u8],
    ext: &str,
) -> Result<(PathBuf, PathBuf, String, String), MediaError> {
    std::fs::create_dir_all(cache_dir)?;
    let key = media_cache_key(chat, msg_id);
    let base_name = format!("{key}.bin");
    let base_path = cache_dir.join(&base_name);
    atomic_write_file(&base_path, bytes)?;

    let sha256 = hex_encode(&Sha256::digest(bytes));

    if ext != ".bin" {
        let alias_name = format!("{key}.bin{ext}");
        let alias_path = cache_dir.join(&alias_name);
        let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_alias = cache_dir.join(format!("{alias_name}.{seq:x}.tmp"));
        let staged = std::fs::hard_link(&base_path, &tmp_alias).is_ok()
            || std::fs::copy(&base_path, &tmp_alias).is_ok();
        let mut alias_ready = false;
        if staged {
            if std::fs::rename(&tmp_alias, &alias_path).is_ok() {
                alias_ready = true;
            } else {
                // On Windows, `rename` fails when `alias_path` already exists; remove stale alias and retry.
                let _ = std::fs::remove_file(&alias_path);
                if std::fs::rename(&tmp_alias, &alias_path).is_ok() {
                    alias_ready = true;
                } else {
                    let _ = std::fs::remove_file(&tmp_alias);
                }
            }
        } else {
            let _ = std::fs::remove_file(&tmp_alias);
        }
        if alias_ready {
            return Ok((base_path, alias_path, alias_name, sha256));
        }
    }
    Ok((base_path.clone(), base_path, base_name, sha256))
}

fn apply_zone_mark(path: &Path, zone_marker: Option<&dyn ZoneMarker>) {
    if let Some(marker) = zone_marker {
        let _ = marker.mark_downloaded_file(path);
    } else {
        let _ = mark_downloaded_file_zone(path);
    }
}

/// Atomic file write: writes `<path>.<seq>.tmp`, flushes, and renames over `<path>`.
pub fn atomic_write_file(target: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp = target.as_os_str().to_os_string();
    tmp.push(format!(".{seq:x}.tmp"));
    let tmp_path = PathBuf::from(tmp);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(err) = std::fs::rename(&tmp_path, target) {
        // On Windows `rename` can fail if target exists; remove and retry cleanly.
        let _ = std::fs::remove_file(target);
        if std::fs::rename(&tmp_path, target).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    Ok(())
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::{Provider, ReadStatus};

    #[test]
    fn downloadable_placeholder_oversized_and_labels_match_spec() {
        for raw in [
            "image", "sticker", "video", "gif", "audio", "voice", "document", "IMAGE", "Audio",
        ] {
            assert!(is_downloadable_type_str(raw), "expected {raw} downloadable");
        }
        assert!(!is_downloadable_type_str("text"));

        assert!(is_placeholder(Some(MediaKind::Document), "Sent document"));
        assert!(is_placeholder(Some(MediaKind::Audio), "Sent AUDIO"));
        assert!(is_placeholder(Some(MediaKind::Image), "[Image]"));
        assert!(!is_placeholder(
            Some(MediaKind::Document),
            "Sent document yesterday"
        ));
        assert!(!is_placeholder(Some(MediaKind::Video), "holiday.mp4"));

        // Exact 100 MiB boundary
        assert!(!is_oversized(104_857_600));
        assert!(is_oversized(104_857_601));

        // Exact attachmentLabel vectors from media_drain.zig:325-337
        assert_eq!(
            attachment_label("Voice message", 0, AttachmentState::Retry),
            "Voice message · Tap to retry"
        );
        assert_eq!(
            attachment_label("report.pdf", 1500, AttachmentState::Local),
            "report.pdf · 2 KB · Open"
        );
        assert_eq!(
            attachment_label("report.pdf", 101 * 1024 * 1024, AttachmentState::Oversized),
            "report.pdf · 101 MB · Click to download"
        );
        assert_eq!(
            attachment_label("", 0, AttachmentState::Unavailable),
            "Attachment · Not available, ask the sender to send it again"
        );
        assert_eq!(
            voice_state(false, false, false, 104_857_601),
            AttachmentState::Oversized
        );
        assert_eq!(
            voice_state(false, false, false, 104_857_600),
            AttachmentState::Downloading
        );
    }

    #[test]
    fn retry_backoff_and_gone_repaint_throttle() {
        // Transient 429 / 5xx: 5s, 10s, 20s, 40s, 60s, 60s, 60s, then exhausted at 8
        assert_eq!(media_retry_delay_ms(429, 1), Some(5_000));
        assert_eq!(media_retry_delay_ms(503, 2), Some(10_000));
        assert_eq!(media_retry_delay_ms(500, 3), Some(20_000));
        assert_eq!(media_retry_delay_ms(502, 4), Some(40_000));
        assert_eq!(media_retry_delay_ms(502, 5), Some(60_000));
        assert_eq!(media_retry_delay_ms(502, 7), Some(60_000));
        assert_eq!(media_retry_delay_ms(502, 8), None);

        // Non-transient: 30s, 60s, then exhausted at 3
        assert_eq!(media_retry_delay_ms(404, 1), Some(30_000));
        assert_eq!(media_retry_delay_ms(404, 2), Some(60_000));
        assert_eq!(media_retry_delay_ms(404, 3), None);

        // 410 Gone: 0 retries
        assert_eq!(media_retry_delay_ms(410, 1), None);

        // 20s gone repaint throttle
        let mut throttle = GoneRepaintThrottle::default();
        throttle.note_gone();
        assert!(throttle.should_repaint(10_000));
        throttle.note_gone();
        assert!(!throttle.should_repaint(25_000));
        assert!(throttle.should_repaint(30_000));
    }

    #[test]
    fn open_chat_selects_newest_first_and_respects_100mib_and_gone() {
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let mk = |id: &str, kind: MediaKind, size: Option<u64>, deleted: bool| Message {
            chat: chat.clone(),
            msg_id: id.into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:00:00Z".into(),
            body: String::new(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(kind),
            mime_type: None,
            filename: None,
            file_size: size,
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted,
        };
        let msgs = vec![
            mk("m1", MediaKind::Image, Some(1024), false),
            mk("m2", MediaKind::Document, Some(104_857_601), false), // oversized -> skipped
            mk("m3", MediaKind::Audio, Some(104_857_600), false),    // exact 100 MiB -> included!
            mk("m4", MediaKind::Video, Some(2048), true),            // deleted -> skipped
            mk("m5", MediaKind::Voice, Some(4096), false),           // gone -> skipped
            mk("m6", MediaKind::Sticker, Some(512), false),          // newest!
        ];
        let tracker = MediaDrainTracker::new();
        let gone: HashSet<String> = ["m5".to_owned()].into_iter().collect();
        let selected = tracker.select_open_chat_candidates(&msgs, &HashSet::new(), &gone, 1000);
        let ids: Vec<&str> = selected.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m6", "m3", "m1"]);
    }

    #[test]
    fn download_to_cache_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_send_static<T: Send + 'static>(_: &T) {}

        let downloader = Arc::new(MediaDownloader::new(std::env::temp_dir()));
        let endpoint = postbox_hub::Endpoint::parse("http://127.0.0.1:9999", true).unwrap();
        let client = Arc::new(HubClient::new(endpoint, "tok").unwrap());
        let spec = MediaDownloadSpec {
            chat: ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net"),
            msg_id: "m1".into(),
            filename: None,
            mime_type: None,
            media_kind: Some(MediaKind::Image),
        };
        let fut_borrowed = downloader.download_to_cache(&client, None, &spec);
        assert_send(&fut_borrowed);
        drop(fut_borrowed);
        let fut_owned = downloader
            .clone()
            .download_to_cache_owned(client.clone(), None, spec);
        assert_send_static(&fut_owned);
    }

    #[test]
    fn gone_repaint_throttle_enforces_3_per_min_starting_at_zero() {
        let mut throttle = GoneRepaintThrottle::default();
        let mut repaints = Vec::new();
        for t in (0..60_000).step_by(500) {
            throttle.note_gone();
            if throttle.should_repaint(t) {
                repaints.push(t);
            }
            if t == 500 {
                assert_eq!(throttle.next_flush_due_ms(), Some(20_000));
            }
        }
        assert_eq!(repaints, vec![0, 20_000, 40_000]);
        assert!(throttle.is_dirty());
        assert!(throttle.should_repaint(60_000));
        assert!(!throttle.is_dirty());
        assert_eq!(throttle.next_flush_due_ms(), None);
    }

    #[test]
    fn drain_tracker_backoff_exhaustion_and_take_due_retries() {
        let mut tracker = MediaDrainTracker::new();
        let k_transient = "transient_key";
        let mut now = 1_000u64;
        let expected_delays = [5_000u64, 10_000, 20_000, 40_000, 60_000, 60_000, 60_000];
        for (i, &delay) in expected_delays.iter().enumerate() {
            let st = tracker.note_failure(k_transient, 429, now);
            assert_eq!(
                st,
                MediaItemStatus::RetryPending {
                    attempts: (i + 1) as u8,
                    next_retry_ms: now + delay,
                }
            );
            assert!(tracker.should_skip_auto(k_transient, now + delay - 1));
            assert!(tracker.take_due_retries(now + delay - 1).is_empty());
            assert_eq!(tracker.next_retry_due_ms(), Some(now + delay));

            let due = tracker.take_due_retries(now + delay);
            assert_eq!(due, vec![k_transient.to_owned()]);
            assert!(tracker.is_in_flight(k_transient));
            assert_eq!(tracker.next_retry_due_ms(), None);
            now += delay;
        }
        // 8th failure exhausts into Unavailable
        assert_eq!(
            tracker.note_failure(k_transient, 429, now),
            MediaItemStatus::Unavailable
        );
        assert!(tracker.should_skip_auto(k_transient, now + 999_999));
        assert_eq!(tracker.next_retry_due_ms(), None);

        // Non-transient 404: 30s -> 60s -> Unavailable on 3rd
        let k_missing = "missing_key";
        assert_eq!(
            tracker.note_failure(k_missing, 404, 10_000),
            MediaItemStatus::RetryPending {
                attempts: 1,
                next_retry_ms: 40_000
            }
        );
        assert_eq!(tracker.take_due_retries(40_000), vec![k_missing.to_owned()]);
        assert_eq!(
            tracker.note_failure(k_missing, 404, 40_000),
            MediaItemStatus::RetryPending {
                attempts: 2,
                next_retry_ms: 100_000
            }
        );
        assert_eq!(
            tracker.take_due_retries(100_000),
            vec![k_missing.to_owned()]
        );
        assert_eq!(
            tracker.note_failure(k_missing, 404, 100_000),
            MediaItemStatus::Unavailable
        );
        assert!(tracker.should_skip_auto(k_missing, 999_999));
    }
}
