pub mod archive;
pub mod control;
pub mod deep_link;
pub mod dictation;
pub mod drafts;
pub mod outbox;
pub mod read;
pub mod threads;
pub mod views;

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
pub struct Account {
    pub provider: Provider,
    pub account_id: String,
    pub label: String,
    pub state: String,
    pub sort_order: i32,
    pub last_event_at: Option<String>,
}

impl Account {
    pub fn view_key(&self) -> String {
        format!("{}:{}", self.provider.as_str(), self.account_id)
    }

    pub fn to_view_tab(&self) -> ViewTab {
        ViewTab::Account {
            provider: self.provider,
            account: self.account_id.clone(),
            label: self.label.clone(),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub chat: ChatKey,
    pub text_body: String,
    pub reply_to_id: Option<String>,
    pub staged_image_path: Option<String>,
    pub updated_at: i64,
}

impl Draft {
    /// Sidebar preview matching Zig `drafts.zig`: `"DRAFT: "` + up to 160 valid UTF-8 bytes
    /// with `\r`, `\n`, `\t` collapsed to single spaces.
    pub fn sidebar_preview(&self) -> String {
        let mut out = String::from("DRAFT: ");
        let mut body_bytes = 0usize;
        for ch in self.text_body.chars() {
            let mapped = match ch {
                '\r' | '\n' | '\t' => ' ',
                other => other,
            };
            let len = mapped.len_utf8();
            if body_bytes + len > 160 {
                break;
            }
            out.push(mapped);
            body_bytes += len;
        }
        out
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

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Gif => "gif",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::Sticker => "sticker",
            Self::Document => "document",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "image" => Some(Self::Image),
            "gif" => Some(Self::Gif),
            "video" => Some(Self::Video),
            "voice" => Some(Self::Voice),
            "sticker" => Some(Self::Sticker),
            "document" => Some(Self::Document),
            _ => None,
        }
    }
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

impl ReadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "delivered" => Self::Delivered,
            "read" => Self::Read,
            "failed" => Self::Failed,
            _ => Self::None,
        }
    }
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

impl ThemeChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZincDark => "zincdark",
            Self::Amoled => "amoled",
            Self::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ZincDark => "Zinc Dark",
            Self::Amoled => "AMOLED",
            Self::Light => "Light",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "zincdark" | "zinc_dark" | "zinc-dark" => Some(Self::ZincDark),
            "amoled" => Some(Self::Amoled),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontScale(u16);

impl FontScale {
    pub const MIN: u16 = 60;
    pub const DEFAULT: u16 = 100;
    pub const MAX: u16 = 160;
    pub const STEP: u16 = 10;
    pub const LEGACY_DEFAULT: u16 = 82;
    pub const SMALL: u16 = 80;
    pub const LARGE: u16 = 125;
    pub const LARGER: u16 = 150;

    pub fn clamped(percent: u16) -> Self {
        Self(percent.clamp(Self::MIN, Self::MAX))
    }

    /// Migrates the legacy Zig default (`82`) to `100` (`DEFAULT`); clamps all other values to `60..=160`.
    pub fn from_persisted(raw: u16) -> Self {
        if raw == Self::LEGACY_DEFAULT {
            Self(Self::DEFAULT)
        } else {
            Self::clamped(raw)
        }
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

    pub fn reset(self) -> Self {
        Self(Self::DEFAULT)
    }

    pub const fn preset_small() -> Self {
        Self(Self::SMALL)
    }

    pub const fn preset_large() -> Self {
        Self(Self::LARGE)
    }

    pub const fn preset_larger() -> Self {
        Self(Self::LARGER)
    }
}

impl Default for FontScale {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiSettings {
    pub theme: ThemeChoice,
    pub font_scale: FontScale,
    pub view_order: Vec<String>,
    pub show_archived: bool,
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
    pub fn label(&self) -> &str {
        match self {
            Self::CombinedInbox => "Combined inbox",
            Self::Account { label, .. } => label.as_str(),
        }
    }

    pub fn view_key(&self) -> String {
        match self {
            Self::CombinedInbox => "all".to_owned(),
            Self::Account {
                provider, account, ..
            } => format!("{}:{account}", provider.as_str()),
        }
    }

    pub fn window_title(&self, version: &str) -> String {
        Self::window_title_with_status(Some(self), version, 0, None)
    }

    pub fn window_title_with_runtime(
        &self,
        version: &str,
        marked_count: usize,
        notice: Option<&str>,
    ) -> String {
        Self::window_title_with_status(Some(self), version, marked_count, notice)
    }

    pub fn window_title_with_status(
        view: Option<&ViewTab>,
        version: &str,
        marked_count: usize,
        notice: Option<&str>,
    ) -> String {
        let base_label = match view {
            Some(Self::CombinedInbox) => Some("Combined inbox"),
            Some(Self::Account { label, .. }) => {
                let trimmed = label.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            }
            None => None,
        };
        let Some(base) = base_label else {
            return format!("Postbox v{version}");
        };
        let mut prefix = base.to_owned();
        if marked_count > 0 {
            prefix.push_str(&format!(" \u{00B7} {marked_count} messages marked"));
        }
        if let Some(n) = notice.map(str::trim).filter(|s| !s.is_empty()) {
            prefix.push_str(" \u{00B7} ");
            prefix.push_str(n);
        }
        format!("{prefix} - Postbox v{version}")
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
    fn font_scale_migrates_legacy_82_and_supports_presets() {
        assert_eq!(FontScale::from_persisted(82), FontScale::default());
        assert_eq!(FontScale::from_persisted(0).percent(), 60);
        assert_eq!(FontScale::from_persisted(999).percent(), 160);
        assert_eq!(FontScale::from_persisted(125).percent(), 125);
        assert_eq!(FontScale::preset_small().percent(), 80);
        assert_eq!(FontScale::default().percent(), 100);
        assert_eq!(FontScale::preset_large().percent(), 125);
        assert_eq!(FontScale::preset_larger().percent(), 150);
        assert_eq!(FontScale::preset_larger().reset().percent(), 100);
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

    #[test]
    fn window_title_with_status_formats_before_view_marked_and_notice() {
        assert_eq!(
            ViewTab::window_title_with_status(None, "0.1.0", 0, None),
            "Postbox v0.1.0"
        );
        let empty_label = ViewTab::Account {
            provider: Provider::Telegram,
            account: "main".into(),
            label: String::new(),
        };
        assert_eq!(
            ViewTab::window_title_with_status(Some(&empty_label), "0.1.0", 2, Some("Notice")),
            "Postbox v0.1.0"
        );
        assert_eq!(
            ViewTab::CombinedInbox.window_title_with_runtime("0.1.0", 1, None),
            "Combined inbox \u{00B7} 1 messages marked - Postbox v0.1.0"
        );
        assert_eq!(
            ViewTab::CombinedInbox.window_title_with_runtime("0.1.0", 3, Some("Copied transcript")),
            "Combined inbox \u{00B7} 3 messages marked \u{00B7} Copied transcript - Postbox v0.1.0"
        );
        assert_eq!(
            ViewTab::CombinedInbox.window_title_with_runtime("0.1.0", 0, Some("   ")),
            "Combined inbox - Postbox v0.1.0"
        );
    }

    #[test]
    fn window_title_trims_whitespace_only_and_padded_account_labels() {
        let whitespace_label = ViewTab::Account {
            provider: Provider::Telegram,
            account: "main".into(),
            label: "   \t\n  ".into(),
        };
        assert_eq!(
            ViewTab::window_title_with_status(Some(&whitespace_label), "0.1.0", 2, Some("Notice")),
            "Postbox v0.1.0"
        );

        let padded_label = ViewTab::Account {
            provider: Provider::Slack,
            account: "T123".into(),
            label: "  Acme Slack  ".into(),
        };
        assert_eq!(
            padded_label.window_title("0.1.0"),
            "Acme Slack - Postbox v0.1.0"
        );
    }

    #[test]
    fn enum_parse_and_as_str_round_trip() {
        for kind in [
            MediaKind::Image,
            MediaKind::Gif,
            MediaKind::Video,
            MediaKind::Voice,
            MediaKind::Sticker,
            MediaKind::Document,
        ] {
            assert_eq!(MediaKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                MediaKind::parse(&kind.as_str().to_ascii_uppercase()),
                Some(kind)
            );
        }
        assert_eq!(MediaKind::parse("unknown"), None);

        for status in [
            ReadStatus::None,
            ReadStatus::Sending,
            ReadStatus::Sent,
            ReadStatus::Delivered,
            ReadStatus::Read,
            ReadStatus::Failed,
        ] {
            assert_eq!(ReadStatus::parse(status.as_str()), status);
        }
        assert_eq!(ReadStatus::parse("garbage"), ReadStatus::None);

        for theme in [
            ThemeChoice::ZincDark,
            ThemeChoice::Amoled,
            ThemeChoice::Light,
        ] {
            assert_eq!(ThemeChoice::parse(theme.as_str()), Some(theme));
            assert!(!theme.label().is_empty());
        }
        assert_eq!(ThemeChoice::parse("zinc_dark"), Some(ThemeChoice::ZincDark));
        assert_eq!(ThemeChoice::parse("zinc-dark"), Some(ThemeChoice::ZincDark));
        assert_eq!(ThemeChoice::parse("neon"), None);
    }

    #[test]
    fn draft_sidebar_preview_flattens_whitespace_and_respects_utf8_cap() {
        let draft = Draft {
            chat: ChatKey::new(Provider::Slack, "T1", "C1"),
            text_body: "Hello\r\nworld\tfrom\nPostbox".into(),
            reply_to_id: None,
            staged_image_path: None,
            updated_at: 100,
        };
        assert_eq!(draft.sidebar_preview(), "DRAFT: Hello  world from Postbox");

        // Multi-byte UTF-8 characters (4-byte emoji): 45 * 4 = 180 bytes -> capped at 40 emoji (160 bytes)
        let long_draft = Draft {
            chat: ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net"),
            text_body: "\u{1F680}".repeat(45),
            reply_to_id: None,
            staged_image_path: None,
            updated_at: 200,
        };
        let preview = long_draft.sidebar_preview();
        assert!(preview.starts_with("DRAFT: "));
        assert_eq!(preview.len() - "DRAFT: ".len(), 160);
    }

    #[test]
    fn account_helpers_build_view_key_and_tab() {
        let account = Account {
            provider: Provider::Slack,
            account_id: "T01".into(),
            label: "Acme Slack".into(),
            state: "connected".into(),
            sort_order: 1,
            last_event_at: None,
        };
        assert_eq!(account.view_key(), "slack:T01");
        let tab = account.to_view_tab();
        assert_eq!(tab.label(), "Acme Slack");
        assert_eq!(tab.view_key(), "slack:T01");
        assert_eq!(ViewTab::CombinedInbox.view_key(), "all");
    }
}
