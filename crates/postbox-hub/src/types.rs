use crate::error::HubError;
use postbox_core::{Chat, ChatKey, MediaKind, Message, Provider, Reaction, ReadStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

        if authority.is_empty()
            || authority.contains(['/', '?', '#', '@', '\\'])
            || authority.bytes().any(|b| b <= 0x20 || b == 0x7F)
        {
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

    pub fn base_url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        let default_port = if self.secure { 443 } else { 80 };
        if self.port == default_port {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    pub fn url_for(&self, path: &str) -> String {
        format!("{}{path}", self.base_url())
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

#[derive(Debug, Clone, Copy)]
pub struct SendFileRequest<'a> {
    pub chat: &'a str,
    pub text: &'a str,
    pub reply_to: &'a str,
    pub client_id: &'a str,
    pub ptt: bool,
    pub filename: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Completed,
    ManagedLocally,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveBatchItemResult {
    pub chat: String,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaFetchState {
    Ready(Vec<u8>),
    Pending,
    Unavailable,
    /// HTTP 410 Gone — media is permanently unavailable and MUST be recorded in SQLite `gone_media`.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFetchDetailed {
    pub state: MediaFetchState,
    pub content_type: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubSyncSignal {
    Reset,
    Event(SseEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HubChat {
    pub jid: String,
    pub provider: Provider,
    pub view_account: String,
    pub source_account: String,
    pub team_name: String,
    pub name: String,
    pub last_message_ts: String,
    pub last_message_preview: String,
    pub unread: bool,
    pub unread_count: u32,
    pub archived: bool,
    pub is_group: bool,
    pub saved: bool,
    pub avatar_hash: Option<String>,
    pub raw: Value,
}

impl HubChat {
    pub fn chat_key(&self) -> ChatKey {
        ChatKey::new(self.provider, self.view_account.clone(), self.jid.clone())
    }

    pub fn to_core_chat(&self, archived_local: bool, pinned: bool) -> Chat {
        let unread_count = if self.unread_count > 0 {
            self.unread_count
        } else if self.unread {
            1
        } else {
            0
        };
        Chat {
            key: self.chat_key(),
            title: if self.saved {
                "Saved Messages".to_owned()
            } else if self.name.is_empty() {
                self.jid.clone()
            } else {
                self.name.clone()
            },
            is_group: self.is_group,
            archived_local,
            archived_remote: self.archived,
            pinned,
            unread_count,
            last_msg_ts: self.last_message_ts.clone(),
            last_msg_preview: self.last_message_preview.clone(),
            avatar_hash: self.avatar_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HubMessage {
    pub msg_id: String,
    pub chat_jid: String,
    pub sender_id: String,
    pub sender_name: String,
    pub from_me: bool,
    pub ts: String,
    pub display_text: String,
    pub text: String,
    pub effective_text: String,
    pub reply_to_id: Option<String>,
    pub thread_ts: Option<String>,
    pub reply_broadcast: bool,
    pub media_type: String,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    pub file_size: Option<u64>,
    pub reaction_to_id: String,
    pub reaction_emoji: String,
    pub reactions: Vec<Reaction>,
    pub revoked: bool,
    pub delivered_count: i64,
    pub read_by_count: i64,
    pub raw: Value,
}

impl HubMessage {
    pub fn to_core_message(&self, chat_key: &ChatKey, is_group: bool) -> Message {
        let media_kind = if self.media_type.is_empty() {
            None
        } else if self
            .mime_type
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("image/gif"))
        {
            Some(MediaKind::Gif)
        } else {
            MediaKind::parse(&self.media_type).or_else(|| {
                match self.media_type.to_ascii_lowercase().as_str() {
                    "ptt" => Some(MediaKind::Voice),
                    "file" | "doc" => Some(MediaKind::Document),
                    _ => None,
                }
            })
        };

        let cap = postbox_core::read::ReceiptCapability::for_chat(chat_key.provider, is_group);
        let tick = postbox_core::read::compute_tick(
            cap,
            self.from_me,
            true,
            self.revoked,
            self.delivered_count,
            self.read_by_count,
        );
        let read_status = if self.from_me && !self.revoked {
            let s = tick.to_read_status();
            if s == ReadStatus::None {
                ReadStatus::Sent
            } else {
                s
            }
        } else {
            ReadStatus::None
        };

        let reply_to_id = self
            .thread_ts
            .as_deref()
            .filter(|t| !t.is_empty() && *t != self.msg_id)
            .map(str::to_owned)
            .or_else(|| {
                self.reply_to_id
                    .as_deref()
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
            });

        Message {
            chat: chat_key.clone(),
            msg_id: self.msg_id.clone(),
            sender_id: self.sender_id.clone(),
            sender_name: self.sender_name.clone(),
            from_me: self.from_me,
            ts: self.ts.clone(),
            body: self.effective_text.clone(),
            reply_to_id,
            reply_broadcast: self.reply_broadcast,
            media_kind,
            mime_type: self.mime_type.clone(),
            filename: self.filename.clone(),
            file_size: self.file_size,
            reactions: self.reactions.clone(),
            read_status,
            deleted: self.revoked,
        }
    }
}
