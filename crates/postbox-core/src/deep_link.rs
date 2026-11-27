use crate::{ChatKey, Provider};
use thiserror::Error;

pub const SCHEME_PREFIX: &str = "postbox://";
pub const LINK_PIPE_PREFIX: &str = r"\\.\pipe\postbox-link-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepLinkTarget {
    pub chat: ChatKey,
    pub message: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeepLinkError {
    #[error("invalid_scheme")]
    InvalidScheme,
    #[error("unsupported_provider")]
    UnsupportedProvider,
    #[error("missing_account")]
    MissingAccount,
    #[error("missing_chat")]
    MissingChat,
    #[error("too_many_segments")]
    TooManySegments,
    #[error("invalid_encoding")]
    InvalidEncoding,
    #[error("control_character")]
    ControlCharacter,
}

pub fn is_postbox_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() >= SCHEME_PREFIX.len()
        && trimmed[..SCHEME_PREFIX.len()].eq_ignore_ascii_case(SCHEME_PREFIX)
}

pub fn missing_chat_note(provider: Provider) -> &'static str {
    match provider {
        Provider::Whatsapp => "Could not find that WhatsApp chat.",
        Provider::Slack => "Could not find that Slack channel.",
        Provider::Telegram => "Could not find that Telegram chat.",
        Provider::Signal => "Could not find that Signal chat.",
    }
}

pub fn build(chat: &ChatKey, message: Option<&str>) -> String {
    let mut url = format!("{SCHEME_PREFIX}{}", chat.provider.as_str());
    for segment in [Some(chat.account.as_str()), Some(chat.id.as_str()), message]
        .into_iter()
        .flatten()
    {
        url.push('/');
        for byte in segment.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                url.push(char::from(byte));
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                url.push('%');
                url.push(char::from(HEX[(byte >> 4) as usize]));
                url.push(char::from(HEX[(byte & 15) as usize]));
            }
        }
    }
    url
}

pub fn parse(raw: &str) -> Result<DeepLinkTarget, DeepLinkError> {
    let trimmed = raw.trim();
    if !is_postbox_url(trimmed) {
        return Err(DeepLinkError::InvalidScheme);
    }
    let rest = &trimmed[SCHEME_PREFIX.len()..];
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let rest = rest.trim_end_matches('/');

    let mut segments = rest.split('/');
    let provider_seg = segments.next().unwrap_or("");
    let provider = Provider::parse(provider_seg).ok_or(DeepLinkError::UnsupportedProvider)?;

    let account = decode_segment(segments.next().ok_or(DeepLinkError::MissingAccount)?)?;
    if account.is_empty() {
        return Err(DeepLinkError::MissingAccount);
    }

    let chat_id = decode_segment(segments.next().ok_or(DeepLinkError::MissingChat)?)?;
    if chat_id.is_empty() {
        return Err(DeepLinkError::MissingChat);
    }

    let message = match segments.next() {
        Some(seg) if !seg.is_empty() => Some(normalize_message_id(provider, decode_segment(seg)?)),
        Some(_) | None => None,
    };

    if segments.next().is_some() {
        return Err(DeepLinkError::TooManySegments);
    }

    Ok(DeepLinkTarget {
        chat: ChatKey::new(provider, account, chat_id),
        message,
    })
}

fn normalize_message_id(provider: Provider, raw: String) -> String {
    if provider == Provider::Slack
        && raw.len() == 17
        && raw.starts_with('p')
        && raw[1..].bytes().all(|b| b.is_ascii_digit())
    {
        return format!("{}.{}", &raw[1..11], &raw[11..]);
    }
    raw
}

fn decode_segment(seg: &str) -> Result<String, DeepLinkError> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let decoded = if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(DeepLinkError::InvalidEncoding);
            }
            let hi = hex_val(bytes[i + 1]).ok_or(DeepLinkError::InvalidEncoding)?;
            let lo = hex_val(bytes[i + 2]).ok_or(DeepLinkError::InvalidEncoding)?;
            i += 3;
            (hi << 4) | lo
        } else {
            i += 1;
            b
        };
        if decoded < 0x20 || decoded == 0x7F {
            return Err(DeepLinkError::ControlCharacter);
        }
        out.push(decoded);
    }
    String::from_utf8(out).map_err(|_| DeepLinkError::InvalidEncoding)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_encoded_links_that_round_trip() {
        for provider in [
            Provider::Whatsapp,
            Provider::Slack,
            Provider::Telegram,
            Provider::Signal,
        ] {
            let chat = ChatKey::new(provider, "account /?#%", "chat/é@host");
            let url = build(&chat, Some("message/?#% é"));
            assert!(!url.contains(' '));
            assert_eq!(
                parse(&url).unwrap(),
                DeepLinkTarget {
                    chat: chat.clone(),
                    message: Some("message/?#% é".into()),
                }
            );
            assert_eq!(parse(&build(&chat, None)).unwrap().message, None);
        }
    }

    #[test]
    fn parses_whatsapp_and_slack_permalink_deep_links() {
        let wa = parse("postbox://whatsapp/biz/491234%40s.whatsapp.net/MSG-1").unwrap();
        assert_eq!(wa.chat.provider, Provider::Whatsapp);
        assert_eq!(wa.chat.account, "biz");
        assert_eq!(wa.chat.id, "491234@s.whatsapp.net");
        assert_eq!(wa.message.as_deref(), Some("MSG-1"));

        let slack = parse("postbox://slack/T01/C02/p1700000000123456").unwrap();
        assert_eq!(slack.chat.provider, Provider::Slack);
        assert_eq!(slack.message.as_deref(), Some("1700000000.123456"));
    }

    #[test]
    fn rejects_control_characters_and_missing_segments() {
        assert_eq!(
            parse("postbox://whatsapp/acct/chat%0Aevil"),
            Err(DeepLinkError::ControlCharacter)
        );
        assert_eq!(
            parse("postbox://whatsapp/acct"),
            Err(DeepLinkError::MissingChat)
        );
    }
}
