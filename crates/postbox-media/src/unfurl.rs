use crate::MediaError;
use crate::drain::{atomic_write_file, hex_encode};
use futures_util::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::ACCEPT;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

pub const MAX_UNFURL_RING_ENTRIES: usize = 24;
pub const MAX_UNFURL_CONCURRENT: usize = 3;
pub const UNFURL_CARD_WIDTH_PX: u32 = 320;
pub const UNFURL_VIDEO_CARD_HEIGHT_PX: u32 = 100;
pub const UNFURL_PAGE_CARD_HEIGHT_PX: u32 = 72;
pub const UNFURL_THUMB_MAX_WIDTH_PX: u32 = 160;
pub const MAX_OEMBED_BYTES: usize = 16 * 1024;
pub const MAX_PAGE_HTML_BYTES: usize = 256 * 1024;
pub const MAX_YT_WATCH_BYTES: usize = 1024 * 1024;
pub const MAX_UNFURL_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_UNFURL_JSON_BYTES: usize = 16 * 1024;
pub const MAX_ID_LEN: usize = 40;
pub const MAX_CANONICAL_LEN: usize = 384;
pub const MAX_TITLE_BYTES: usize = 160;
pub const MAX_AUTHOR_BYTES: usize = 64;
pub const MAX_PLAY_URL_BYTES: usize = 1024;

const UNFURL_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnfurlProvider {
    Youtube,
    Instagram,
    Facebook,
    Image,
    Page,
}

impl UnfurlProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Youtube => "youtube",
            Self::Instagram => "instagram",
            Self::Facebook => "facebook",
            Self::Image => "image",
            Self::Page => "page",
        }
    }

    pub const fn card_height_px(self) -> u32 {
        match self {
            Self::Youtube | Self::Instagram | Self::Facebook => UNFURL_VIDEO_CARD_HEIGHT_PX,
            Self::Page | Self::Image => UNFURL_PAGE_CARD_HEIGHT_PX,
        }
    }

    pub const fn fallback_title(self) -> &'static str {
        match self {
            Self::Instagram => "Reel",
            Self::Youtube | Self::Facebook => "Video",
            Self::Page | Self::Image => "Open link",
        }
    }

    pub const fn placeholder_color_hex(self) -> Option<u32> {
        match self {
            Self::Youtube => Some(0xC52828),
            Self::Instagram => Some(0xE1306C),
            Self::Facebook => Some(0x1877F2),
            Self::Page | Self::Image => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassifiedUrl {
    pub provider: UnfurlProvider,
    pub id: String,
    pub canonical: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnfurlCard {
    pub provider: UnfurlProvider,
    pub id: String,
    pub canonical: String,
    pub site: String,
    pub title: String,
    pub author: String,
    pub description: String,
    pub play_url: Option<String>,
    pub thumb_path: Option<PathBuf>,
}

impl UnfurlCard {
    pub fn display_title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else {
            self.provider.fallback_title()
        }
    }

    pub fn card_geometry(&self, available_width: u32) -> (u32, u32, u32) {
        let card_w = available_width.min(UNFURL_CARD_WIDTH_PX);
        let card_h = self.provider.card_height_px();
        let thumb_w = if self.provider == UnfurlProvider::Page && self.thumb_path.is_none() {
            0
        } else {
            UNFURL_THUMB_MAX_WIDTH_PX.min(card_w / 2)
        };
        (card_w, card_h, thumb_w)
    }
}

fn is_link_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn valid_link_id(s: &str, min_len: usize) -> bool {
    s.len() >= min_len && s.len() <= MAX_ID_LEN && s.bytes().all(is_link_id_char)
}

fn trim_url_token(raw: &str) -> &str {
    raw.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
        .trim_end_matches(|c| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\''
            )
        })
}

fn split_scheme(raw: &str) -> Option<(&'static str, &str)> {
    let bytes = raw.as_bytes();
    if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        Some(("https", &raw[8..]))
    } else if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        Some(("http", &raw[7..]))
    } else {
        None
    }
}

/// Splits authority host (without `:port` and trailing `.`) and path+query from post-scheme rest.
/// Strictly rejects `@` userinfo, empty/double dots, and any char outside `[A-Za-z0-9.-]`.
fn split_host_and_rest(rest: &str) -> Option<(&str, &str)> {
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host_no_port = match authority.rsplit_once(':') {
        Some((h, port_str))
            if !port_str.is_empty() && port_str.bytes().all(|b| b.is_ascii_digit()) =>
        {
            h
        }
        Some(_) => return None,
        None => authority,
    };
    let host = host_no_port.strip_suffix('.').unwrap_or(host_no_port);
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    Some((host, &rest[host_end..]))
}

pub fn site_host(url_str: &str) -> String {
    let Some((_, rest)) = split_scheme(url_str) else {
        return String::new();
    };
    let Some((host, _)) = split_host_and_rest(rest) else {
        return String::new();
    };
    let lower = host.to_ascii_lowercase();
    let stripped = lower.strip_prefix("www.").unwrap_or(&lower);
    truncate_utf8(stripped, MAX_AUTHOR_BYTES)
}

pub fn is_direct_image_url(url_str: &str) -> bool {
    let Some((_, rest)) = split_scheme(url_str) else {
        return false;
    };
    let Some((_, path_and_more)) = split_host_and_rest(rest) else {
        return false;
    };
    let path = path_and_more.split(['?', '#']).next().unwrap_or("");
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v);
        }
    }
    None
}

/// Exact port of `unfurl.classify` (`unfurl.zig:26-235`).
pub fn classify_url(raw: &str) -> Option<ClassifiedUrl> {
    let cleaned = trim_url_token(raw);
    if cleaned.is_empty() || cleaned.len() > MAX_CANONICAL_LEN {
        return None;
    }
    let (_scheme, rest) = split_scheme(cleaned)?;
    let (host, tail) = split_host_and_rest(rest)?;
    let host_lc = host.to_ascii_lowercase();
    let (path, query_frag) = match tail.find('?') {
        Some(idx) => (&tail[..idx], &tail[idx + 1..]),
        None => (tail.split('#').next().unwrap_or(tail), ""),
    };
    let query = query_frag.split('#').next().unwrap_or("");

    // 1. YouTube
    if matches!(
        host_lc.as_str(),
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "music.youtube.com"
            | "youtu.be"
            | "youtube-nocookie.com"
    ) {
        let yt_id = if host_lc == "youtu.be" {
            path.strip_prefix('/').and_then(|s| s.split('/').next())
        } else if path.eq_ignore_ascii_case("/watch") {
            query_param(query, "v")
        } else {
            let p_lc = path.to_ascii_lowercase();
            ["/shorts/", "/embed/", "/live/", "/v/"]
                .iter()
                .find_map(|pfx| {
                    p_lc.starts_with(pfx)
                        .then(|| path[pfx.len()..].split('/').next())
                        .flatten()
                })
        };
        if let Some(id) = yt_id
            && valid_link_id(id, 5)
        {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Youtube,
                id: id.to_owned(),
                canonical: format!("https://www.youtube.com/watch?v={id}"),
            });
        }
    }

    // 2. Instagram
    if matches!(
        host_lc.as_str(),
        "instagram.com" | "www.instagram.com" | "m.instagram.com" | "instagr.am" | "www.instagr.am"
    ) {
        let p_lc = path.to_ascii_lowercase();
        let ig_match = [
            ("/reels/", "/reel/"),
            ("/reel/", "/reel/"),
            ("/p/", "/p/"),
            ("/tv/", "/tv/"),
        ]
        .iter()
        .find_map(|(pfx, norm_pfx)| {
            if p_lc.starts_with(pfx) {
                let id = path[pfx.len()..].split('/').next()?;
                valid_link_id(id, 5).then_some((*norm_pfx, id))
            } else {
                None
            }
        });
        if let Some((norm_pfx, id)) = ig_match {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Instagram,
                id: id.to_owned(),
                canonical: format!("https://www.instagram.com{norm_pfx}{id}/"),
            });
        }
    }

    // 3. Facebook
    if matches!(
        host_lc.as_str(),
        "facebook.com" | "www.facebook.com" | "m.facebook.com" | "web.facebook.com" | "fb.watch"
    ) {
        let p_lc = path.to_ascii_lowercase();
        if host_lc == "fb.watch"
            && let Some(id) = path.strip_prefix('/').and_then(|s| s.split('/').next())
            && valid_link_id(id, 1)
        {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Facebook,
                id: id.to_owned(),
                canonical: format!("https://fb.watch/{id}/"),
            });
        }
        if p_lc == "/watch"
            && let Some(id) = query_param(query, "v")
            && valid_link_id(id, 1)
        {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Facebook,
                id: id.to_owned(),
                canonical: format!("https://www.facebook.com/watch?v={id}"),
            });
        }
        if p_lc.starts_with("/share/v/")
            && let Some(id) = path["/share/v/".len()..].split('/').next()
            && valid_link_id(id, 1)
        {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Facebook,
                id: id.to_owned(),
                canonical: format!("https://www.facebook.com/share/v/{id}/"),
            });
        }
        if p_lc.contains("/videos/")
            && let Some(id) = path.rsplit('/').find(|seg| !seg.is_empty())
            && !id.is_empty()
            && id.len() <= MAX_ID_LEN
            && id.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(ClassifiedUrl {
                provider: UnfurlProvider::Facebook,
                id: id.to_owned(),
                canonical: format!("https://www.facebook.com/videos/{id}/"),
            });
        }
    }

    // 4. Fallback: Image or Page
    let digest = Sha256::digest(cleaned.as_bytes());
    let id = hex_encode(&digest[..8]);
    let provider = if is_direct_image_url(cleaned) {
        UnfurlProvider::Image
    } else {
        UnfurlProvider::Page
    };
    Some(ClassifiedUrl {
        provider,
        id,
        canonical: cleaned.to_owned(),
    })
}

/// Extracts the first unfurlable URL from a message body (skipping direct image unfurl if message
/// already carries native media, `main.zig:8660-8682`).
pub fn extract_first_unfurl_url(body: &str, has_native_media: bool) -> Option<ClassifiedUrl> {
    for raw_token in body.split([' ', '\t', '\r', '\n', '<', '>']) {
        // Support Slack mrkdwn `<url|label>` by inspecting the URL half before `|`.
        let candidate = raw_token.split('|').next().unwrap_or(raw_token);
        if let Some(classified) = classify_url(candidate) {
            if classified.provider == UnfurlProvider::Image && has_native_media {
                continue;
            }
            return Some(classified);
        }
    }
    None
}

/// Strips the first token in `body` classifying to the same `(provider, id)` as `target` so the
/// message bubble does not duplicate a bare URL already shown as a rich preview card
/// (`chat_screen.zig:151-202`).
pub fn omit_unfurled_url_from_body(body: &str, target: &ClassifiedUrl) -> String {
    let mut best_span: Option<(usize, usize)> = None;
    let mut start = 0usize;
    let bytes = body.as_bytes();
    while start < bytes.len() {
        while start < bytes.len() && matches!(bytes[start], b' ' | b'\t' | b'\r' | b'\n') {
            start += 1;
        }
        if start >= bytes.len() {
            break;
        }
        // Recognize Slack mrkdwn `<url>` or `<url|multi-word label>` spans on the same line
        // without splitting at spaces inside the label. Whitespace is disallowed before `|` so
        // stray `< ` prose never swallows a later bare URL.
        let mut mrkdwn_end: Option<usize> = None;
        if bytes[start] == b'<' {
            let mut scan = start + 1;
            let mut saw_pipe = false;
            while scan < bytes.len() {
                match bytes[scan] {
                    b'>' => {
                        mrkdwn_end = Some(scan + 1);
                        break;
                    }
                    b'|' => {
                        saw_pipe = true;
                    }
                    b'\r' | b'\n' | b'<' => break,
                    b' ' | b'\t' if !saw_pipe => break,
                    _ => {}
                }
                scan += 1;
            }
        }
        let end = if let Some(m_end) = mrkdwn_end {
            m_end
        } else {
            let mut e = start;
            while e < bytes.len() && !matches!(bytes[e], b' ' | b'\t' | b'\r' | b'\n') {
                e += 1;
            }
            e
        };
        let token = &body[start..end];
        let trimmed = token.trim_matches(|c| matches!(c, '<' | '>'));
        let url_part = trimmed.split('|').next().unwrap_or(trimmed);
        if let Some(cls) = classify_url(url_part)
            && cls.provider == target.provider
            && cls.id == target.id
        {
            best_span = Some((start, end));
            break;
        }
        start = end;
    }

    let Some((s, e)) = best_span else {
        return body.to_owned();
    };
    let before = body[..s].trim_end();
    let after = body[e..].trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => after.to_owned(),
        (false, true) => before.to_owned(),
        (false, false) => format!("{before} {after}"),
    }
}

/// Returns `true` iff `ip` is a globally routable public IP (rejects loopback, RFC1918, CGNAT,
/// link-local, multicast, unspecified, broadcast, ULA, and IPv4-mapped private IPv6).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

pub const fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    if a == 0 {
        return false; // 0.0.0.0/8 unspecified
    }
    if a == 10 {
        return false; // 10.0.0.0/8 RFC1918
    }
    if a == 100 && matches!(b, 64..=127) {
        return false; // 100.64.0.0/10 CGNAT
    }
    if a == 127 {
        return false; // 127.0.0.0/8 loopback
    }
    if a == 169 && b == 254 {
        return false; // 169.254.0.0/16 link-local / cloud metadata
    }
    if a == 172 && matches!(b, 16..=31) {
        return false; // 172.16.0.0/12 RFC1918
    }
    if a == 192 && b == 168 {
        return false; // 192.168.0.0/16 RFC1918
    }
    if a >= 224 {
        return false; // 224.0.0.0/4 multicast + 240.0.0.0/4 reserved/broadcast
    }
    true
}

pub const fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segs = ip.segments();
    // Unspecified `::` or loopback `::1`
    if ip.is_unspecified() || ip.is_loopback() {
        return false;
    }
    // Multicast `ff00::/8`
    if (segs[0] & 0xFF00) == 0xFF00 {
        return false;
    }
    // Unique Local Address (ULA) `fc00::/7`
    if (segs[0] & 0xFE00) == 0xFC00 {
        return false;
    }
    // Link-local `fe80::/10`
    if (segs[0] & 0xFFC0) == 0xFE80 {
        return false;
    }
    // IPv4-mapped `::ffff:0:0/96` or IPv4-compatible `::0:0/96`
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    if segs[0] == 0 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0 && segs[4] == 0 && segs[5] == 0
    {
        let v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xFF) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xFF) as u8,
        );
        return is_public_ipv4(v4);
    }
    true
}

/// Strict SSRF URL fetch guard (`unfurl.zig:307-335` + IPv6/octal/hex hardening).
pub fn is_page_fetch_allowed(url_str: &str) -> bool {
    if url_str.is_empty()
        || url_str
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\\') || b < 0x20 || b == 0x7F)
    {
        return false;
    }
    let Some((_, rest)) = split_scheme(url_str) else {
        return false;
    };
    let Some((host, _)) = split_host_and_rest(rest) else {
        return false;
    };
    // Also verify with `url::Url` parser so mismatched authority / IPv6 literals are rejected.
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    if matches!(parsed.host(), Some(url::Host::Ipv6(_))) {
        return false;
    }

    let lower = host.to_ascii_lowercase();
    if lower.is_empty()
        || !lower.contains('.')
        || lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.starts_with("0x")
    {
        return false;
    }
    // Reject hex/octal labels anywhere in dotted IP forms (e.g., `0x7f.0.0.1`, `0177.0.0.1`).
    if lower.split('.').all(|seg| {
        !seg.is_empty()
            && (seg.bytes().all(|b| b.is_ascii_digit())
                || seg.strip_prefix("0x").is_some_and(|hex| {
                    !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
                }))
    }) {
        let parts: Vec<&str> = lower.split('.').collect();
        if parts.len() != 4 {
            return false;
        }
        let mut octets = [0u8; 4];
        for (idx, part) in parts.iter().enumerate() {
            if part.starts_with("0x") || (part.len() > 1 && part.starts_with('0')) {
                return false;
            }
            let Ok(v) = part.parse::<u8>() else {
                return false;
            };
            octets[idx] = v;
        }
        return is_public_ipv4(Ipv4Addr::from(octets));
    }

    true
}

fn has_valid_dns_labels(host: &str) -> bool {
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Exact port of `unfurl.isStreamUrlAllowed` (`unfurl.zig:270-272, 337-346`) hardened against
/// control bytes, whitespace, backslashes, empty DNS labels, and invalid label hyphens:
/// requires `https://`, no `@` or port, valid DNS labels, and strict subdomain suffix match
/// against `.googlevideo.com`, `.cdninstagram.com`, or `.fbcdn.net`.
pub fn is_stream_url_allowed(url_str: &str) -> bool {
    if url_str.is_empty()
        || url_str
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\\') || b < 0x20 || b == 0x7F)
    {
        return false;
    }
    let Some(("https", rest)) = split_scheme(url_str) else {
        return false;
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    if authority.is_empty() || authority.contains('@') || authority.contains(':') {
        return false;
    }
    // Strip at most ONE trailing FQDN dot (`..` trailing must fail).
    let host_raw = authority.strip_suffix('.').unwrap_or(authority);
    let host = host_raw.to_ascii_lowercase();
    if !has_valid_dns_labels(&host) {
        return false;
    }
    // Cross-check with `url::Url` to prevent any authority parser differential.
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        return false;
    }
    for suffix in [".googlevideo.com", ".cdninstagram.com", ".fbcdn.net"] {
        if host.len() > suffix.len() && host.ends_with(suffix) {
            return true;
        }
    }
    false
}

/// Decodes common HTML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, `&apos;`) safely.
pub fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            let rest = &input[i..];
            if rest.starts_with("&amp;") {
                out.push('&');
                i += 5;
                continue;
            } else if rest.starts_with("&lt;") {
                out.push('<');
                i += 4;
                continue;
            } else if rest.starts_with("&gt;") {
                out.push('>');
                i += 4;
                continue;
            } else if rest.starts_with("&quot;") {
                out.push('"');
                i += 6;
                continue;
            } else if rest.starts_with("&#39;") {
                out.push('\'');
                i += 5;
                continue;
            } else if rest.starts_with("&apos;") {
                out.push('\'');
                i += 6;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Exact port of `unfurl.resolveImageUrl` (`unfurl.zig:290-305`): safely resolves relative image
/// URLs against `page_url` while rejecting `javascript:`, `data:`, `vbscript:`, etc.
pub fn resolve_image_url(page_url: &str, image: &str) -> Option<String> {
    let trimmed = image.trim();
    if trimmed.is_empty() {
        return None;
    }
    if split_scheme(trimmed).is_some() {
        return Some(trimmed.to_owned());
    }
    let (scheme, rest) = split_scheme(page_url)?;
    if trimmed.starts_with("//") {
        return Some(format!("{scheme}:{trimmed}"));
    }
    // Reject non-HTTP schemes like `javascript:`, `data:`, `vbscript:`
    if trimmed.contains(':') {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = format!("{scheme}://{}", &rest[..authority_end]);
    if trimmed.starts_with('/') {
        return Some(format!("{origin}{trimmed}"));
    }
    let path = rest[authority_end..].split(['?', '#']).next().unwrap_or("");
    if let Some(last_slash) = path.rfind('/') {
        Some(format!("{origin}{}{trimmed}", &path[..=last_slash]))
    } else {
        Some(format!("{origin}/{trimmed}"))
    }
}

/// Bounded zero-panic `<meta ...>` OG tag extractor (`unfurl.zig:348-383`):
/// handles attributes in any order (`property`/`name` + `content`), whitespace around `=`,
/// single/double/unquoted values, and truncated tags cleanly.
pub fn scrape_og_meta(html: &str, key: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i + 5 < bytes.len() {
        if bytes[i] == b'<'
            && bytes[i + 1..i + 5].eq_ignore_ascii_case(b"meta")
            && bytes[i + 5].is_ascii_whitespace()
        {
            i += 6;
            let mut prop_val: Option<&str> = None;
            let mut content_val: Option<&str> = None;

            while i < bytes.len() {
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] == b'>' {
                    if i < bytes.len() {
                        i += 1;
                    }
                    break;
                }
                if bytes[i] == b'/' {
                    i += 1;
                    continue;
                }
                // Read attribute name
                let name_start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'='
                    && bytes[i] != b'>'
                    && bytes[i] != b'/'
                {
                    i += 1;
                }
                let attr_name = &html[name_start..i];
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b'=' {
                    continue;
                }
                i += 1; // consume '='
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }
                let val = if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let quote = bytes[i];
                    i += 1;
                    let val_start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        break; // truncated quote -> ignore tag safely
                    }
                    let slice = &html[val_start..i];
                    i += 1;
                    slice
                } else {
                    let val_start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                        i += 1;
                    }
                    &html[val_start..i]
                };

                if attr_name.eq_ignore_ascii_case("property")
                    || attr_name.eq_ignore_ascii_case("name")
                {
                    prop_val = Some(val);
                } else if attr_name.eq_ignore_ascii_case("content") {
                    content_val = Some(val);
                }
            }

            if let (Some(p), Some(c)) = (prop_val, content_val)
                && p.eq_ignore_ascii_case(key)
                && !c.is_empty()
            {
                return Some(decode_html_entities(c));
            }
            continue;
        }
        i += 1;
    }
    None
}

/// Extracts a YouTube `https://www.googlevideo.com/videoplayback...` stream URL from watch HTML
/// (`unfurl.zig:385-412`), validating through `is_stream_url_allowed`.
pub fn extract_youtube_stream(html: &str) -> Option<String> {
    const NEEDLE: &str = "\"https://www.googlevideo.com/videoplayback";
    let start = html.find(NEEDLE)? + 1;
    let rest = &html[start..];
    let bytes = rest.as_bytes();
    let mut unescaped = String::with_capacity(256);
    let mut i = 0usize;
    while i < bytes.len() && unescaped.len() <= MAX_PLAY_URL_BYTES * 2 {
        if bytes[i] == b'"' {
            break;
        }
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i..].starts_with(b"\\u0026") {
                unescaped.push('&');
                i += 6;
                continue;
            }
            i += 1;
        }
        let ch = rest[i..].chars().next()?;
        unescaped.push(ch);
        i += ch.len_utf8();
    }
    if unescaped.len() <= MAX_PLAY_URL_BYTES && is_stream_url_allowed(&unescaped) {
        Some(unescaped)
    } else {
        None
    }
}

pub fn youtube_oembed_url(canonical: &str) -> String {
    let encoded = postbox_hub::encode_uri_component(canonical);
    format!("https://www.youtube.com/oembed?url={encoded}&format=json")
}

pub fn youtube_thumbnail_url(video_id: &str) -> String {
    format!("https://img.youtube.com/vi/{video_id}/hqdefault.jpg")
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskUnfurlMeta {
    title: String,
    author: String,
    description: String,
    #[serde(default)]
    play_url: String,
    #[serde(default)]
    image: bool,
}

/// 24-entry FIFO in-memory ring cache (`main.zig:422, 8699-8728`).
#[derive(Debug, Default)]
pub struct UnfurlRingCache {
    entries: VecDeque<((UnfurlProvider, String), UnfurlCard)>,
}

impl UnfurlRingCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, provider: UnfurlProvider, id: &str) -> Option<&UnfurlCard> {
        self.entries
            .iter()
            .find(|((p, k), _)| *p == provider && k == id)
            .map(|(_, card)| card)
    }

    pub fn insert(&mut self, card: UnfurlCard) {
        let key = (card.provider, card.id.clone());
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries[pos].1 = card;
            return;
        }
        if self.entries.len() >= MAX_UNFURL_RING_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, card));
    }
}

#[derive(Debug)]
struct SsrfBlockedDnsError;

impl std::fmt::Display for SsrfBlockedDnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ssrf_blocked")
    }
}

impl std::error::Error for SsrfBlockedDnsError {}

fn error_chain_has_ssrf_blocked(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.is::<SsrfBlockedDnsError>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Custom `reqwest::dns::Resolve` eliminating DNS-rebinding TOCTOU by validating every resolved
/// `SocketAddr` inline inside `reqwest`'s connection pipeline (allowing loopback ONLY when
/// `allow_loopback_for_test` is true).
struct PublicOnlyDnsResolver {
    allow_loopback_for_test: bool,
}

impl Resolve for PublicOnlyDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback_for_test;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let mut allowed = Vec::new();
            for addr in resolved {
                let ip = addr.ip();
                let ok = is_public_ip(ip) || (allow_loopback && ip.is_loopback());
                if !ok {
                    return Err(
                        Box::new(SsrfBlockedDnsError) as Box<dyn std::error::Error + Send + Sync>
                    );
                }
                allowed.push(addr);
            }
            if allowed.is_empty() {
                return Err(
                    Box::new(SsrfBlockedDnsError) as Box<dyn std::error::Error + Send + Sync>
                );
            }
            let iter: Addrs = Box::new(allowed.into_iter());
            Ok(iter)
        })
    }
}

/// SSRF-safe async link unfurler with atomic `link-previews/` disk cache, inline DNS resolved-IP
/// validation via `PublicOnlyDnsResolver`, no-redirects HTTP client, and 3-slot semaphore.
#[derive(Clone)]
pub struct UnfurlService {
    cache_dir: PathBuf,
    client: reqwest::Client,
    slots: Arc<Semaphore>,
    allow_loopback_for_test: bool,
    youtube_oembed_override_base: Option<String>,
}

impl UnfurlService {
    pub fn new(cache_dir: PathBuf) -> Result<Self, MediaError> {
        Self::build(cache_dir, false, None)
    }

    /// Creates a loopback-enabled `UnfurlService` strictly for deterministic loopback integration
    /// tests (matching `verificationUnfurlUrl` in `main.zig:8956-8961`).
    pub fn new_for_loopback_test(
        cache_dir: PathBuf,
        youtube_oembed_override_base: Option<String>,
    ) -> Result<Self, MediaError> {
        Self::build(cache_dir, true, youtube_oembed_override_base)
    }

    fn build(
        cache_dir: PathBuf,
        allow_loopback_for_test: bool,
        youtube_oembed_override_base: Option<String>,
    ) -> Result<Self, MediaError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(UNFURL_USER_AGENT)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .dns_resolver(Arc::new(PublicOnlyDnsResolver {
                allow_loopback_for_test,
            }));
        if allow_loopback_for_test {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| MediaError::Transport(e.to_string()))?;
        Ok(Self {
            cache_dir,
            client,
            slots: Arc::new(Semaphore::new(MAX_UNFURL_CONCURRENT)),
            allow_loopback_for_test,
            youtube_oembed_override_base,
        })
    }

    pub fn cache_paths(&self, provider: UnfurlProvider, id: &str) -> (PathBuf, PathBuf) {
        let json_path = self
            .cache_dir
            .join(format!("{}-{id}.json", provider.as_str()));
        let img_path = self
            .cache_dir
            .join(format!("{}-{id}.json.image", provider.as_str()));
        (json_path, img_path)
    }

    /// Loads an unfurl card from `link-previews/` disk cache (`main.zig:8906-8954`).
    /// If `"image": true` in JSON, the `.json.image` file MUST also exist cleanly.
    pub fn load_from_disk(&self, target: &ClassifiedUrl) -> Option<UnfurlCard> {
        let (json_path, img_path) = self.cache_paths(target.provider, &target.id);
        let meta_bytes = std::fs::read(&json_path).ok()?;
        if meta_bytes.len() > MAX_UNFURL_JSON_BYTES {
            return None;
        }
        let meta: DiskUnfurlMeta = serde_json::from_slice(&meta_bytes).ok()?;
        let thumb_path = if meta.image {
            let img_meta = std::fs::metadata(&img_path).ok()?;
            if !img_meta.is_file() || img_meta.len() as usize > MAX_UNFURL_IMAGE_BYTES {
                return None;
            }
            Some(img_path)
        } else {
            None
        };
        Some(UnfurlCard {
            provider: target.provider,
            id: target.id.clone(),
            canonical: target.canonical.clone(),
            site: site_host(&target.canonical),
            title: meta.title,
            author: meta.author,
            description: meta.description,
            play_url: (!meta.play_url.is_empty()).then_some(meta.play_url),
            thumb_path,
        })
    }

    fn save_to_disk(
        &self,
        card: &UnfurlCard,
        thumb_bytes: Option<&[u8]>,
    ) -> Result<Option<PathBuf>, MediaError> {
        let (json_path, img_path) = self.cache_paths(card.provider, &card.id);
        let written_thumb = if let Some(bytes) = thumb_bytes {
            atomic_write_file(&img_path, bytes)?;
            Some(img_path)
        } else {
            None
        };
        let meta = DiskUnfurlMeta {
            title: card.title.clone(),
            author: card.author.clone(),
            description: card.description.clone(),
            play_url: card.play_url.clone().unwrap_or_default(),
            image: written_thumb.is_some(),
        };
        let json_bytes =
            serde_json::to_vec(&meta).map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
        atomic_write_file(&json_path, &json_bytes)?;
        Ok(written_thumb)
    }

    fn validate_fetch_target(&self, url_str: &str) -> Result<(), MediaError> {
        if self.allow_loopback_for_test {
            let parsed = url::Url::parse(url_str).map_err(|_| MediaError::InvalidUrl)?;
            if parsed.host_str() == Some("127.0.0.1") {
                return Ok(());
            }
        }
        if !is_page_fetch_allowed(url_str) {
            return Err(MediaError::SsrfBlocked);
        }
        Ok(())
    }

    async fn http_get_checked(
        &self,
        url_str: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, MediaError> {
        self.validate_fetch_target(url_str)?;
        let resp = self
            .client
            .get(url_str)
            .header(ACCEPT, "*/*")
            .send()
            .await
            .map_err(|e| {
                if error_chain_has_ssrf_blocked(&e) {
                    MediaError::SsrfBlocked
                } else {
                    MediaError::Transport(e.to_string())
                }
            })?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(MediaError::HttpStatus(status));
        }
        if let Some(cl) = resp.content_length()
            && cl > max_bytes as u64
        {
            return Err(MediaError::FileTooLarge);
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| MediaError::Transport(e.to_string()))?;
            if out.len().saturating_add(chunk.len()) > max_bytes {
                return Err(MediaError::FileTooLarge);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Fetches or loads an unfurl card for `target` respecting the 3-slot semaphore and disk cache.
    pub async fn fetch_or_load(&self, target: &ClassifiedUrl) -> Result<UnfurlCard, MediaError> {
        if let Some(cached) = self.load_from_disk(target) {
            return Ok(cached);
        }
        let _permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Cancelled)?;

        let site = site_host(&target.canonical);
        match target.provider {
            UnfurlProvider::Youtube => {
                let oembed_url = match &self.youtube_oembed_override_base {
                    Some(base) => format!(
                        "{base}/oembed?url={}&format=json",
                        postbox_hub::encode_uri_component(&target.canonical)
                    ),
                    None => youtube_oembed_url(&target.canonical),
                };
                let mut title = String::new();
                let mut author = String::new();
                if let Ok(bytes) = self.http_get_checked(&oembed_url, MAX_OEMBED_BYTES).await
                    && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes)
                {
                    if let Some(t) = val.get("title").and_then(serde_json::Value::as_str) {
                        title = truncate_utf8(t, MAX_TITLE_BYTES);
                    }
                    if let Some(a) = val.get("author_name").and_then(serde_json::Value::as_str) {
                        author = truncate_utf8(a, MAX_AUTHOR_BYTES);
                    }
                }
                let watch_url = match &self.youtube_oembed_override_base {
                    Some(base) => format!("{base}/watch?v={}", target.id),
                    None => target.canonical.clone(),
                };
                let play_url = self
                    .http_get_checked(&watch_url, MAX_YT_WATCH_BYTES)
                    .await
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .and_then(|html| extract_youtube_stream(&html));

                let thumb_url = match &self.youtube_oembed_override_base {
                    Some(base) => format!("{base}/vi/{}/hqdefault.jpg", target.id),
                    None => youtube_thumbnail_url(&target.id),
                };
                let thumb_bytes = self
                    .http_get_checked(&thumb_url, MAX_UNFURL_IMAGE_BYTES)
                    .await
                    .ok();

                if title.is_empty() && play_url.is_none() && thumb_bytes.is_none() {
                    return Err(MediaError::Transport(
                        "youtube_unfurl_all_requests_failed".into(),
                    ));
                }

                let mut card = UnfurlCard {
                    provider: target.provider,
                    id: target.id.clone(),
                    canonical: target.canonical.clone(),
                    site,
                    title,
                    author,
                    description: String::new(),
                    play_url,
                    thumb_path: None,
                };
                card.thumb_path = self.save_to_disk(&card, thumb_bytes.as_deref())?;
                Ok(card)
            }
            UnfurlProvider::Image => {
                let bytes = self
                    .http_get_checked(&target.canonical, MAX_UNFURL_IMAGE_BYTES)
                    .await?;
                let mut card = UnfurlCard {
                    provider: UnfurlProvider::Image,
                    id: target.id.clone(),
                    canonical: target.canonical.clone(),
                    site,
                    title: String::new(),
                    author: String::new(),
                    description: String::new(),
                    play_url: None,
                    thumb_path: None,
                };
                card.thumb_path = self.save_to_disk(&card, Some(&bytes))?;
                Ok(card)
            }
            UnfurlProvider::Page | UnfurlProvider::Instagram | UnfurlProvider::Facebook => {
                let html_bytes = self
                    .http_get_checked(&target.canonical, MAX_PAGE_HTML_BYTES)
                    .await?;
                let html = String::from_utf8_lossy(&html_bytes);
                let mut title = scrape_og_meta(&html, "og:title")
                    .map(|s| truncate_utf8(&s, MAX_TITLE_BYTES))
                    .unwrap_or_default();
                let description = scrape_og_meta(&html, "og:description")
                    .map(|s| truncate_utf8(&s, MAX_TITLE_BYTES))
                    .unwrap_or_default();
                let author = if target.provider == UnfurlProvider::Page {
                    site.clone()
                } else {
                    String::new()
                };
                if target.provider == UnfurlProvider::Page && title.is_empty() {
                    title = author.clone();
                }
                let play_url = if matches!(
                    target.provider,
                    UnfurlProvider::Instagram | UnfurlProvider::Facebook
                ) {
                    scrape_og_meta(&html, "og:video")
                        .filter(|u| u.len() <= MAX_PLAY_URL_BYTES && is_stream_url_allowed(u))
                } else {
                    None
                };
                let raw_img = scrape_og_meta(&html, "og:image:secure_url")
                    .or_else(|| scrape_og_meta(&html, "og:image"));
                let thumb_bytes = match raw_img
                    .as_deref()
                    .and_then(|img| resolve_image_url(&target.canonical, img))
                {
                    Some(img_url) => self
                        .http_get_checked(&img_url, MAX_UNFURL_IMAGE_BYTES)
                        .await
                        .ok(),
                    None => None,
                };

                let mut card = UnfurlCard {
                    provider: target.provider,
                    id: target.id.clone(),
                    canonical: target.canonical.clone(),
                    site,
                    title,
                    author,
                    description,
                    play_url,
                    thumb_path: None,
                };
                card.thumb_path = self.save_to_disk(&card, thumb_bytes.as_deref())?;
                Ok(card)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard_blocks_all_private_loopback_cgnat_ipv6_octal_hex_userinfo_and_spoofed_cdns() {
        for blocked in [
            "http://localhost/secret",
            "https://sub.localhost/secret",
            "https://printer.local/admin",
            "http://intranet/page",
            "http://127.0.0.1/meta",
            "http://127.1.2.3/meta",
            "http://10.0.0.1/internal",
            "http://172.16.0.1/internal",
            "http://172.31.255.254/internal",
            "http://192.168.1.1/router",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/cgnat",
            "http://100.127.255.254/cgnat",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "http://255.255.255.255/",
            "http://[::1]/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://0x7f000001/",
            "http://0x7f.0.0.1/",
            "http://0177.0.0.1/",
            "https://user:pass@youtube.com/watch?v=dQw4w9WgXcQ",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://example.com/a b",
        ] {
            assert!(
                !is_page_fetch_allowed(blocked),
                "MUST block SSRF URL: {blocked}"
            );
        }

        // Public hosts and public IPv4 allowed
        assert!(is_page_fetch_allowed("https://example.com/article"));
        assert!(is_page_fetch_allowed("https://8.8.8.8/dns"));
        assert!(is_page_fetch_allowed("https://172.32.0.1/public"));

        // IPv6 unit checks for DNS-resolved IPs
        assert!(!is_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_public_ip(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6("ff02::1".parse().unwrap())));
        assert!(!is_public_ip(IpAddr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        )));
        assert!(!is_public_ip(IpAddr::V6(
            "::ffff:10.0.0.1".parse().unwrap()
        )));
        assert!(is_public_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));

        // Stream CDN allowlist
        assert!(is_stream_url_allowed(
            "https://rr1---sn.googlevideo.com/videoplayback?id=1"
        ));
        assert!(is_stream_url_allowed(
            "https://scontent.cdninstagram.com/v/reel.mp4"
        ));
        assert!(is_stream_url_allowed("https://video.fbcdn.net/v/123.mp4"));
        assert!(!is_stream_url_allowed(
            "http://rr1.googlevideo.com/videoplayback"
        ));
        assert!(!is_stream_url_allowed(
            "https://googlevideo.com/videoplayback"
        ));
        assert!(!is_stream_url_allowed(
            "https://www.googlevideo.com.evil.com/videoplayback"
        ));
        assert!(!is_stream_url_allowed(
            "https://evil.com/?x=.googlevideo.com"
        ));
    }

    #[test]
    fn url_classifier_og_parser_relative_image_and_url_omission() {
        let yt = classify_url("https://youtu.be/dQw4w9WgXcQ!").unwrap();
        assert_eq!(yt.provider, UnfurlProvider::Youtube);
        assert_eq!(yt.canonical, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");

        let ig = classify_url("https://www.instagram.com/reels/Cx12345/").unwrap();
        assert_eq!(ig.provider, UnfurlProvider::Instagram);
        assert_eq!(ig.canonical, "https://www.instagram.com/reel/Cx12345/");

        let fb = classify_url("https://www.facebook.com/page/videos/987654321/").unwrap();
        assert_eq!(fb.provider, UnfurlProvider::Facebook);
        assert_eq!(fb.canonical, "https://www.facebook.com/videos/987654321/");

        // Spoofed YouTube domain falls back to Page, NOT Youtube
        let spoof = classify_url("https://youtube.com.evil.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(spoof.provider, UnfurlProvider::Page);

        // OG parser with reversed attributes, single quotes, entities, and truncated tail
        let html = r#"<html><head>
            <meta content='Cats &amp; Dogs &lt;3' property="og:title">
            <meta property="og:image" content="/assets/card.png">
            <meta property="og:description" content="Truncated"#;
        assert_eq!(
            scrape_og_meta(html, "og:title").as_deref(),
            Some("Cats & Dogs <3")
        );
        assert_eq!(
            scrape_og_meta(html, "og:image").as_deref(),
            Some("/assets/card.png")
        );
        assert_eq!(scrape_og_meta(html, "og:description"), None);

        // Relative URL resolution blocks javascript: and data:
        assert_eq!(
            resolve_image_url("https://example.com/blog/post", "/img/a.png").as_deref(),
            Some("https://example.com/img/a.png")
        );
        assert_eq!(
            resolve_image_url("https://example.com/blog/post", "thumb.jpg").as_deref(),
            Some("https://example.com/blog/thumb.jpg")
        );
        assert_eq!(
            resolve_image_url("https://example.com/blog", "javascript:alert(1)"),
            None
        );
        assert_eq!(
            resolve_image_url("https://example.com/blog", "data:image/png;base64,AA"),
            None
        );

        // URL omission in message bubble
        assert_eq!(
            omit_unfurled_url_from_body("watch this https://youtu.be/dQw4w9WgXcQ please", &yt),
            "watch this please"
        );
        assert_eq!(
            omit_unfurled_url_from_body("https://youtu.be/dQw4w9WgXcQ", &yt),
            ""
        );

        // 24-entry FIFO ring eviction
        let mut ring = UnfurlRingCache::new();
        for i in 0..30 {
            ring.insert(UnfurlCard {
                provider: UnfurlProvider::Page,
                id: format!("id_{i}"),
                canonical: format!("https://example.com/{i}"),
                site: "example.com".into(),
                title: format!("T{i}"),
                author: String::new(),
                description: String::new(),
                play_url: None,
                thumb_path: None,
            });
        }
        assert_eq!(ring.len(), MAX_UNFURL_RING_ENTRIES);
        assert!(ring.get(UnfurlProvider::Page, "id_0").is_none());
        assert!(ring.get(UnfurlProvider::Page, "id_29").is_some());
    }

    #[test]
    fn unfurl_handles_multibyte_utf8_words_without_panicking() {
        for sample in [
            "Frühstück",
            "Kommst du zum Frühstück?",
            "🎉🎉🎉",
            "aaaaaaÜb",
            "日本語テスト",
            "https://example.com/Frühstück",
        ] {
            let cls = classify_url(sample);
            let extracted = extract_first_unfurl_url(sample, false);
            if let Some(c) = extracted {
                let _ = omit_unfurled_url_from_body(sample, &c);
            }
            assert!(!is_stream_url_allowed(sample));
            if sample.starts_with("https://") {
                assert!(cls.is_some());
            } else {
                assert!(cls.is_none());
            }
        }
    }

    #[test]
    fn stream_url_allowlist_rejects_empty_labels_crlf_whitespace_and_invalid_hyphens() {
        for bad in [
            "https://..googlevideo.com/videoplayback",
            "https://a..googlevideo.com/v",
            "https://rr1.googlevideo.com../v",
            "https://-bad.googlevideo.com/v",
            "https://bad-.googlevideo.com/v",
            "https://rr1.googlevideo.com/video\r\nInjected: 1",
            "https://rr1.googlevideo.com/video with spaces",
            "https://rr1.googlevideo.com/video\t1",
            "https://rr1.googlevideo.com\\@evil.com/v",
            "https://rr1.googlevideo.com/video\x00",
            "https://rr1.googlevideo.com/video\x7f",
        ] {
            assert!(!is_stream_url_allowed(bad), "should reject {bad:?}");
        }
        for good in [
            "https://rr1---sn.googlevideo.com/videoplayback?id=1",
            "https://rr1.googlevideo.com./videoplayback",
            "https://scontent.cdninstagram.com/v/reel.mp4",
            "https://video.fbcdn.net/v/123.mp4",
        ] {
            assert!(is_stream_url_allowed(good), "should allow {good:?}");
        }
    }

    #[tokio::test]
    async fn public_only_dns_resolver_blocks_localhost_by_default_and_allows_in_test_mode() {
        let strict = PublicOnlyDnsResolver {
            allow_loopback_for_test: false,
        };
        let name: Name = "localhost".parse().unwrap();
        let err = strict
            .resolve(name)
            .await
            .err()
            .expect("strict resolver must reject localhost");
        assert!(error_chain_has_ssrf_blocked(&*err));

        let test_mode = PublicOnlyDnsResolver {
            allow_loopback_for_test: true,
        };
        let name2: Name = "localhost".parse().unwrap();
        let addrs: Vec<_> = test_mode
            .resolve(name2)
            .await
            .expect("test resolver must allow localhost")
            .collect();
        assert!(addrs.iter().any(|a| a.ip().is_loopback()));
    }

    #[test]
    fn f7_omit_unfurled_url_handles_slack_mrkdwn_multiword_and_utf8_labels() {
        let yt = classify_url("https://youtu.be/dQw4w9WgXcQ").unwrap();

        assert_eq!(
            omit_unfurled_url_from_body("See <https://youtu.be/dQw4w9WgXcQ|Click here> now", &yt),
            "See now"
        );
        assert_eq!(
            omit_unfurled_url_from_body(
                "Schau <https://youtu.be/dQw4w9WgXcQ|Übung hier 🎉> an",
                &yt
            ),
            "Schau an"
        );
        assert_eq!(
            omit_unfurled_url_from_body("<https://youtu.be/dQw4w9WgXcQ|Click here right now>", &yt),
            ""
        );
        assert_eq!(
            omit_unfurled_url_from_body(
                "Keep <https://example.com/other|Two words> and <https://youtu.be/dQw4w9WgXcQ|Drop this label> end",
                &yt
            ),
            "Keep <https://example.com/other|Two words> and end"
        );
        // Bare URL and single-word mrkdwn still omitted cleanly
        assert_eq!(
            omit_unfurled_url_from_body("See https://youtu.be/dQw4w9WgXcQ now", &yt),
            "See now"
        );
        assert_eq!(
            omit_unfurled_url_from_body("See <https://youtu.be/dQw4w9WgXcQ|watch> now", &yt),
            "See now"
        );
        // Stray `< ` comparison prose before bare URL must not swallow the bare URL
        assert_eq!(
            omit_unfurled_url_from_body("a < b https://youtu.be/dQw4w9WgXcQ > c", &yt),
            "a < b > c"
        );
    }
}
