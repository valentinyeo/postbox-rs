//! Windows Toast XML generation and the notification suppression policy.
//!
//! The XML builder is pure so the escaping rules, the byte clips and the
//! `postbox://` launch payload are unit-tested on every platform. The Windows
//! `Notifier` only hands the finished markup to WinRT.

use crate::ChatKey;
use crate::deep_link::{self, SCHEME_PREFIX};
use std::collections::HashMap;

/// Longest title the toast layout keeps (`IGSH-1737`).
pub const MAX_TITLE_BYTES: usize = 120;
/// Longest body the toast layout keeps.
pub const MAX_BODY_BYTES: usize = 400;
/// Hard ceiling for the finished markup; WinRT rejects oversized documents.
pub const MAX_XML_BYTES: usize = 4096;

/// Why a message must not raise a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// The message came from this account.
    FromMe,
    /// The window is focused and already showing that chat.
    FocusedChat,
    /// This message (or an older one) already raised a toast.
    AlreadySeen,
}

/// What the shell knows about its own window when a message arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToastFocus<'a> {
    /// `true` when the Postbox window owns the foreground.
    pub window_focused: bool,
    /// The chat the window is showing, when it shows one.
    pub chat: Option<&'a ChatKey>,
}

impl ToastFocus<'_> {
    pub fn blurred() -> Self {
        Self::default()
    }

    pub fn focused_on(chat: &ChatKey) -> ToastFocus<'_> {
        ToastFocus {
            window_focused: true,
            chat: Some(chat),
        }
    }

    pub fn focused_elsewhere() -> Self {
        ToastFocus {
            window_focused: true,
            chat: None,
        }
    }
}

/// Decides whether an incoming message may raise a toast, and remembers the high-water mark
/// per chat so a replayed history page or a re-delivered SSE event stays silent.
#[derive(Debug, Default)]
pub struct NotificationGate {
    seen: HashMap<ChatKey, i64>,
}

impl NotificationGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records everything already on screen at startup so the first painted inbox is silent.
    pub fn prime(&mut self, chat: &ChatKey, last_ts: i64) {
        let entry = self.seen.entry(chat.clone()).or_insert(last_ts);
        if *entry < last_ts {
            *entry = last_ts;
        }
    }

    /// The newest timestamp this chat has been notified about.
    pub fn high_water(&self, chat: &ChatKey) -> Option<i64> {
        self.seen.get(chat).copied()
    }

    /// `None` means "raise a toast". The gate advances only when it returns `None`.
    pub fn check(
        &mut self,
        chat: &ChatKey,
        ts: i64,
        from_me: bool,
        focus: ToastFocus<'_>,
    ) -> Option<Suppression> {
        if from_me {
            return Some(Suppression::FromMe);
        }
        if focus.window_focused && focus.chat == Some(chat) {
            return Some(Suppression::FocusedChat);
        }
        if let Some(high) = self.seen.get(chat)
            && ts <= *high
        {
            return Some(Suppression::AlreadySeen);
        }
        self.seen.insert(chat.clone(), ts);
        None
    }
}

/// `postbox://<provider>/<account>/<chat_id>[/<message_id>]` for the toast launch action.
///
/// Every segment is percent-encoded, so a WhatsApp JID's `@` and a Slack channel's `/`
/// survive the round trip through the registry handler.
pub fn launch_url(chat: &ChatKey, message: Option<&str>) -> String {
    let mut url = String::with_capacity(SCHEME_PREFIX.len() + 64);
    url.push_str(SCHEME_PREFIX);
    url.push_str(chat.provider.as_str());
    url.push('/');
    encode_segment_into(&mut url, &chat.account);
    url.push('/');
    encode_segment_into(&mut url, &chat.id);
    if let Some(msg) = message.filter(|m| !m.is_empty()) {
        url.push('/');
        encode_segment_into(&mut url, msg);
    }
    url
}

fn encode_segment_into(out: &mut String, segment: &str) {
    for byte in segment.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0F));
        }
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + (nibble - 10)
    })
}

/// Escapes `&`, `<`, `>`, `"` and `'`, turns tabs/newlines into spaces and drops every other
/// control character, so the markup WinRT receives is always well formed.
pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(' '),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {}
            c => out.push(c),
        }
    }
    out
}

/// Cuts `text` down to at most `max` bytes without splitting a character.
pub fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The finished toast document, ready for `XmlDocument::LoadXml`.
///
/// `activationType="protocol"` with a `postbox://` launch target means a click runs the
/// registered protocol handler, which focuses the running instance and opens the chat.
pub fn build_xml(chat: &ChatKey, title: &str, body: &str) -> String {
    let launch = escape_xml(&launch_url(chat, None));
    let mut xml = String::with_capacity(MAX_XML_BYTES);
    xml.push_str("<toast activationType=\"protocol\" launch=\"");
    xml.push_str(&launch);
    xml.push_str("\"><visual><binding template=\"ToastGeneric\"><text>");
    xml.push_str(&escape_xml(clip(title, MAX_TITLE_BYTES)));
    xml.push_str("</text><text>");
    xml.push_str(&escape_xml(clip(body, MAX_BODY_BYTES)));
    xml.push_str("</text></binding></visual><audio silent=\"true\"/></toast>");
    clip(&xml, MAX_XML_BYTES).to_owned()
}

/// Message-triggered toast body: the sender plus a one-line preview.
pub fn preview_body(sender: &str, text: &str) -> String {
    let sender = sender.trim();
    let text = text.trim();
    if sender.is_empty() {
        text.to_owned()
    } else if text.is_empty() {
        sender.to_owned()
    } else {
        format!("{sender}: {text}")
    }
}

/// Chat a launch target resolves to, for tests that walk the whole click path.
pub fn launch_target(url: &str) -> Option<ChatKey> {
    deep_link::parse(url).ok().map(|target| target.chat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    fn wa() -> ChatKey {
        ChatKey::new(
            Provider::Whatsapp,
            "biz",
            "491234@s.whatsapp.net".to_owned(),
        )
    }

    #[test]
    fn markup_escapes_every_character_windows_rejects() {
        let xml = build_xml(&wa(), "Tom & Jerry <3", "say \"hi\" it's > ok");
        assert!(
            xml.contains("<text>Tom &amp; Jerry &lt;3</text>"),
            "title must be escaped: {xml}"
        );
        assert!(
            xml.contains("<text>say &quot;hi&quot; it&apos;s &gt; ok</text>"),
            "body must be escaped: {xml}"
        );
        assert!(!xml.contains("&<"));
    }

    #[test]
    fn control_characters_never_reach_the_markup() {
        let xml = build_xml(&wa(), "a\u{0}b\u{7}c", "line one\nline two\ttab\u{7f}");
        assert!(xml.contains("<text>abc</text>"), "{xml}");
        assert!(xml.contains("<text>line one line two tab</text>"), "{xml}");
        assert!(!xml.chars().any(|c| (c as u32) < 0x20));
    }

    #[test]
    fn long_text_clips_on_a_character_boundary() {
        let long = "x".repeat(MAX_BODY_BYTES - 1) + "\u{e9}";
        let clipped = clip(&long, MAX_BODY_BYTES);
        assert_eq!(clipped.len(), MAX_BODY_BYTES - 1);
        assert!(clipped.chars().all(|c| c == 'x'));
        let xml = build_xml(&wa(), "t", &long);
        assert!(xml.len() <= MAX_XML_BYTES);
    }

    #[test]
    fn launch_action_parses_back_into_the_same_chat() {
        let xml = build_xml(&wa(), "Ada", "Ping");
        assert!(
            xml.starts_with("<toast activationType=\"protocol\" launch=\"postbox://whatsapp/biz/491234%40s.whatsapp.net\""),
            "{xml}"
        );
        let url = launch_url(&wa(), Some("MSG-1"));
        assert_eq!(url, "postbox://whatsapp/biz/491234%40s.whatsapp.net/MSG-1");
        assert_eq!(launch_target(&url), Some(wa()));

        // Slack channel ids contain `/` on the permalink path; the encoder must keep them whole.
        let slack = ChatKey::new(Provider::Slack, "T01", "C02/x");
        assert_eq!(launch_target(&launch_url(&slack, None)), Some(slack));
    }

    #[test]
    fn launch_action_escapes_an_ampersand_in_an_account_name() {
        let odd = ChatKey::new(Provider::Telegram, "a&b", "chat");
        let xml = build_xml(&odd, "t", "b");
        assert!(
            xml.contains("launch=\"postbox://telegram/a%26b/chat\""),
            "{xml}"
        );
    }

    #[test]
    fn own_messages_and_the_focused_chat_stay_silent() {
        let chat = wa();
        let mut gate = NotificationGate::new();
        assert_eq!(
            gate.check(&chat, 1_000, true, ToastFocus::blurred()),
            Some(Suppression::FromMe)
        );
        assert_eq!(
            gate.check(&chat, 1_000, false, ToastFocus::focused_on(&chat)),
            Some(Suppression::FocusedChat)
        );
        assert_eq!(gate.check(&chat, 1_000, false, ToastFocus::blurred()), None);
        assert_eq!(gate.high_water(&chat), Some(1_000));
    }

    #[test]
    fn a_focused_window_on_another_chat_still_toasts() {
        let chat = wa();
        let other = ChatKey::new(Provider::Slack, "T01", "C02");
        let mut gate = NotificationGate::new();
        assert_eq!(
            gate.check(&chat, 5, false, ToastFocus::focused_on(&other)),
            None
        );
        assert_eq!(
            gate.check(&chat, 5, false, ToastFocus::focused_elsewhere()),
            Some(Suppression::AlreadySeen)
        );
    }

    #[test]
    fn replay_and_out_of_order_events_do_not_raise_a_second_toast() {
        let chat = wa();
        let mut gate = NotificationGate::new();
        gate.prime(&chat, 1_500);
        assert_eq!(
            gate.check(&chat, 1_200, false, ToastFocus::blurred()),
            Some(Suppression::AlreadySeen)
        );
        assert_eq!(gate.check(&chat, 1_501, false, ToastFocus::blurred()), None);
        // The mark never moves backwards.
        gate.prime(&chat, 10);
        assert_eq!(gate.high_water(&chat), Some(1_501));
    }

    #[test]
    fn preview_body_keeps_the_sender_visible() {
        assert_eq!(preview_body("Ada", " Ping "), "Ada: Ping");
        assert_eq!(preview_body("Ada", "   "), "Ada");
        assert_eq!(preview_body("  ", "Hello"), "Hello");
    }
}
