use crate::error::HubError;
use crate::types::{HubRequest, SendFileRequest};
use postbox_core::crash::CrashReport;

pub const MAX_ID_LEN: usize = 191;
pub const MAX_CLIENT_ID_LEN: usize = 128;
pub const MAX_TEXT_LEN: usize = 4095;
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_SLACK_EMOJI_INDEX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SLACK_EMOJI_MEDIA_BYTES: usize = 1024 * 1024;
pub const OLDER_PAGE_LIMIT: usize = 80;
pub const MAX_ROWS: usize = 500;
pub const MAX_CACHED_HISTORY: usize = 1000;
pub const USER_AGENT: &str = "Postbox/1.0";
/// One serialised crash report never exceeds this many bytes.
pub const MAX_CRASH_REPORT_BYTES: usize = postbox_core::crash::MAX_CRASH_REPORT_BYTES;

/// Builds the Hub-only crash report upload. The report is redacted again here, so a caller
/// that assembled one by hand still cannot ship message content or credentials.
pub fn crash_report_request(report: &CrashReport) -> Result<HubRequest, HubError> {
    let redacted = report.clone().redacted();
    if redacted.report_id.is_empty() {
        return Err(HubError::InvalidId);
    }
    let body = redacted.to_line();
    if body.len() > MAX_CRASH_REPORT_BYTES {
        return Err(HubError::InvalidSend);
    }
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/crash-reports".to_owned(),
        body: Some(body),
        last_event_id: None,
    })
}

/// Exact port of `hub.zig:361-364`: `1 <= len <= 191`, no control bytes (`< 0x20` or `0x7F`).
pub fn valid_id(value: &str) -> Result<(), HubError> {
    if value.is_empty() || value.len() > MAX_ID_LEN || value.bytes().any(|b| b < 0x20 || b == 0x7F)
    {
        return Err(HubError::InvalidId);
    }
    Ok(())
}

/// Exact port of `hub.zig:216-219`: `valid_id`, no `:`, not `.` or `..`.
pub fn valid_team(team: &str) -> Result<(), HubError> {
    valid_id(team).map_err(|_| HubError::InvalidTeam)?;
    if team.contains(':') || team == "." || team == ".." {
        return Err(HubError::InvalidTeam);
    }
    Ok(())
}

/// Exact port of `hub.zig:366-369`: `1 <= len <= 128`, no control bytes.
pub fn valid_client_id(cid: &str) -> Result<(), HubError> {
    if cid.is_empty() || cid.len() > MAX_CLIENT_ID_LEN || cid.bytes().any(|b| b < 0x20 || b == 0x7F)
    {
        return Err(HubError::InvalidClientId);
    }
    Ok(())
}

/// Exact port of `winhttp.zig:335-336`: non-empty printable ASCII `0x21..=0x7E` (no whitespace/CRLF).
pub fn valid_token(token: &str) -> Result<(), HubError> {
    if token.is_empty() || !token.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(HubError::InvalidToken);
    }
    Ok(())
}

/// Exact port of `main.zig:10983`: text UTF-8 byte length <= 4095.
pub fn valid_text(text: &str) -> Result<(), HubError> {
    if text.len() > MAX_TEXT_LEN {
        return Err(HubError::InvalidSend);
    }
    Ok(())
}

/// Exact port of SSE `Last-Event-ID` header validation: digits-only, `1..=32` chars.
pub fn valid_last_event_id(id: &str) -> Result<(), HubError> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HubError::BadEventId);
    }
    Ok(())
}

/// RFC 3986 percent-encoding (`wa_hub.zig:66-78`): unreserved `[A-Za-z0-9\-_.~]` verbatim.
pub fn encode_uri_component(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0F) as usize] as char);
        }
    }
    out
}

pub fn before_cursor(msg_id: &str, timestamp: &str) -> Result<String, HubError> {
    valid_id(msg_id)?;
    valid_id(timestamp)?;
    Ok(format!("{msg_id}:{timestamp}"))
}

pub fn chats_request(archived: bool, limit: usize) -> HubRequest {
    HubRequest {
        method: "GET",
        path: format!("/v1/wazig/chats?archived={archived}&limit={limit}"),
        body: None,
        last_event_id: None,
    }
}

pub fn messages_request(
    chat: &str,
    limit: usize,
    before: Option<&str>,
) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    let encoded_chat = encode_uri_component(chat);
    let encoded_before = before.map(encode_uri_component).unwrap_or_default();
    Ok(HubRequest {
        method: "GET",
        path: format!(
            "/v1/wazig/messages?chat={encoded_chat}&limit={limit}&before={encoded_before}"
        ),
        body: None,
        last_event_id: None,
    })
}

pub fn send_text_request(
    chat: &str,
    text: &str,
    reply_to: Option<&str>,
    reply_broadcast: bool,
    client_id: &str,
) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    valid_client_id(client_id)?;
    valid_text(text)?;
    if reply_broadcast && reply_to.is_none_or(str::is_empty) {
        return Err(HubError::InvalidSend);
    }
    let payload = if reply_broadcast {
        serde_json::json!({
            "chat": chat,
            "text": text,
            "reply_to": reply_to.unwrap_or(""),
            "client_id": client_id,
            "reply_broadcast": true,
        })
    } else {
        serde_json::json!({
            "chat": chat,
            "text": text,
            "reply_to": reply_to.unwrap_or(""),
            "client_id": client_id,
        })
    };
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/send".to_owned(),
        body: Some(payload.to_string()),
        last_event_id: None,
    })
}

pub fn mark_read_request(chat: &str, message_id: &str) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    if !message_id.is_empty() {
        valid_id(message_id)?;
    }
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/mark-read".to_owned(),
        body: Some(
            serde_json::json!({
                "chat": chat,
                "message_id": message_id,
            })
            .to_string(),
        ),
        last_event_id: None,
    })
}

pub fn archive_request(chat: &str, archived: bool) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/archive".to_owned(),
        body: Some(
            serde_json::json!({
                "chat": chat,
                "archived": archived,
            })
            .to_string(),
        ),
        last_event_id: None,
    })
}

/// Exact port of `hub.zig:124-128` & `wa_hub.zig:164-169`: includes `"account": ""`.
pub fn archive_batch_request(chats: &[&str], archived: bool) -> Result<HubRequest, HubError> {
    if chats.is_empty() {
        return Err(HubError::InvalidSend);
    }
    let mut items = Vec::with_capacity(chats.len());
    for &c in chats {
        valid_id(c)?;
        items.push(serde_json::json!({ "chat": c }));
    }
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/archive-batch".to_owned(),
        body: Some(
            serde_json::json!({
                "account": "",
                "chats": items,
                "archived": archived,
            })
            .to_string(),
        ),
        last_event_id: None,
    })
}

pub fn react_request(
    chat: &str,
    msg_id: &str,
    emoji: &str,
    client_id: &str,
) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    valid_id(msg_id)?;
    valid_client_id(client_id)?;
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/react".to_owned(),
        body: Some(
            serde_json::json!({
                "chat": chat,
                "msg": msg_id,
                "emoji": emoji,
                "client_id": client_id,
            })
            .to_string(),
        ),
        last_event_id: None,
    })
}

pub fn delete_request(chat: &str, msg_id: &str, client_id: &str) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    valid_id(msg_id)?;
    valid_client_id(client_id)?;
    Ok(HubRequest {
        method: "POST",
        path: "/v1/wazig/delete".to_owned(),
        body: Some(
            serde_json::json!({
                "chat": chat,
                "msg": msg_id,
                "client_id": client_id,
            })
            .to_string(),
        ),
        last_event_id: None,
    })
}

pub fn media_request(chat: &str, message_id: Option<&str>) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    let c = encode_uri_component(chat);
    let path = match message_id {
        Some(id) => {
            valid_id(id)?;
            format!("/v1/wazig/media/{c}/{}", encode_uri_component(id))
        }
        None => format!("/v1/wazig/avatar/{c}"),
    };
    Ok(HubRequest {
        method: "GET",
        path,
        body: None,
        last_event_id: None,
    })
}

pub fn workspace_icon_request(team: &str) -> Result<HubRequest, HubError> {
    valid_team(team)?;
    Ok(HubRequest {
        method: "GET",
        path: format!("/v1/wazig/workspace-icon/{}", encode_uri_component(team)),
        body: None,
        last_event_id: None,
    })
}

pub fn slack_emoji_index_request(team: &str) -> Result<HubRequest, HubError> {
    valid_team(team)?;
    Ok(HubRequest {
        method: "GET",
        path: format!(
            "/v1/wazig/slack/emoji?account={}",
            encode_uri_component(team)
        ),
        body: None,
        last_event_id: None,
    })
}

pub fn slack_emoji_media_request(team: &str, name: &str) -> Result<HubRequest, HubError> {
    valid_team(team)?;
    valid_id(name)?;
    Ok(HubRequest {
        method: "GET",
        path: format!(
            "/v1/wazig/slack/emoji/media?account={}&name={}",
            encode_uri_component(team),
            encode_uri_component(name)
        ),
        body: None,
        last_event_id: None,
    })
}

pub fn events_request(last_event_id: Option<&str>) -> HubRequest {
    HubRequest {
        method: "GET",
        path: "/v1/wazig/events".to_owned(),
        body: None,
        last_event_id: last_event_id.map(str::to_owned),
    }
}

/// Generates a 32-char lowercase hex multipart boundary (`main.zig:25233-25235`).
pub fn generate_boundary() -> Result<String, HubError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| HubError::InvalidBoundary)?;
    Ok(postbox_core::outbox::format_client_id(bytes))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn sanitize_wire_filename(filename: &str, ptt: bool) -> String {
    if ptt {
        return "voice.ogg".to_owned();
    }
    let sanitized: String = filename
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c == '\x7F' || matches!(c, '"' | '\\' | '/') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if sanitized.is_empty() {
        "attachment.bin".to_owned()
    } else {
        sanitized
    }
}

/// Builds the exact `/v1/wazig/send` multipart payload (`hub.zig:112-117`, `wa_hub.zig:127-152`).
/// Returns `(content_type_header, body_bytes)`.
pub fn encode_send_file_multipart(
    req: &SendFileRequest<'_>,
    boundary: &str,
) -> Result<(String, Vec<u8>), HubError> {
    valid_id(req.chat)?;
    valid_client_id(req.client_id)?;
    valid_text(req.text)?;
    if req.bytes.len() > MAX_FILE_BYTES {
        return Err(HubError::FileTooLarge);
    }
    if !(16..=80).contains(&boundary.len()) || !boundary.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(HubError::InvalidBoundary);
    }

    let wire_name = sanitize_wire_filename(req.filename, req.ptt);
    let b_bytes = boundary.as_bytes();
    if contains_subslice(req.bytes, b_bytes)
        || contains_subslice(req.text.as_bytes(), b_bytes)
        || contains_subslice(req.reply_to.as_bytes(), b_bytes)
        || contains_subslice(req.client_id.as_bytes(), b_bytes)
        || contains_subslice(b"hub", b_bytes)
        || contains_subslice(req.chat.as_bytes(), b_bytes)
        || contains_subslice(wire_name.as_bytes(), b_bytes)
    {
        return Err(HubError::InvalidBoundary);
    }

    let ptt_val = if req.ptt { "true" } else { "false" };
    let mime = if req.ptt {
        "audio/ogg"
    } else {
        "application/octet-stream"
    };

    let mut body = Vec::with_capacity(req.bytes.len() + 512);
    let text_parts: [(&str, &str); 6] = [
        ("account", "hub"),
        ("chat", req.chat),
        ("text", req.text),
        ("reply_to", req.reply_to),
        ("client_id", req.client_id),
        ("ptt", ptt_val),
    ];
    for (name, val) in text_parts {
        body.extend_from_slice(b"--");
        body.extend_from_slice(b_bytes);
        body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(b"\"\r\n\r\n");
        body.extend_from_slice(val.as_bytes());
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(b"--");
    body.extend_from_slice(b_bytes);
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"");
    body.extend_from_slice(wire_name.as_bytes());
    body.extend_from_slice(b"\"\r\nContent-Type: ");
    body.extend_from_slice(mime.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(req.bytes);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(b_bytes);
    body.extend_from_slice(b"--\r\n");

    if body.len() > MAX_FILE_BYTES {
        return Err(HubError::FileTooLarge);
    }

    let content_type = format!("multipart/form-data; boundary={boundary}");
    Ok((content_type, body))
}
