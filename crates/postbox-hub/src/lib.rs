use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
pub const OLDER_PAGE_LIMIT: usize = 80;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HubError {
    #[error("invalid_url")]
    InvalidUrl,
    #[error("insecure_url")]
    InsecureUrl,
    #[error("invalid_id")]
    InvalidId,
    #[error("invalid_send")]
    InvalidSend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub secure: bool,
}

impl Endpoint {
    pub fn parse(url: &str, allow_loopback: bool) -> Result<Self, HubError> {
        let trimmed = url.trim().trim_end_matches('/');
        let (secure, authority) = if let Some(rest) = trimmed.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(HubError::InvalidUrl);
        };

        if authority.is_empty() || authority.contains(['/', '?', '#', '@', '\r', '\n', '\\']) {
            return Err(HubError::InvalidUrl);
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| HubError::InvalidUrl)?),
            None => (authority, if secure { 443 } else { 80 }),
        };

        if host.is_empty() || port == 0 {
            return Err(HubError::InvalidUrl);
        }
        if !secure && !(allow_loopback && host == "127.0.0.1") {
            return Err(HubError::InsecureUrl);
        }

        Ok(Self {
            host: host.to_owned(),
            port,
            secure,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubRequest {
    pub method: &'static str,
    pub path: String,
    pub body: Option<String>,
    pub last_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReply {
    pub client_id: String,
    #[serde(default)]
    pub msg_id: String,
    #[serde(default)]
    pub ts: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SseEvent {
    pub id: u64,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub chat: String,
    #[serde(default)]
    pub ts: String,
    pub kind: String,
}

pub fn valid_id(value: &str) -> Result<(), HubError> {
    if value.is_empty() || value.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(HubError::InvalidId);
    }
    Ok(())
}

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

pub fn before_cursor(msg_id: &str, timestamp: &str) -> Result<String, HubError> {
    valid_id(msg_id)?;
    valid_id(timestamp)?;
    Ok(format!("{msg_id}:{timestamp}"))
}

pub fn send_text_request(
    chat: &str,
    text: &str,
    reply_to: Option<&str>,
    reply_broadcast: bool,
    client_id: &str,
) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    valid_id(client_id)?;
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

pub fn react_request(
    chat: &str,
    msg_id: &str,
    emoji: &str,
    client_id: &str,
) -> Result<HubRequest, HubError> {
    valid_id(chat)?;
    valid_id(msg_id)?;
    valid_id(client_id)?;
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

pub fn events_request(last_event_id: Option<&str>) -> HubRequest {
    HubRequest {
        method: "GET",
        path: "/v1/wazig/events".to_owned(),
        body: None,
        last_event_id: last_event_id.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_https_except_explicit_loopback() {
        assert_eq!(
            Endpoint::parse("http://example.com:8080", true),
            Err(HubError::InsecureUrl)
        );
        let loopback = Endpoint::parse("http://127.0.0.1:9000", true).unwrap();
        assert_eq!(loopback.port, 9000);
        assert!(!loopback.secure);

        let tls = Endpoint::parse("https://hub.example.invalid/", false).unwrap();
        assert!(tls.secure);
        assert_eq!(tls.port, 443);
    }

    #[test]
    fn builds_paginated_messages_and_broadcast_send_requests() {
        let cursor = before_cursor("M100", "2026-11-26T10:00:00Z").unwrap();
        let req = messages_request("whatsapp:biz:49123@s.whatsapp.net", 80, Some(&cursor)).unwrap();
        assert!(
            req.path
                .starts_with("/v1/wazig/messages?chat=whatsapp%3Abiz%3A")
        );
        assert!(req.path.contains("limit=80"));

        assert_eq!(
            send_text_request("slack:T1:C1", "hi", None, true, "cid-1"),
            Err(HubError::InvalidSend)
        );
        let broadcast =
            send_text_request("slack:T1:C1", "hi", Some("1700.1"), true, "cid-2").unwrap();
        assert!(broadcast.body.unwrap().contains("\"reply_broadcast\":true"));
    }
}
