use crate::MediaError;
use futures_util::StreamExt;
use postbox_core::ChatKey;
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::redirect::Policy;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::watch;

pub const GIPHY_GRID_COLUMNS: usize = 4;
pub const GIPHY_GRID_ROWS: usize = 3;
pub const GIPHY_DEFAULT_LIMIT: usize = 25;
pub const GIPHY_MAX_RESULTS: usize = 25;
pub const GIPHY_MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const GIPHY_MAX_PREVIEW_BYTES: usize = 4 * 1024 * 1024;
pub const GIPHY_MAX_ORIGINAL_BYTES: usize = 25 * 1024 * 1024;
pub const GIPHY_MAX_QUERY_BYTES: usize = 200;
pub const GIPHY_MAX_QUERY_CODEPOINTS: usize = 50;
pub const GIPHY_MAX_KEY_BYTES: usize = 256;
pub const GIPHY_MAX_OFFSET: u32 = 4_999;
pub const GIPHY_MAX_RETRY_AFTER_SECS: u64 = 86_400;
pub const GIPHY_MAX_MEDIA_URL_LEN: usize = 512;
pub const GIPHY_MAX_DIMENSION: u32 = 4_096;

pub const GIPHY_CUE_TEXT: &str = "Search GIPHY";
pub const GIPHY_MSG_NEEDS_KEY: &str = "GIF search needs a GIPHY key";
pub const GIPHY_MSG_REFUSED_KEY: &str = "GIPHY refused the key. Check it, then try again";
pub const GIPHY_MSG_RATE_LIMITED: &str = "GIPHY is busy. Wait a moment, then press Enter to retry";
pub const GIPHY_MSG_QUERY_TOO_LONG: &str = "GIF search text is too long";
pub const GIPHY_MSG_TOO_LARGE: &str = "That GIF is too large to send";
pub const GIPHY_MSG_SEARCH_FAILED: &str = "GIF search failed. Press Enter to retry";
pub const GIPHY_MSG_DOWNLOAD_FAILED: &str = "GIF download failed. Press Enter to retry";
pub const GIPHY_MSG_DOWNLOADING: &str = "Downloading GIF...";
pub const GIPHY_MSG_TARGET_CHANGED: &str = "GIF dropped because the chat or reply changed";
pub const GIPHY_MSG_SAVE_FAILED: &str = "Could not save the GIF";
pub const GIPHY_MSG_ATTACH_FAILED: &str = "Could not attach the GIF";

/// 5-field target guard (`provider`, `account`, `chat_id`, `reply_to_id`, `reply_sender`) ensuring
/// a GIF finishing download can NEVER stage into a different chat or reply thread
/// (`giphy_catalogue.zig:215-274`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiphyTarget {
    pub chat: ChatKey,
    pub reply_to_id: String,
    pub reply_sender: String,
}

impl GiphyTarget {
    pub fn new(chat: ChatKey, reply_to_id: Option<&str>, reply_sender: Option<&str>) -> Self {
        Self {
            chat,
            reply_to_id: reply_to_id.unwrap_or("").chars().take(191).collect(),
            reply_sender: reply_sender.unwrap_or("").chars().take(191).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiphyRendition {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiphyItem {
    pub id: String,
    pub title: String,
    pub preview: GiphyRendition,
    pub original: GiphyRendition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiphyPage {
    pub items: Vec<GiphyItem>,
    pub total_count: u64,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiphyStatus {
    Closed,
    NeedsKey,
    Loading,
    Ready,
    Failed,
    Staged,
}

/// Complete 4×3 Giphy picker state machine with generation-based cancellation & 5-field target
/// race guard (`giphy_catalogue.zig:215-274`, `main.zig:13695-13980`).
#[derive(Debug, Clone)]
pub struct GiphyPickerState {
    pub generation: u64,
    pub download_generation: u64,
    pub active_download_generation: Option<u64>,
    pub status: GiphyStatus,
    pub target: Option<GiphyTarget>,
    pub query: String,
    pub submitted_query: String,
    pub items: Vec<GiphyItem>,
    pub selected: usize,
    pub error_message: Option<String>,
    pub downloading_index: Option<usize>,
}

impl Default for GiphyPickerState {
    fn default() -> Self {
        Self {
            generation: 0,
            download_generation: 0,
            active_download_generation: None,
            status: GiphyStatus::Closed,
            target: None,
            query: String::new(),
            submitted_query: String::new(),
            items: Vec::new(),
            selected: 0,
            error_message: None,
            downloading_index: None,
        }
    }
}

impl GiphyPickerState {
    pub fn is_open(&self) -> bool {
        self.status != GiphyStatus::Closed
    }

    pub fn begin(&mut self, target: GiphyTarget, has_key: bool, query: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.target = Some(target);
        self.items.clear();
        self.selected = 0;
        self.downloading_index = None;
        self.active_download_generation = None;
        self.submitted_query = query.trim_matches(' ').to_owned();
        self.query = self.submitted_query.clone();
        if has_key {
            self.status = GiphyStatus::Loading;
            self.error_message = None;
        } else {
            self.status = GiphyStatus::NeedsKey;
            self.error_message = Some(GIPHY_MSG_NEEDS_KEY.to_owned());
        }
        self.generation
    }

    pub fn accept(&mut self, generation: u64, target: &GiphyTarget, page: GiphyPage) -> bool {
        if self.status != GiphyStatus::Loading
            || self.generation != generation
            || self.target.as_ref() != Some(target)
        {
            return false;
        }
        self.items = page.items;
        self.selected = 0;
        self.status = GiphyStatus::Ready;
        self.error_message = None;
        true
    }

    pub fn fail(&mut self, generation: u64, target: &GiphyTarget, message: String) -> bool {
        if self.status != GiphyStatus::Loading
            || self.generation != generation
            || self.target.as_ref() != Some(target)
        {
            return false;
        }
        self.status = GiphyStatus::Failed;
        self.error_message = Some(message);
        true
    }

    /// Marks `index` as actively downloading and mints a fresh monotonic `download_generation`
    /// (never reset on cancel) so superseded or ABA tile callbacks can never clobber the active
    /// download.
    pub fn begin_download(&mut self, index: usize) -> Option<u64> {
        if self.status != GiphyStatus::Ready || index >= self.items.len() {
            return None;
        }
        self.download_generation = self.download_generation.wrapping_add(1);
        if self.download_generation == 0 {
            self.download_generation = 1;
        }
        let dl_gen = self.download_generation;
        self.selected = index;
        self.downloading_index = Some(index);
        self.active_download_generation = Some(dl_gen);
        Some(dl_gen)
    }

    /// Returns whether `(generation, index, download_generation)` matches the currently active
    /// tile download on an open picker.
    pub fn is_active_download(
        &self,
        generation: u64,
        index: usize,
        download_generation: u64,
    ) -> bool {
        self.is_open()
            && self.generation == generation
            && self.downloading_index == Some(index)
            && self.active_download_generation == Some(download_generation)
    }

    /// Clears the active download state only if `(generation, index, download_generation)` matches
    /// the in-flight download, returning `true` when the error applies to the current tile.
    pub fn fail_download(
        &mut self,
        generation: u64,
        index: usize,
        download_generation: u64,
    ) -> bool {
        if !self.is_active_download(generation, index, download_generation) {
            return false;
        }
        self.downloading_index = None;
        self.active_download_generation = None;
        true
    }

    pub fn select_for_staging(
        &mut self,
        generation: u64,
        live_target: Option<&GiphyTarget>,
        index: usize,
    ) -> Result<GiphyItem, &'static str> {
        if self.status != GiphyStatus::Ready || self.generation != generation {
            return Err(GIPHY_MSG_DOWNLOAD_FAILED);
        }
        if live_target.is_none() || live_target != self.target.as_ref() {
            self.cancel();
            return Err(GIPHY_MSG_TARGET_CHANGED);
        }
        if index >= self.items.len() {
            return Err(GIPHY_MSG_DOWNLOAD_FAILED);
        }
        let chosen = self.items[index].clone();
        self.items.clear();
        self.downloading_index = None;
        self.active_download_generation = None;
        self.status = GiphyStatus::Staged;
        Ok(chosen)
    }

    /// Cancels and closes picker WITHOUT resetting `generation` or `download_generation` so late
    /// callbacks can never match.
    pub fn cancel(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.downloading_index = None;
        self.active_download_generation = None;
        self.error_message = None;
        self.target = None;
        self.status = GiphyStatus::Closed;
    }

    pub fn move_grid(&mut self, delta_rows: isize, delta_cols: isize) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        let delta = delta_rows * (GIPHY_GRID_COLUMNS as isize) + delta_cols;
        let next = (self.selected as isize + delta).clamp(0, (self.items.len() - 1) as isize);
        self.selected = next as usize;
    }

    /// Returns `(start_index, visible_items)` for the 4×3 viewport tracking `self.selected`
    /// across all up-to-25 results (mirrors `EmojiPickerState::visible_slice`).
    pub fn visible_slice(&self) -> (usize, &[GiphyItem]) {
        let page_size = GIPHY_GRID_COLUMNS * GIPHY_GRID_ROWS;
        let sel_row = self.selected / GIPHY_GRID_COLUMNS;
        let start_row = sel_row.saturating_sub(GIPHY_GRID_ROWS - 1);
        let start = (start_row * GIPHY_GRID_COLUMNS).min(self.items.len());
        let end = (start + page_size).min(self.items.len());
        (start, &self.items[start..end])
    }

    /// Exact empty / status overlay text (`main.zig:13725-13732`).
    pub fn empty_state_text(&self) -> &'static str {
        match self.status {
            GiphyStatus::Loading => "Searching GIPHY...",
            GiphyStatus::Failed => "Search failed. Press Enter to retry",
            GiphyStatus::Ready if self.items.is_empty() => "No GIFs found",
            _ => "Type a search, then press Enter",
        }
    }

    /// Returns whether pressing `Enter` should trigger a new search vs download the selected tile
    /// (`main.zig:13878-13890`).
    pub fn enter_should_search(&self) -> bool {
        let trimmed = self.query.trim_matches(' ');
        trimmed != self.submitted_query
            || matches!(self.status, GiphyStatus::Failed | GiphyStatus::NeedsKey)
    }
}

/// Validates a GIPHY GIF ID (`1..=64` ASCII alphanumeric only, `giphy_catalogue.zig:134-142`).
pub fn valid_giphy_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Builds a validated GIPHY trending or search URL (`giphy_catalogue.zig:54-70`).
pub fn build_giphy_api_url(
    base_url: &str,
    api_key: &str,
    query: &str,
    limit: usize,
    offset: u32,
) -> Result<String, MediaError> {
    if api_key.is_empty() {
        return Err(MediaError::NeedsApiKey);
    }
    if api_key.len() > GIPHY_MAX_KEY_BYTES {
        return Err(MediaError::InvalidRequest("key_too_long".into()));
    }
    if limit == 0 || limit > GIPHY_MAX_RESULTS {
        return Err(MediaError::InvalidRequest("invalid_limit".into()));
    }
    if offset > GIPHY_MAX_OFFSET || u64::from(offset) + limit as u64 > 5_000 {
        return Err(MediaError::InvalidRequest("invalid_offset".into()));
    }
    if query.len() > GIPHY_MAX_QUERY_BYTES || query.chars().count() > GIPHY_MAX_QUERY_CODEPOINTS {
        return Err(MediaError::InvalidRequest(GIPHY_MSG_QUERY_TOO_LONG.into()));
    }

    let base = base_url.trim_end_matches('/');
    let enc_key = postbox_hub::encode_uri_component(api_key);
    if query.is_empty() {
        Ok(format!(
            "{base}/v1/gifs/trending?api_key={enc_key}&limit={limit}&offset={offset}&rating=pg-13"
        ))
    } else {
        let enc_q = postbox_hub::encode_uri_component(query);
        Ok(format!(
            "{base}/v1/gifs/search?api_key={enc_key}&q={enc_q}&limit={limit}&offset={offset}&rating=pg-13"
        ))
    }
}

/// Strict GIPHY CDN host & URL allowlist (`giphy_catalogue.zig:134-179`):
/// - length `<= 512`, valid `gif_id`
/// - `https://` ONLY, NO userinfo, NO explicit `:port`, NO `#fragment`
/// - host MUST be `media.giphy.com` or `media0..4.giphy.com`
/// - path `/media/[v1.<token>/]<gif_id>/<filename>.gif` (no `..`, filename `> 4` chars)
/// - query `%XX` escapes MUST NOT decode to control bytes `< 32`, `127`, or `\`!
pub fn validate_giphy_cdn_url(url_str: &str, gif_id: &str) -> Result<(), MediaError> {
    if url_str.len() > GIPHY_MAX_MEDIA_URL_LEN || !valid_giphy_id(gif_id) {
        return Err(MediaError::InvalidUrl);
    }
    let Some(rest) = url_str.strip_prefix("https://") else {
        return Err(MediaError::InvalidUrl);
    };
    if rest.contains('#') {
        return Err(MediaError::InvalidUrl);
    }
    let path_start = rest.find('/').ok_or(MediaError::InvalidUrl)?;
    let authority = &rest[..path_start];
    if authority.contains('@') || authority.contains(':') {
        return Err(MediaError::InvalidUrl);
    }
    let allowed_host = matches!(
        authority,
        "media.giphy.com"
            | "media0.giphy.com"
            | "media1.giphy.com"
            | "media2.giphy.com"
            | "media3.giphy.com"
            | "media4.giphy.com"
    );
    if !allowed_host {
        return Err(MediaError::InvalidUrl);
    }

    let path_and_query = &rest[path_start..];
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };

    let after_media = path.strip_prefix("/media/").ok_or(MediaError::InvalidUrl)?;
    if !after_media.ends_with(".gif") {
        return Err(MediaError::InvalidUrl);
    }
    let segments: Vec<&str> = after_media.split('/').collect();
    let (id_seg, file_seg) = match segments.as_slice() {
        [id_s, file_s] => (*id_s, *file_s),
        [track_s, id_s, file_s] => {
            let tok = track_s.strip_prefix("v1.").ok_or(MediaError::InvalidUrl)?;
            if tok.is_empty()
                || !tok
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(MediaError::InvalidUrl);
            }
            (*id_s, *file_s)
        }
        _ => return Err(MediaError::InvalidUrl),
    };
    if id_seg != gif_id {
        return Err(MediaError::InvalidUrl);
    }
    if file_seg.len() <= 4
        || file_seg.contains("..")
        || !file_seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(MediaError::InvalidUrl);
    }

    if let Some(q) = query {
        let qb = q.as_bytes();
        let mut i = 0usize;
        while i < qb.len() {
            if qb[i] == b'%' {
                if i + 2 >= qb.len() {
                    return Err(MediaError::InvalidUrl);
                }
                let hi = hex_nibble(qb[i + 1]).ok_or(MediaError::InvalidUrl)?;
                let lo = hex_nibble(qb[i + 2]).ok_or(MediaError::InvalidUrl)?;
                let decoded = (hi << 4) | lo;
                if decoded < 32 || decoded == 127 || decoded == b'\\' {
                    return Err(MediaError::InvalidUrl);
                }
                i += 3;
            } else {
                let c = qb[i];
                let ok = c.is_ascii_alphanumeric()
                    || matches!(
                        c,
                        b'-' | b'.'
                            | b'_'
                            | b'~'
                            | b'!'
                            | b'$'
                            | b'&'
                            | b'\''
                            | b'('
                            | b')'
                            | b'*'
                            | b'+'
                            | b','
                            | b';'
                            | b'='
                            | b':'
                            | b'@'
                            | b'/'
                            | b'?'
                    );
                if !ok {
                    return Err(MediaError::InvalidUrl);
                }
                i += 1;
            }
        }
    }
    Ok(())
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Validates GIF87a / GIF89a magic bytes (`giphy_http_win.zig:183-186`).
pub fn validate_gif_magic(bytes: &[u8]) -> Result<(), MediaError> {
    if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        Ok(())
    } else {
        Err(MediaError::NotGif)
    }
}

/// Parses `Retry-After` header strictly as ASCII digits capped at `86_400` seconds
/// (`giphy_http_win.zig:240-260`).
pub fn parse_retry_after_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let val = trimmed.parse::<u64>().ok()?;
    Some(val.min(GIPHY_MAX_RETRY_AFTER_SECS))
}

fn parse_strict_json_uint(val: &Value) -> Option<u64> {
    let num = val.as_number()?;
    let raw = num.to_string();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok()
}

fn parse_strict_string_uint(val: &Value) -> Option<u64> {
    let s = val.as_str()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn parse_rendition(val: &Value, gif_id: &str) -> Result<GiphyRendition, MediaError> {
    let obj = val
        .as_object()
        .ok_or_else(|| MediaError::DecodeFailed("missing rendition object".into()))?;
    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| MediaError::DecodeFailed("missing rendition url".into()))?;
    validate_giphy_cdn_url(url, gif_id)?;
    let width = obj
        .get("width")
        .and_then(parse_strict_string_uint)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| (1..=GIPHY_MAX_DIMENSION).contains(&v))
        .ok_or_else(|| MediaError::DecodeFailed("invalid rendition width".into()))?;
    let height = obj
        .get("height")
        .and_then(parse_strict_string_uint)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| (1..=GIPHY_MAX_DIMENSION).contains(&v))
        .ok_or_else(|| MediaError::DecodeFailed("invalid rendition height".into()))?;
    let size = obj
        .get("size")
        .and_then(parse_strict_string_uint)
        .filter(|&v| (1..=GIPHY_MAX_ORIGINAL_BYTES as u64).contains(&v))
        .ok_or_else(|| MediaError::DecodeFailed("invalid rendition size".into()))?;
    Ok(GiphyRendition {
        url: url.to_owned(),
        width,
        height,
        size,
    })
}

/// Strictly parses a GIPHY API JSON page (`giphy_catalogue.zig:92-213`).
pub fn parse_giphy_page(
    bytes: &[u8],
    requested_limit: usize,
    expected_offset: u32,
) -> Result<GiphyPage, MediaError> {
    if bytes.len() > GIPHY_MAX_RESPONSE_BYTES {
        return Err(MediaError::FileTooLarge);
    }
    let root: Value =
        serde_json::from_slice(bytes).map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let meta_status = root
        .get("meta")
        .and_then(|m| m.get("status"))
        .and_then(parse_strict_json_uint)
        .ok_or_else(|| MediaError::DecodeFailed("invalid meta.status".into()))?;
    if meta_status != 200 {
        return Err(MediaError::HttpStatus(meta_status as u16));
    }

    let pagination = root
        .get("pagination")
        .and_then(Value::as_object)
        .ok_or_else(|| MediaError::DecodeFailed("missing pagination".into()))?;
    let total_count = pagination
        .get("total_count")
        .and_then(parse_strict_json_uint)
        .ok_or_else(|| MediaError::DecodeFailed("invalid pagination.total_count".into()))?;
    let count = pagination
        .get("count")
        .and_then(parse_strict_json_uint)
        .ok_or_else(|| MediaError::DecodeFailed("invalid pagination.count".into()))?;
    let offset = pagination
        .get("offset")
        .and_then(parse_strict_json_uint)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| MediaError::DecodeFailed("invalid pagination.offset".into()))?;

    let data = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| MediaError::DecodeFailed("missing data array".into()))?;
    if data.len() > requested_limit || count != data.len() as u64 || offset != expected_offset {
        return Err(MediaError::DecodeFailed("pagination mismatch".into()));
    }

    let mut seen_ids = HashSet::with_capacity(data.len());
    let mut items = Vec::with_capacity(data.len());
    for entry in data {
        let obj = entry
            .as_object()
            .ok_or_else(|| MediaError::DecodeFailed("non-object gif item".into()))?;
        if obj.get("type").and_then(Value::as_str) != Some("gif") {
            return Err(MediaError::DecodeFailed("non-gif item type".into()));
        }
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| valid_giphy_id(s))
            .ok_or_else(|| MediaError::DecodeFailed("invalid gif id".into()))?;
        if !seen_ids.insert(id.to_owned()) {
            return Err(MediaError::DecodeFailed("duplicate gif id".into()));
        }
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let images = obj
            .get("images")
            .and_then(Value::as_object)
            .ok_or_else(|| MediaError::DecodeFailed("missing images".into()))?;
        let preview = parse_rendition(
            images
                .get("fixed_width")
                .ok_or_else(|| MediaError::DecodeFailed("missing fixed_width".into()))?,
            id,
        )?;
        let original = parse_rendition(
            images
                .get("original")
                .ok_or_else(|| MediaError::DecodeFailed("missing original".into()))?,
            id,
        )?;
        items.push(GiphyItem {
            id: id.to_owned(),
            title,
            preview,
            original,
        });
    }

    Ok(GiphyPage {
        items,
        total_count,
        offset,
    })
}

/// Maps `MediaError` to exact user-facing GIPHY status strings (`main.zig:13640-13648`).
pub fn giphy_error_message(err: &MediaError, is_download: bool) -> &'static str {
    match err {
        MediaError::NeedsApiKey => GIPHY_MSG_NEEDS_KEY,
        MediaError::Unauthorized | MediaError::Forbidden => GIPHY_MSG_REFUSED_KEY,
        MediaError::RateLimited { .. } => GIPHY_MSG_RATE_LIMITED,
        MediaError::InvalidRequest(_) => GIPHY_MSG_QUERY_TOO_LONG,
        MediaError::FileTooLarge | MediaError::NotGif if is_download => GIPHY_MSG_TOO_LARGE,
        _ if is_download => GIPHY_MSG_DOWNLOAD_FAILED,
        _ => GIPHY_MSG_SEARCH_FAILED,
    }
}

/// Cancellable async GIPHY HTTP client (`Policy::none()` — zero redirects, strict size caps,
/// `GIF87a`/`GIF89a` magic byte check, optional loopback test port override).
#[derive(Clone)]
pub struct GiphyClient {
    api_base: String,
    loopback_rewrite_port: Option<u16>,
    client: reqwest::Client,
}

impl GiphyClient {
    pub fn new() -> Result<Self, MediaError> {
        let loopback_port = std::env::var("POSTBOX_GIPHY_PORT")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|&p| p > 0);
        Self::with_loopback_port(loopback_port)
    }

    pub fn with_loopback_port(loopback_port: Option<u16>) -> Result<Self, MediaError> {
        let mut builder = reqwest::Client::builder()
            .user_agent("Postbox/1.0")
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5));
        if loopback_port.is_some() {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| MediaError::Transport(e.to_string()))?;
        let api_base = match loopback_port {
            Some(port) => format!("http://127.0.0.1:{port}"),
            None => "https://api.giphy.com".to_owned(),
        };
        Ok(Self {
            api_base,
            loopback_rewrite_port: loopback_port,
            client,
        })
    }

    fn rewrite_media_url_if_loopback(&self, url: &str) -> String {
        if let Some(port) = self.loopback_rewrite_port
            && let Some(rest) = url.strip_prefix("https://")
            && let Some(slash) = rest.find('/')
        {
            return format!("http://127.0.0.1:{port}{}", &rest[slash..]);
        }
        url.to_owned()
    }

    async fn read_cancellable_bytes(
        &self,
        url: &str,
        accept: &'static str,
        max_bytes: usize,
        timeout: Duration,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<Vec<u8>, MediaError> {
        if *cancel_rx.borrow() {
            return Err(MediaError::Cancelled);
        }
        let send_fut = self
            .client
            .get(url)
            .header(ACCEPT, accept)
            .timeout(timeout)
            .send();
        let resp = tokio::select! {
            _ = cancel_rx.changed() => return Err(MediaError::Cancelled),
            res = send_fut => res.map_err(|e| MediaError::Transport(e.to_string()))?,
        };
        let status = resp.status().as_u16();
        match status {
            200 => {}
            401 => return Err(MediaError::Unauthorized),
            403 => return Err(MediaError::Forbidden),
            429 => {
                let retry_after_secs = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after_secs);
                return Err(MediaError::RateLimited { retry_after_secs });
            }
            other => return Err(MediaError::HttpStatus(other)),
        }
        if let Some(cl) = resp.content_length()
            && cl > max_bytes as u64
        {
            return Err(MediaError::FileTooLarge);
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        loop {
            let next = tokio::select! {
                _ = cancel_rx.changed() => return Err(MediaError::Cancelled),
                chunk = stream.next() => chunk,
            };
            match next {
                Some(Ok(chunk)) => {
                    if out.len().saturating_add(chunk.len()) > max_bytes {
                        return Err(MediaError::FileTooLarge);
                    }
                    out.extend_from_slice(&chunk);
                }
                Some(Err(err)) => return Err(MediaError::Transport(err.to_string())),
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn search_or_trending(
        &self,
        api_key: &str,
        query: &str,
        limit: usize,
        offset: u32,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<GiphyPage, MediaError> {
        let url = build_giphy_api_url(&self.api_base, api_key, query, limit, offset)?;
        let bytes = self
            .read_cancellable_bytes(
                &url,
                "application/json",
                GIPHY_MAX_RESPONSE_BYTES,
                Duration::from_secs(20),
                cancel_rx,
            )
            .await?;
        parse_giphy_page(&bytes, limit, offset)
    }

    pub async fn download_gif(
        &self,
        gif_id: &str,
        cdn_url: &str,
        is_preview: bool,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<Vec<u8>, MediaError> {
        validate_giphy_cdn_url(cdn_url, gif_id)?;
        let max_bytes = if is_preview {
            GIPHY_MAX_PREVIEW_BYTES
        } else {
            GIPHY_MAX_ORIGINAL_BYTES
        };
        let fetch_url = self.rewrite_media_url_if_loopback(cdn_url);
        let bytes = self
            .read_cancellable_bytes(
                &fetch_url,
                "image/gif",
                max_bytes,
                Duration::from_secs(30),
                cancel_rx,
            )
            .await?;
        validate_gif_magic(&bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    #[test]
    fn giphy_cdn_allowlist_api_bounds_and_strict_json_parsing() {
        // Valid CDN hosts media..media4
        for host in [
            "media.giphy.com",
            "media0.giphy.com",
            "media1.giphy.com",
            "media2.giphy.com",
            "media3.giphy.com",
            "media4.giphy.com",
        ] {
            let url =
                format!("https://{host}/media/v1.abc_123/Gif123/giphy.gif?cid=test&rid=giphy.gif");
            assert!(
                validate_giphy_cdn_url(&url, "Gif123").is_ok(),
                "should allow {host}"
            );
        }
        // Invalid CDN URLs rejected
        for bad in [
            "http://media1.giphy.com/media/Gif123/giphy.gif",
            "https://media5.giphy.com/media/Gif123/giphy.gif",
            "https://media00.giphy.com/media/Gif123/giphy.gif",
            "https://media1.giphy.com:443/media/Gif123/giphy.gif",
            "https://media1.giphy.com.evil.test/media/Gif123/giphy.gif",
            "https://media1.giphy.com/media/WrongId/giphy.gif",
            "https://media1.giphy.com/media/Gif123/../evil.gif",
            "https://media1.giphy.com/media/Gif123/.gif",
            "https://media1.giphy.com/media/Gif123/giphy.gif?x=%0d%0aInjected",
            "https://media1.giphy.com/media/Gif123/giphy.gif#frag",
        ] {
            assert!(
                validate_giphy_cdn_url(bad, "Gif123").is_err(),
                "MUST reject {bad}"
            );
        }

        // Query bounds: 50 codepoints OK, 51 rejected
        let q50: String = std::iter::repeat_n('é', 50).collect();
        let q51: String = std::iter::repeat_n('é', 51).collect();
        assert!(build_giphy_api_url("https://api.giphy.com", "k", &q50, 25, 0).is_ok());
        assert!(build_giphy_api_url("https://api.giphy.com", "k", &q51, 25, 0).is_err());
        assert!(build_giphy_api_url("https://api.giphy.com", "k", "cat", 25, 4976).is_err());

        // Retry-After parsing capped at 86400
        assert_eq!(parse_retry_after_secs("120"), Some(120));
        assert_eq!(parse_retry_after_secs("999999"), Some(86_400));
        assert_eq!(
            parse_retry_after_secs("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );

        // GIF magic check
        assert!(validate_gif_magic(b"GIF89a\x01\x00").is_ok());
        assert!(validate_gif_magic(b"GIF87a\x01\x00").is_ok());
        assert!(matches!(
            validate_gif_magic(b"\x89PNG\r\n\x1a\n"),
            Err(MediaError::NotGif)
        ));

        // Strict numeric JSON parsing (rejects string meta.status or numeric rendition width)
        let bad_json = br#"{"meta":{"status":"200"},"pagination":{"total_count":1,"count":1,"offset":0},"data":[]}"#;
        assert!(parse_giphy_page(bad_json, 25, 0).is_err());

        // 5-field target guard rejects staging if chat or reply changed mid-download!
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let t1 = GiphyTarget::new(chat.clone(), Some("m1"), Some("Alice"));
        let t2 = GiphyTarget::new(chat.clone(), Some("m2"), Some("Alice"));
        let mut st = GiphyPickerState::default();
        let generation = st.begin(t1.clone(), true, "cats".into());
        let page = GiphyPage {
            items: vec![GiphyItem {
                id: "Gif123".into(),
                title: "Cat".into(),
                preview: GiphyRendition {
                    url: "https://media1.giphy.com/media/Gif123/200w.gif".into(),
                    width: 200,
                    height: 150,
                    size: 1000,
                },
                original: GiphyRendition {
                    url: "https://media1.giphy.com/media/Gif123/giphy.gif".into(),
                    width: 400,
                    height: 300,
                    size: 4000,
                },
            }],
            total_count: 1,
            offset: 0,
        };
        assert!(st.accept(generation, &t1, page));
        assert_eq!(
            st.select_for_staging(generation, Some(&t2), 0),
            Err(GIPHY_MSG_TARGET_CHANGED)
        );
        assert_eq!(st.status, GiphyStatus::Closed);
    }
}
