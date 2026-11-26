pub mod archive;
pub mod control;
pub mod deep_link;
pub mod dictation;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Whatsapp,
    Slack,
    Telegram,
    Signal,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Whatsapp => "whatsapp",
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::Signal => "signal",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "whatsapp" => Some(Self::Whatsapp),
            "slack" => Some(Self::Slack),
            "telegram" => Some(Self::Telegram),
            "signal" => Some(Self::Signal),
            _ => None,
        }
    }
}

/// Provider-qualified chat identity. Never compare bare JIDs across accounts or providers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChatKey {
    pub provider: Provider,
    pub account: String,
    pub id: String,
}

impl ChatKey {
    pub fn new(provider: Provider, account: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider,
            account: account.into(),
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub key: ChatKey,
    pub title: String,
    pub is_group: bool,
    pub archived_local: bool,
    pub archived_remote: bool,
    pub pinned: bool,
    pub unread_count: u32,
    pub last_msg_ts: String,
    pub last_msg_preview: String,
    pub avatar_hash: Option<String>,
}

impl Chat {
    pub fn is_archived(&self) -> bool {
        self.archived_local || self.archived_remote
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Gif,
    Video,
    Voice,
    Sticker,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadStatus {
    None,
    Sending,
    Sent,
    Delivered,
    Read,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    pub from_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub chat: ChatKey,
    pub msg_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub from_me: bool,
    pub ts: String,
    pub body: String,
    pub reply_to_id: Option<String>,
    pub reply_broadcast: bool,
    pub media_kind: Option<MediaKind>,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    pub reactions: Vec<Reaction>,
    pub read_status: ReadStatus,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    ZincDark,
    Amoled,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontScale(u16);

impl FontScale {
    pub const MIN: u16 = 60;
    pub const DEFAULT: u16 = 100;
    pub const MAX: u16 = 160;
    pub const STEP: u16 = 10;

    pub fn clamped(percent: u16) -> Self {
        Self(percent.clamp(Self::MIN, Self::MAX))
    }

    pub fn percent(self) -> u16 {
        self.0
    }

    pub fn larger(self) -> Self {
        Self::clamped(self.0.saturating_add(Self::STEP))
    }

    pub fn smaller(self) -> Self {
        Self::clamped(self.0.saturating_sub(Self::STEP))
    }
}

impl Default for FontScale {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewTab {
    CombinedInbox,
    Account {
        provider: Provider,
        account: String,
        label: String,
    },
}

impl ViewTab {
    pub fn window_title(&self, version: &str) -> String {
        match self {
            Self::CombinedInbox => format!("Combined inbox - Postbox v{version}"),
            Self::Account { label, .. } if !label.is_empty() => {
                format!("{label} - Postbox v{version}")
            }
            Self::Account { .. } => format!("Postbox v{version}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_key_distinguishes_same_jid_across_accounts() {
        let personal = ChatKey::new(Provider::Whatsapp, "personal", "12345@s.whatsapp.net");
        let business = ChatKey::new(Provider::Whatsapp, "business", "12345@s.whatsapp.net");
        assert_ne!(personal, business);
    }

    #[test]
    fn font_scale_clamps_to_supported_bounds() {
        assert_eq!(FontScale::clamped(20).percent(), 60);
        assert_eq!(FontScale::clamped(250).percent(), 160);
        assert_eq!(FontScale::default().larger().percent(), 110);
        assert_eq!(FontScale::default().smaller().percent(), 90);
    }

    #[test]
    fn window_title_matches_product_invariant() {
        assert_eq!(
            ViewTab::CombinedInbox.window_title("0.1.0"),
            "Combined inbox - Postbox v0.1.0"
        );
        let slack = ViewTab::Account {
            provider: Provider::Slack,
            account: "T123".into(),
            label: "Acme Slack".into(),
        };
        assert_eq!(slack.window_title("0.1.0"), "Acme Slack - Postbox v0.1.0");
    }
}
