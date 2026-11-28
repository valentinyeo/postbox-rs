use crate::MediaError;
use crate::drain::atomic_write_file;
use postbox_core::{ChatKey, Provider};
use postbox_hub::{AvatarFetchState, HubClient, HubError, valid_team};
use postbox_store::Store;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const AVATAR_POSITIVE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const AVATAR_NEGATIVE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const AVATAR_FAILURE_TTL_SECS: i64 = 24 * 60 * 60;
pub const SLACK_EMOJI_INDEX_TTL_SECS: i64 = 60 * 60;
pub const MAX_SLACK_EMOJI_INDEX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SLACK_EMOJI_MEDIA_BYTES: usize = 1024 * 1024;
pub const MAX_SLACK_ALIAS_HOPS: usize = 32;
pub const AVATAR_BASE_SIZE: usize = 42;

/// Exact sender palette from `design_tokens.zig:226-244` (6 colors).
pub const SENDER_PALETTE_HEX: [u32; 6] =
    [0x00A884, 0x53BDEB, 0xEB8C54, 0xB284EB, 0xEBC354, 0xEB6E96];

const SLACK_EMOJI_TSV: &str = include_str!("../data/slack_emoji.tsv");

static SLACK_STANDARD_TABLE: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();

fn slack_standard_table() -> &'static [(&'static str, &'static str)] {
    SLACK_STANDARD_TABLE.get_or_init(|| {
        SLACK_EMOJI_TSV
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .collect()
    })
}

/// Binary-searches the 4,205-entry byte-sorted `slack_emoji.tsv` (`slack_emoji.zig:9-24`).
pub fn lookup_standard_slack_emoji(name: &str) -> Option<&'static str> {
    let table = slack_standard_table();
    table
        .binary_search_by(|(k, _)| k.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|idx| table[idx].1)
}

pub fn standard_slack_emoji_count() -> usize {
    slack_standard_table().len()
}

/// Exact port of `hub.zig:221-223`: chat avatars are enabled for WhatsApp and Telegram.
pub const fn chat_avatar_enabled(provider: Provider) -> bool {
    matches!(provider, Provider::Whatsapp | Provider::Telegram)
}

/// Exact port of `hub.zig:207-219`: extracts `"slack:<team>:"` workspace avatar key.
pub fn workspace_avatar_key(chat_id: &str) -> Option<String> {
    let rest = chat_id.strip_prefix("slack:")?;
    let colon = rest.find(':')?;
    if colon == 0 || colon + 1 >= rest.len() {
        return None;
    }
    let team = &rest[..colon];
    if valid_team(team).is_err() {
        return None;
    }
    Some(format!("slack:{team}:"))
}

/// Exact port of `sync_gate.zig:54-77`: classifies whether a cached avatar/icon is usable and
/// whether a background refresh is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvatarCacheDecision {
    pub has_image: bool,
    pub needs_fetch: bool,
}

pub fn classify_avatar_cache(
    image_secs: Option<i64>,
    image_decodes: bool,
    none_secs: Option<i64>,
    failed_secs: Option<i64>,
    now_unix: i64,
) -> AvatarCacheDecision {
    let fresh =
        |stamp: Option<i64>, ttl: i64| stamp.is_some_and(|s| now_unix.saturating_sub(s) < ttl);
    if fresh(failed_secs, AVATAR_FAILURE_TTL_SECS) {
        return AvatarCacheDecision {
            has_image: image_secs.is_some() && image_decodes,
            needs_fetch: false,
        };
    }
    if image_secs.is_some() && image_decodes {
        return AvatarCacheDecision {
            has_image: true,
            needs_fetch: !fresh(image_secs, AVATAR_POSITIVE_TTL_SECS),
        };
    }
    AvatarCacheDecision {
        has_image: false,
        needs_fetch: !fresh(none_secs, AVATAR_NEGATIVE_TTL_SECS),
    }
}

/// Exact port of `avatar_mask.zig:13-15`.
pub fn distance_alpha(distance: f32) -> u8 {
    ((0.5 - distance).clamp(0.0, 1.0) * 255.0) as u8
}

/// Exact port of `avatar_mask.zig:3-11`.
pub fn circle_alpha_at(x: usize, y: usize, size: usize) -> u8 {
    if size == 0 {
        return 0;
    }
    let radius = size as f32 / 2.0;
    let dx = (x as f32 + 0.5) - radius;
    let dy = (y as f32 + 0.5) - radius;
    let dist = (dx * dx + dy * dy).sqrt();
    distance_alpha(dist - radius)
}

/// Applies premultiplied anti-aliased circular mask in-place (`avatar_mask.zig:17-30`).
pub fn apply_circle_mask(pixels: &mut [u8], size: usize) {
    if size == 0 || pixels.len() < size * size * 4 {
        return;
    }
    for y in 0..size {
        for x in 0..size {
            let a = u16::from(circle_alpha_at(x, y, size));
            let base = (y * size + x) * 4;
            for c in 0..4 {
                pixels[base + c] = ((u16::from(pixels[base + c]) * a) / 255) as u8;
            }
        }
    }
}

/// Exact port of `avatar_mask.zig:32-56`: pixel-center bilinear resample + premultiplied AA
/// circle mask at exact destination resolution.
pub fn scale_circle_avatar(
    source: &[u8],
    src_w: usize,
    src_h: usize,
    target_size: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || target_size == 0 || source.len() < src_w * src_h * 4 {
        return Vec::new();
    }
    let mut dst = vec![0u8; target_size * target_size * 4];
    let dst_len = dst.len();
    if src_w == target_size && src_h == target_size {
        dst.copy_from_slice(&source[..dst_len]);
        apply_circle_mask(&mut dst, target_size);
        return dst;
    }
    let max_sx = (src_w - 1) as f32;
    let max_sy = (src_h - 1) as f32;
    for y in 0..target_size {
        let sy = ((y as f32 + 0.5) * src_h as f32 / target_size as f32 - 0.5).clamp(0.0, max_sy);
        let y0 = sy as usize;
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = sy - y0 as f32;
        for x in 0..target_size {
            let sx =
                ((x as f32 + 0.5) * src_w as f32 / target_size as f32 - 0.5).clamp(0.0, max_sx);
            let x0 = sx as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let fx = sx - x0 as f32;

            let p00 = (y0 * src_w + x0) * 4;
            let p10 = (y0 * src_w + x1) * 4;
            let p01 = (y1 * src_w + x0) * 4;
            let p11 = (y1 * src_w + x1) * 4;
            let out_base = (y * target_size + x) * 4;
            for c in 0..4 {
                let top = f32::from(source[p00 + c]) * (1.0 - fx) + f32::from(source[p10 + c]) * fx;
                let bottom =
                    f32::from(source[p01 + c]) * (1.0 - fx) + f32::from(source[p11 + c]) * fx;
                let val = (top * (1.0 - fy) + bottom * fy).clamp(0.0, 255.0).round();
                dst[out_base + c] = val as u8;
            }
        }
    }
    apply_circle_mask(&mut dst, target_size);
    dst
}

/// Exact port of `avatar_mask.zig:58-81`: integer area-averaging box downscale for square
/// Slack workspace icons.
pub fn scale_square_area(source: &[u8], source_size: usize, target_size: usize) -> Vec<u8> {
    if source_size == 0 || target_size == 0 || source.len() < source_size * source_size * 4 {
        return Vec::new();
    }
    let mut dst = vec![0u8; target_size * target_size * 4];
    let area = (source_size as u64) * (source_size as u64);
    for y in 0..target_size {
        let top = y * source_size;
        let bottom = (y + 1) * source_size;
        let end_y = bottom.div_ceil(target_size);
        for x in 0..target_size {
            let left = x * source_size;
            let right = (x + 1) * source_size;
            let end_x = right.div_ceil(target_size);
            let mut sums = [0u64; 4];
            for sy in (top / target_size)..end_y {
                let oy = bottom.min((sy + 1) * target_size) - top.max(sy * target_size);
                for sx in (left / target_size)..end_x {
                    let ox = right.min((sx + 1) * target_size) - left.max(sx * target_size);
                    let w = (ox as u64) * (oy as u64);
                    let base = (sy * source_size + sx) * 4;
                    for c in 0..4 {
                        sums[c] += u64::from(source[base + c]) * w;
                    }
                }
            }
            let out_base = (y * target_size + x) * 4;
            for c in 0..4 {
                dst[out_base + c] = ((sums[c] + area / 2) / area) as u8;
            }
        }
    }
    dst
}

/// Applies rounded-rectangle SDF AA mask for Slack workspace icon badges (`main.zig:17361-17394`).
pub fn apply_rounded_rect_mask(pixels: &mut [u8], side: usize, radius: f32) {
    if side == 0 || pixels.len() < side * side * 4 {
        return;
    }
    let side_f = side as f32;
    for y in 0..side {
        let py = y as f32 + 0.5;
        let dy = (radius - py).max(py - (side_f - radius));
        for x in 0..side {
            let px = x as f32 + 0.5;
            let dx = (radius - px).max(px - (side_f - radius));
            let dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - radius;
            let a = u16::from(distance_alpha(dist));
            let base = (y * side + x) * 4;
            for c in 0..4 {
                pixels[base + c] = ((u16::from(pixels[base + c]) * a) / 255) as u8;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderBadge {
    WorkspaceIcon,
    WhatsappMark,
    TelegramMark,
    SignalMark,
    Letter(&'static str),
}

/// Exact port of `hub.providerBadge` (`hub.zig:225-234`).
pub const fn provider_badge(
    provider: Provider,
    has_cached_workspace_icon: bool,
    brand_marks_enabled: bool,
) -> ProviderBadge {
    match provider {
        Provider::Slack => {
            if brand_marks_enabled && has_cached_workspace_icon {
                ProviderBadge::WorkspaceIcon
            } else {
                ProviderBadge::Letter("S")
            }
        }
        Provider::Whatsapp => {
            if brand_marks_enabled {
                ProviderBadge::WhatsappMark
            } else {
                ProviderBadge::Letter("W")
            }
        }
        Provider::Telegram => {
            if brand_marks_enabled {
                ProviderBadge::TelegramMark
            } else {
                ProviderBadge::Letter("T")
            }
        }
        Provider::Signal => {
            if brand_marks_enabled {
                ProviderBadge::SignalMark
            } else {
                ProviderBadge::Letter("S")
            }
        }
    }
}

/// Deterministic FNV-1a 32-bit hash picking a sender avatar color index `0..6`
/// (`main.zig:18561-18605`).
pub fn sender_color_index(sender_id: &str, sender_name: &str) -> usize {
    let key = if !sender_id.is_empty() {
        sender_id
    } else {
        sender_name
    };
    let mut hash: u32 = 2_166_136_261;
    for &b in key.as_bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(16_777_619);
    }
    (hash as usize) % SENDER_PALETTE_HEX.len()
}

/// Returns the first Unicode scalar (uppercase) for avatar fallback letter badges.
pub fn avatar_initial(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

/// Token emitted by `next_slack_emoji_token` (`slack_emoji.zig:41-61`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackEmojiToken<'a> {
    pub start: usize,
    pub end: usize,
    pub name: &'a str,
}

fn is_slack_emoji_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+')
}

/// Exact port of `slack_emoji.next` (`slack_emoji.zig:41-61`): finds the next `:name:` token
/// starting at or after `from`, skipping `http://` and `https://` URLs up to whitespace, `|`,
/// or `>` so Slack mrkdwn links `<https://example.invalid/:tea:|:broken_heart:>` still parse
/// `:broken_heart:` after `|`.
pub fn next_slack_emoji_token(text: &str, from: usize) -> Option<SlackEmojiToken<'_>> {
    let bytes = text.as_bytes();
    let mut start = from;
    while start < bytes.len() {
        let rest = &bytes[start..];
        if rest.starts_with(b"https://") || rest.starts_with(b"http://") {
            while start < bytes.len()
                && !bytes[start].is_ascii_whitespace()
                && bytes[start] != b'|'
                && bytes[start] != b'>'
            {
                start += 1;
            }
            continue;
        }
        if bytes[start] == b':' {
            let mut end = start + 1;
            while end < bytes.len() && end - start <= 192 {
                let c = bytes[end];
                if c == b':' {
                    if end > start + 1 {
                        return Some(SlackEmojiToken {
                            start,
                            end: end + 1,
                            name: &text[start + 1..end],
                        });
                    }
                    break;
                }
                if !is_slack_emoji_char(c) {
                    break;
                }
                end += 1;
            }
        }
        start += 1;
    }
    None
}

/// Parsed Slack workspace custom emoji index (`GET /v1/wazig/slack/emoji?account={team}`,
/// 1h TTL, `<= 4 MiB`, 32-hop `alias:` chain resolution rejecting cycles).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackEmojiIndex {
    pub team: String,
    pub fetched_at: i64,
    pub entries: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSlackEmoji {
    Standard(&'static str),
    Custom { name: String, url: String },
}

impl SlackEmojiIndex {
    pub fn from_hub_json(
        team: impl Into<String>,
        value: &serde_json::Value,
        fetched_at: i64,
    ) -> Self {
        let mut entries = HashMap::new();
        let map = value
            .get("emoji")
            .and_then(serde_json::Value::as_object)
            .or_else(|| value.as_object());
        if let Some(obj) = map {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    entries.insert(k.clone(), s.to_owned());
                }
            }
        }
        Self {
            team: team.into(),
            fetched_at,
            entries,
        }
    }

    pub fn is_fresh(&self, now_unix: i64) -> bool {
        now_unix.saturating_sub(self.fetched_at) < SLACK_EMOJI_INDEX_TTL_SECS
    }

    /// Exact port of `slack_emoji.customName` (`slack_emoji.zig:26-37`): resolves `alias:` chains
    /// up to 32 hops, explicitly rejecting cycles and non-HTTP(S) values.
    pub fn resolve(&self, name: &str) -> Option<ResolvedSlackEmoji> {
        let mut current = name;
        let mut visited: HashSet<&str> = HashSet::new();
        for _ in 0..MAX_SLACK_ALIAS_HOPS {
            if let Some(std_emoji) = lookup_standard_slack_emoji(current) {
                return Some(ResolvedSlackEmoji::Standard(std_emoji));
            }
            if !visited.insert(current) {
                return None;
            }
            let val = self.entries.get(current)?;
            if let Some(alias_target) = val.strip_prefix("alias:") {
                if alias_target.is_empty() {
                    return None;
                }
                current = alias_target;
                continue;
            }
            if val.starts_with("https://") || val.starts_with("http://") {
                return Some(ResolvedSlackEmoji::Custom {
                    name: current.to_owned(),
                    url: val.clone(),
                });
            }
            return None;
        }
        None
    }

    /// Replaces standard `:name:` (including adjacent `:skin-tone-N:` chains) and optionally
    /// resolves custom emoji via `custom_resolver` (`slack_emoji.zig:63-89`).
    pub fn replace_text<F>(&self, text: &str, mut custom_resolver: F) -> String
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        while let Some(tok) = next_slack_emoji_token(text, cursor) {
            out.push_str(&text[cursor..tok.start]);
            let mut end = tok.end;
            let mut current_name = tok.name.to_owned();
            let mut emoji = lookup_standard_slack_emoji(&current_name).map(str::to_owned);

            // Adjacent skin-tone chaining (`+1` + `skin-tone-2` -> `+1::skin-tone-2`).
            while end < text.len() {
                let Some(tone) = next_slack_emoji_token(text, end) else {
                    break;
                };
                if tone.start != end || !tone.name.starts_with("skin-tone-") {
                    break;
                }
                let combined = format!("{current_name}::{}", tone.name);
                if combined.len() <= 256
                    && let Some(toned) = lookup_standard_slack_emoji(&combined)
                {
                    current_name = combined;
                    emoji = Some(toned.to_owned());
                    end = tone.end;
                    continue;
                }
                break;
            }

            if emoji.is_none() {
                match self.resolve(&current_name) {
                    Some(ResolvedSlackEmoji::Standard(s)) => emoji = Some(s.to_owned()),
                    Some(ResolvedSlackEmoji::Custom { name, .. }) => {
                        emoji = custom_resolver(&name);
                    }
                    None => {}
                }
            }

            if let Some(replacement) = emoji {
                out.push_str(&replacement);
            } else {
                out.push_str(&text[tok.start..end]);
            }
            cursor = end;
        }
        out.push_str(&text[cursor..]);
        out
    }

    /// Returns unique non-standard custom emoji names referenced in `text`.
    pub fn referenced_custom_names(&self, text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = 0usize;
        while let Some(tok) = next_slack_emoji_token(text, cursor) {
            cursor = tok.end;
            if lookup_standard_slack_emoji(tok.name).is_some() {
                continue;
            }
            if let Some(ResolvedSlackEmoji::Custom { name, .. }) = self.resolve(tok.name)
                && seen.insert(name.clone())
            {
                names.push(name);
            }
        }
        names
    }
}

pub fn default_wacli_remote_avatars_dir() -> Option<PathBuf> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home_var).map(|home| PathBuf::from(home).join(".wacli").join("remote-avatars"))
}

pub fn find_local_wacli_avatar_in_dir(chat: &ChatKey, dir: &Path) -> Option<PathBuf> {
    if chat.provider != Provider::Whatsapp {
        return None;
    }
    let bare_jid = chat
        .id
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(&chat.id);
    if bare_jid.is_empty() || bare_jid.contains(['/', '\\', '\0']) || bare_jid.contains("..") {
        return None;
    }
    [
        format!("{bare_jid}.jpg"),
        format!("{bare_jid}.jpeg"),
        format!("{bare_jid}.png"),
        format!("{bare_jid}.webp"),
        bare_jid.to_owned(),
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .find(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarCacheLookup {
    Ready(PathBuf),
    NeedsFetch { fallback_path: Option<PathBuf> },
    Missing,
}

pub fn resolve_avatar_cache_or_seed(
    store: &Store,
    avatar_dir: &Path,
    seed_dir: Option<&Path>,
    chat: &ChatKey,
    now_unix: i64,
) -> Result<AvatarCacheLookup, MediaError> {
    if !chat_avatar_enabled(chat.provider) {
        return Ok(AvatarCacheLookup::Missing);
    }
    let marker_key = format!(
        "avatar:{}:{}:{}",
        chat.provider.as_str(),
        chat.account,
        chat.id
    );
    let file_hash = short_sha256_hex(marker_key.as_bytes());
    let file_path = avatar_dir.join(format!("{file_hash}.img"));

    let existing = store.get_fetch_marker(&marker_key)?;
    let (img_secs, none_secs, fail_secs) = match existing.as_ref() {
        Some(m) if m.status == "ready" => (Some(m.checked_at), None, None),
        Some(m) if m.status == "none" => (None, Some(m.checked_at), None),
        Some(m) if m.status == "fail" => {
            (file_path.is_file().then_some(0), None, Some(m.checked_at))
        }
        _ => (None, None, None),
    };
    let has_disk_file = file_path.is_file();
    let decision = classify_avatar_cache(img_secs, has_disk_file, none_secs, fail_secs, now_unix);
    if decision.has_image && !decision.needs_fetch {
        return Ok(AvatarCacheLookup::Ready(file_path));
    }

    // Local images take precedence over negative markers, including failures from queued requests.
    if let Some(seed_path) = seed_dir.and_then(|dir| find_local_wacli_avatar_in_dir(chat, dir)) {
        let bytes = std::fs::read(seed_path)?;
        if !bytes.is_empty() {
            atomic_write_file(&file_path, &bytes)?;
            let rel = format!("{file_hash}.img");
            store.upsert_fetch_marker(&marker_key, "ready", now_unix, Some(&rel))?;
            return Ok(AvatarCacheLookup::Ready(file_path));
        }
    }

    if !decision.needs_fetch {
        return Ok(if decision.has_image {
            AvatarCacheLookup::Ready(file_path)
        } else {
            AvatarCacheLookup::Missing
        });
    }
    Ok(AvatarCacheLookup::NeedsFetch {
        fallback_path: decision.has_image.then_some(file_path),
    })
}

pub fn commit_avatar_fetch_result(
    store: &Store,
    avatar_dir: &Path,
    chat: &ChatKey,
    now_unix: i64,
    outcome: Result<AvatarFetchState, HubError>,
) -> Result<Option<PathBuf>, MediaError> {
    let marker_key = format!(
        "avatar:{}:{}:{}",
        chat.provider.as_str(),
        chat.account,
        chat.id
    );
    let file_hash = short_sha256_hex(marker_key.as_bytes());
    let file_path = avatar_dir.join(format!("{file_hash}.img"));
    match outcome {
        Ok(AvatarFetchState::Ready(bytes)) => {
            atomic_write_file(&file_path, &bytes)?;
            let rel = format!("{file_hash}.img");
            store.upsert_fetch_marker(&marker_key, "ready", now_unix, Some(&rel))?;
            Ok(Some(file_path))
        }
        Ok(AvatarFetchState::Pending) => Ok(file_path.is_file().then_some(file_path)),
        Ok(AvatarFetchState::Missing) => {
            let _ = std::fs::remove_file(&file_path);
            store.upsert_fetch_marker(&marker_key, "none", now_unix, None)?;
            Ok(None)
        }
        Err(_) => {
            store.upsert_fetch_marker(&marker_key, "fail", now_unix, None)?;
            Ok(file_path.is_file().then_some(file_path))
        }
    }
}

/// Fetches and caches a chat avatar (`GET /v1/wazig/avatar/{chat}`) honoring the exact 7d/7d/24h
/// TTL policy via SQLite `media_fetch_markers` and disk storage.
pub async fn fetch_or_load_avatar(
    client: &HubClient,
    store: &Store,
    avatar_dir: &Path,
    chat: &ChatKey,
    now_unix: i64,
) -> Result<Option<PathBuf>, MediaError> {
    match resolve_avatar_cache_or_seed(store, avatar_dir, None, chat, now_unix)? {
        AvatarCacheLookup::Ready(path) => Ok(Some(path)),
        AvatarCacheLookup::Missing => Ok(None),
        AvatarCacheLookup::NeedsFetch { .. } => {
            let outcome = client.fetch_avatar_state(&chat.id).await;
            commit_avatar_fetch_result(store, avatar_dir, chat, now_unix, outcome)
        }
    }
}

/// Fetches and caches a Slack workspace icon (`GET /v1/wazig/workspace-icon/{team}`) with 7d/24h TTL.
pub async fn fetch_or_load_workspace_icon(
    client: &HubClient,
    store: &Store,
    avatar_dir: &Path,
    team: &str,
    now_unix: i64,
) -> Result<Option<PathBuf>, MediaError> {
    if valid_team(team).is_err() {
        return Ok(None);
    }
    let marker_key = format!("wsicon:slack:{team}:");
    let file_hash = short_sha256_hex(marker_key.as_bytes());
    let file_path = avatar_dir.join(format!("{file_hash}.img"));

    let existing = store.get_fetch_marker(&marker_key)?;
    let (img_secs, none_secs, fail_secs) = match existing.as_ref() {
        Some(m) if m.status == "ready" => (Some(m.checked_at), None, None),
        Some(m) if m.status == "none" => (None, Some(m.checked_at), None),
        Some(m) if m.status == "fail" => {
            (file_path.is_file().then_some(0), None, Some(m.checked_at))
        }
        _ => (None, None, None),
    };
    let has_disk_file = file_path.is_file();
    let decision = classify_avatar_cache(img_secs, has_disk_file, none_secs, fail_secs, now_unix);
    if !decision.needs_fetch {
        return Ok(decision.has_image.then_some(file_path));
    }

    match client.fetch_workspace_icon(team).await {
        Ok(Some(bytes)) => {
            atomic_write_file(&file_path, &bytes)?;
            let rel = format!("{file_hash}.img");
            store.upsert_fetch_marker(&marker_key, "ready", now_unix, Some(&rel))?;
            Ok(Some(file_path))
        }
        Ok(None) => {
            let _ = std::fs::remove_file(&file_path);
            store.upsert_fetch_marker(&marker_key, "none", now_unix, None)?;
            Ok(None)
        }
        Err(_) => {
            store.upsert_fetch_marker(&marker_key, "fail", now_unix, None)?;
            Ok(has_disk_file.then_some(file_path))
        }
    }
}

/// Fetches and caches a single Slack custom emoji image (`<= 1 MiB`) on disk.
pub async fn fetch_or_load_slack_emoji_media(
    client: &HubClient,
    cache_dir: &Path,
    team: &str,
    name: &str,
    url: &str,
) -> Result<Option<PathBuf>, MediaError> {
    let key = format!(
        "slack-emoji:{team}:{name}:{}",
        short_sha256_hex(url.as_bytes())
    );
    let path = cache_dir.join(format!("{}.img", short_sha256_hex(key.as_bytes())));
    if path.is_file() {
        return Ok(Some(path));
    }
    match client.fetch_slack_emoji_media(team, name).await? {
        Some(bytes) if bytes.len() <= MAX_SLACK_EMOJI_MEDIA_BYTES => {
            atomic_write_file(&path, &bytes)?;
            Ok(Some(path))
        }
        Some(_) => Err(MediaError::FileTooLarge),
        None => Ok(None),
    }
}

fn short_sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    crate::drain::hex_encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avatar_test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "postbox_avatar_{tag}_{}_{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn local_wacli_avatar_seeds_cache_and_heals_negative_markers() {
        let root = avatar_test_dir("seed");
        let seed_dir = root.join(".wacli").join("remote-avatars");
        let avatar_dir = root.join("avatars");
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::write(seed_dir.join("123@s.whatsapp.net.jpg"), b"LOCAL-AVATAR").unwrap();
        let chat = ChatKey::new(
            Provider::Whatsapp,
            "personal",
            "personal:123@s.whatsapp.net",
        );
        let marker_key = "avatar:whatsapp:personal:personal:123@s.whatsapp.net";
        let rel = format!("{}.img", short_sha256_hex(marker_key.as_bytes()));
        let path = avatar_dir.join(&rel);
        let store = Store::open_in_memory().unwrap();

        for status in ["fail", "none"] {
            store
                .upsert_fetch_marker(marker_key, status, 100, None)
                .unwrap();
            assert_eq!(
                resolve_avatar_cache_or_seed(&store, &avatar_dir, Some(&seed_dir), &chat, 101)
                    .unwrap(),
                AvatarCacheLookup::Ready(path.clone())
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"LOCAL-AVATAR");
            let marker = store.get_fetch_marker(marker_key).unwrap().unwrap();
            assert_eq!(marker.status, "ready");
            assert_eq!(marker.checked_at, 101);
            assert_eq!(marker.rel_path.as_deref(), Some(rel.as_str()));
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_wacli_avatar_ignores_empty_negative_cache_files() {
        let root = avatar_test_dir("empty");
        let seed_dir = root.join("remote-avatars");
        let avatar_dir = root.join("avatars");
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::write(seed_dir.join("123@s.whatsapp.net.jpg"), b"").unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "personal", "123@s.whatsapp.net");
        let store = Store::open_in_memory().unwrap();
        assert_eq!(find_local_wacli_avatar_in_dir(&chat, &seed_dir), None);
        assert_eq!(
            resolve_avatar_cache_or_seed(&store, &avatar_dir, Some(&seed_dir), &chat, 100).unwrap(),
            AvatarCacheLookup::NeedsFetch {
                fallback_path: None
            }
        );
        assert!(
            store
                .get_fetch_marker("avatar:whatsapp:personal:123@s.whatsapp.net")
                .unwrap()
                .is_none()
        );
        let png = seed_dir.join("123@s.whatsapp.net.png");
        std::fs::write(&png, b"PNG-AVATAR").unwrap();
        assert_eq!(find_local_wacli_avatar_in_dir(&chat, &seed_dir), Some(png));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_wacli_avatar_rejects_unsafe_ids_and_other_providers() {
        let root = avatar_test_dir("unsafe");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("123@s.whatsapp.net.jpg"), b"LOCAL-AVATAR").unwrap();
        let telegram = ChatKey::new(Provider::Telegram, "personal", "123@s.whatsapp.net");
        assert_eq!(find_local_wacli_avatar_in_dir(&telegram, &root), None);
        for id in [
            "",
            "personal:",
            "../123",
            "personal:../123",
            "a/b",
            "a\\b",
            "a..b",
            "a\0b",
        ] {
            let chat = ChatKey::new(Provider::Whatsapp, "personal", id);
            assert_eq!(find_local_wacli_avatar_in_dir(&chat, &root), None);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_avatar_fetch_leaves_markers_and_cached_image_unchanged() {
        let root = avatar_test_dir("pending");
        let chat = ChatKey::new(
            Provider::Whatsapp,
            "personal",
            "personal:123@s.whatsapp.net",
        );
        let marker_key = "avatar:whatsapp:personal:personal:123@s.whatsapp.net";
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            commit_avatar_fetch_result(&store, &root, &chat, 100, Ok(AvatarFetchState::Pending))
                .unwrap(),
            None
        );
        assert!(store.get_fetch_marker(marker_key).unwrap().is_none());

        let path = commit_avatar_fetch_result(
            &store,
            &root,
            &chat,
            101,
            Ok(AvatarFetchState::Ready(b"CACHED-AVATAR".to_vec())),
        )
        .unwrap()
        .unwrap();
        for status in ["ready", "fail", "none"] {
            store
                .upsert_fetch_marker(marker_key, status, 101, Some("cached.img"))
                .unwrap();
            let before = store.get_fetch_marker(marker_key).unwrap();
            assert_eq!(
                commit_avatar_fetch_result(
                    &store,
                    &root,
                    &chat,
                    102,
                    Ok(AvatarFetchState::Pending)
                )
                .unwrap(),
                Some(path.clone())
            );
            assert_eq!(store.get_fetch_marker(marker_key).unwrap(), before);
            assert_eq!(std::fs::read(&path).unwrap(), b"CACHED-AVATAR");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn circle_mask_and_square_area_downscale_match_spec() {
        assert_eq!(circle_alpha_at(0, 0, 42), 0);
        assert_eq!(circle_alpha_at(21, 21, 42), 255);
        // Symmetric
        assert_eq!(circle_alpha_at(5, 10, 42), circle_alpha_at(10, 5, 42));

        let src = vec![255u8; 84 * 84 * 4];
        let masked = scale_circle_avatar(&src, 84, 84, 42);
        assert_eq!(masked.len(), 42 * 42 * 4);
        assert_eq!(masked[3], 0); // corner fully transparent
        let center = (21 * 42 + 21) * 4;
        assert_eq!(masked[center + 3], 255); // center opaque

        let area_dst = scale_square_area(&src, 84, 21);
        assert_eq!(area_dst.len(), 21 * 21 * 4);
        assert_eq!(&area_dst[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn avatar_ttl_precedence_and_workspace_key() {
        assert!(chat_avatar_enabled(Provider::Whatsapp));
        assert!(chat_avatar_enabled(Provider::Telegram));
        assert!(!chat_avatar_enabled(Provider::Slack));
        assert_eq!(
            workspace_avatar_key("slack:T123:C456").as_deref(),
            Some("slack:T123:")
        );
        assert_eq!(workspace_avatar_key("slack:..:C456"), None);

        // Fresh .fail suppresses refetch even if image is stale!
        let d = classify_avatar_cache(Some(100), true, None, Some(900_000), 900_100);
        assert_eq!(
            d,
            AvatarCacheDecision {
                has_image: true,
                needs_fetch: false
            }
        );
        // Stale image (> 7d) triggers refresh
        let d2 = classify_avatar_cache(Some(100), true, None, None, 100 + AVATAR_POSITIVE_TTL_SECS);
        assert_eq!(
            d2,
            AvatarCacheDecision {
                has_image: true,
                needs_fetch: true
            }
        );
        // Fresh 404 .none suppresses refresh for 7d
        let d3 = classify_avatar_cache(None, false, Some(1000), None, 2000);
        assert_eq!(
            d3,
            AvatarCacheDecision {
                has_image: false,
                needs_fetch: false
            }
        );
    }

    #[test]
    fn slack_emoji_standard_skin_tone_url_skip_and_32_hop_cycle_rejection() {
        assert_eq!(standard_slack_emoji_count(), 4205);
        assert_eq!(lookup_standard_slack_emoji("+1"), Some("👍"));
        assert_eq!(lookup_standard_slack_emoji("+1::skin-tone-2"), Some("👍🏻"));

        let mut json_map = serde_json::Map::new();
        json_map.insert(
            "shipit".into(),
            serde_json::Value::String("https://cdn.example.invalid/shipit.png".into()),
        );
        json_map.insert(
            "launch".into(),
            serde_json::Value::String("alias:shipit".into()),
        );
        json_map.insert("nice".into(), serde_json::Value::String("alias:+1".into()));
        json_map.insert(
            "cycle_a".into(),
            serde_json::Value::String("alias:cycle_b".into()),
        );
        json_map.insert(
            "cycle_b".into(),
            serde_json::Value::String("alias:cycle_a".into()),
        );
        let idx =
            SlackEmojiIndex::from_hub_json("T1", &serde_json::json!({ "emoji": json_map }), 1000);

        assert_eq!(
            idx.resolve("launch"),
            Some(ResolvedSlackEmoji::Custom {
                name: "shipit".into(),
                url: "https://cdn.example.invalid/shipit.png".into()
            })
        );
        assert_eq!(
            idx.resolve("nice"),
            Some(ResolvedSlackEmoji::Standard("👍"))
        );
        assert_eq!(idx.resolve("cycle_a"), None);

        // URL skipping preserves URL `:tea:` while still resolving `:broken_heart:` after `|`!
        let replaced = idx.replace_text(
            "Check <https://example.invalid/:tea:|:broken_heart:> and :+1::skin-tone-2: :launch:",
            |name| Some(format!("[custom:{name}]")),
        );
        assert_eq!(
            replaced,
            "Check <https://example.invalid/:tea:|💔> and 👍🏻 [custom:shipit]"
        );
    }

    #[test]
    fn slack_emoji_scanner_handles_multibyte_utf8_without_panicking() {
        let mut json_map = serde_json::Map::new();
        json_map.insert(
            "launch".into(),
            serde_json::Value::String("https://cdn.example.invalid/launch.png".into()),
        );
        let idx =
            SlackEmojiIndex::from_hub_json("T1", &serde_json::json!({ "emoji": json_map }), 1000);

        for sample in [
            "Übung :+1:",
            "🎉 :h31_0:",
            "héllo world",
            "Grüße <https://example.invalid/:tea:|:broken_heart:> 日本語 :launch:",
        ] {
            let _ = next_slack_emoji_token(sample, 0);
            let _ = idx.referenced_custom_names(sample);
        }

        // Non-boundary starting byte offset inside multi-byte 'Ü' must never panic
        let tok = next_slack_emoji_token("Übung :+1:", 1).expect("token after Ü");
        assert_eq!(tok.name, "+1");
        assert_eq!(idx.replace_text("Übung :+1:", |_| None), "Übung 👍");
        assert_eq!(
            idx.replace_text(
                "Grüße <https://example.invalid/:tea:|:broken_heart:> 日本語 :launch:",
                |name| Some(format!("[{name}]")),
            ),
            "Grüße <https://example.invalid/:tea:|💔> 日本語 [launch]"
        );
        assert_eq!(
            idx.referenced_custom_names(
                "Grüße <https://example.invalid/:tea:|:broken_heart:> 日本語 :launch:"
            ),
            vec!["launch".to_owned()]
        );
    }
}
