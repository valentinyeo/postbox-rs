use crate::drafts::MAX_DRAFT_TEXT_BYTES;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DICTATION_TRANSCRIPT_CAPACITY: usize = 8192;
pub const DICTATION_MAX_DURATION_SECS: u64 = 600;
pub const DICTATION_CLOSE_STREAM_TIMEOUT_MS: u64 = 10_000;
pub const DICTATION_SAMPLE_RATE: u32 = 16_000;
pub const DICTATION_CHANNELS: u16 = 1;
pub const DICTATION_BYTES_PER_SEC: usize = 32_000;

pub const MIC_CHOICE_MAX_DEVICES: usize = 16;
pub const MIC_CHOICE_NAME_CAP: usize = 128;

pub const MIC_PILL_BAR_COUNT: usize = 8;
pub const MIC_PILL_TICK_MS: u64 = 120;
pub const MIC_PILL_WIDTH_PX: u32 = 116;
pub const MIC_PILL_HEIGHT_PX: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DictationLanguage {
    #[default]
    Automatic,
    English,
    German,
}

impl DictationLanguage {
    /// Registry `REG_DWORD` mapping matching Zig `dictation_policy.zig`:
    /// `0 = Automatic`, `1 = English`, `2 = German`.
    pub const fn to_registry_dword(self) -> u32 {
        match self {
            Self::Automatic => 0,
            Self::English => 1,
            Self::German => 2,
        }
    }

    /// Parses a registry `REG_DWORD` value; any value `> 2` defaults to `Automatic`.
    pub const fn from_registry_dword(raw: u32) -> Self {
        match raw {
            1 => Self::English,
            2 => Self::German,
            _ => Self::Automatic,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::English => "english",
            Self::German => "german",
        }
    }

    /// Accepts canonical names (`"automatic"`, `"english"`, `"german"`) as well as
    /// DWORD string representations (`"0"`, `"1"`, `"2"`).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "automatic" | "0" => Some(Self::Automatic),
            "english" | "1" => Some(Self::English),
            "german" | "2" => Some(Self::German),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::English => "English",
            Self::German => "German",
        }
    }

    pub const fn status_text(self) -> &'static str {
        match self {
            Self::Automatic => "Dictation language: Automatic",
            Self::English => "Dictation language: English",
            Self::German => "Dictation language: German",
        }
    }

    pub fn deepgram_listen_path(self) -> &'static str {
        match self {
            Self::Automatic => {
                "/v1/listen?model=nova-3&smart_format=true&detect_language=en&detect_language=de&tag=postbox"
            }
            Self::English => "/v1/listen?model=nova-3&smart_format=true&language=en&tag=postbox",
            Self::German => "/v1/listen?model=nova-3&smart_format=true&language=de&tag=postbox",
        }
    }

    /// Deepgram Nova-3 live streaming WebSocket listen path (16 kHz mono linear16 PCM).
    pub const fn deepgram_live_listen_path(self) -> &'static str {
        match self {
            Self::Automatic => {
                "/v1/listen?model=nova-3&smart_format=true&interim_results=true&encoding=linear16&sample_rate=16000&channels=1&language=multi&language_hint=en&language_hint=de&tag=postbox"
            }
            Self::English => {
                "/v1/listen?model=nova-3&smart_format=true&interim_results=true&encoding=linear16&sample_rate=16000&channels=1&language=en&tag=postbox"
            }
            Self::German => {
                "/v1/listen?model=nova-3&smart_format=true&interim_results=true&encoding=linear16&sample_rate=16000&channels=1&language=de&tag=postbox"
            }
        }
    }

    /// Single fallback endpoint used ONLY on `NoSpeech` for `audio/ogg` in `Automatic` mode.
    pub const fn deepgram_multilingual_path() -> &'static str {
        "/v1/listen?model=nova-3&smart_format=true&language=multi&tag=postbox"
    }

    /// Automatic mode fails closed: only English or German tags are accepted.
    pub fn accepts_detected_tag(self, tag: Option<&str>) -> bool {
        match self {
            Self::English | Self::German => true,
            Self::Automatic => matches!(
                tag.map(|s| s.to_ascii_lowercase()).as_deref(),
                Some(t) if t == "en" || t == "de" || t.starts_with("en-") || t.starts_with("de-")
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DictationState {
    #[default]
    Idle,
    Recording,
    Transcribing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DictationAnchor {
    pub start: usize,
    pub end: usize,
}

impl DictationAnchor {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DictationPadding {
    pub before: bool,
    pub after: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDictation {
    pub whole: String,
    pub insertion: String,
    pub new_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrepareDictationError {
    #[error("invalid_selection")]
    InvalidSelection,
    #[error("invalid_text")]
    InvalidText,
    #[error("invalid_transcript")]
    InvalidTranscript,
    #[error("empty_transcript")]
    EmptyTranscript,
    #[error("draft_too_long")]
    DraftTooLong,
}

const fn is_word_break(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

fn clamp_char_boundary_floor(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn clamp_char_boundary_ceil(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Computes word-boundary space padding around UTF-8 byte selection `[start, end]`,
/// matching Zig `edit_contract.zig:14-22` safely on UTF-8 char boundaries.
pub fn dictation_padding(text: &str, start: usize, end: usize) -> DictationPadding {
    let first = clamp_char_boundary_floor(text, start);
    let last = clamp_char_boundary_ceil(text, end.max(first));
    let before = text[..first]
        .chars()
        .next_back()
        .is_some_and(|c| !is_word_break(c));
    let after = text[last..]
        .chars()
        .next()
        .is_some_and(|c| !is_word_break(c));
    DictationPadding { before, after }
}

fn normalize_transcript_newlines(transcript: &str) -> String {
    let mut out = String::with_capacity(transcript.len());
    let mut chars = transcript.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                let _ = chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Prepares an anchored dictation replacement for `original[start..end]`, matching
/// Zig `main.zig:11736-11797` in UTF-8:
/// - rejects out-of-bounds or non-char-boundary selections (`InvalidSelection`)
/// - rejects NUL bytes in `original` (`InvalidText`) or `transcript` (`InvalidTranscript`)
/// - normalizes `\r\n` and `\r` to `\n`
/// - rejects empty / Unicode-whitespace-only transcripts (`EmptyTranscript`)
/// - enforces `MAX_DRAFT_TEXT_BYTES` (64 KiB) (`DraftTooLong`).
pub fn prepare_dictation(
    original: &str,
    start: usize,
    end: usize,
    transcript: &str,
) -> Result<PreparedDictation, PrepareDictationError> {
    if start > end
        || end > original.len()
        || !original.is_char_boundary(start)
        || !original.is_char_boundary(end)
    {
        return Err(PrepareDictationError::InvalidSelection);
    }
    if original.len() > MAX_DRAFT_TEXT_BYTES || transcript.len() > MAX_DRAFT_TEXT_BYTES {
        return Err(PrepareDictationError::DraftTooLong);
    }
    if original.as_bytes().contains(&0) {
        return Err(PrepareDictationError::InvalidText);
    }
    if transcript.as_bytes().contains(&0) {
        return Err(PrepareDictationError::InvalidTranscript);
    }

    let normalized = normalize_transcript_newlines(transcript);
    if normalized.chars().all(char::is_whitespace) {
        return Err(PrepareDictationError::EmptyTranscript);
    }

    let pad = dictation_padding(original, start, end);
    let insertion_len =
        usize::from(pad.before) + normalized.len() + usize::from(pad.after);
    let whole_len = original.len() - (end - start) + insertion_len;
    if whole_len > MAX_DRAFT_TEXT_BYTES {
        return Err(PrepareDictationError::DraftTooLong);
    }

    let mut insertion = String::with_capacity(insertion_len);
    if pad.before {
        insertion.push(' ');
    }
    insertion.push_str(&normalized);
    if pad.after {
        insertion.push(' ');
    }

    let mut whole = String::with_capacity(whole_len);
    whole.push_str(&original[..start]);
    whole.push_str(&insertion);
    whole.push_str(&original[end..]);

    let new_end = start + insertion.len();
    Ok(PreparedDictation {
        whole,
        insertion,
        new_end,
    })
}

/// Truncates a microphone device friendly name to at most `MIC_CHOICE_NAME_CAP` (128) UTF-8 bytes
/// without splitting a multi-byte UTF-8 character.
pub fn truncate_mic_name(name: &str) -> &str {
    let trimmed = name.trim();
    if trimmed.len() <= MIC_CHOICE_NAME_CAP {
        return trimmed;
    }
    let cut = clamp_char_boundary_floor(trimmed, MIC_CHOICE_NAME_CAP);
    &trimmed[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_mode_fails_closed_for_unsupported_or_missing_language() {
        assert!(DictationLanguage::Automatic.accepts_detected_tag(Some("en")));
        assert!(DictationLanguage::Automatic.accepts_detected_tag(Some("de-DE")));
        assert!(DictationLanguage::Automatic.accepts_detected_tag(Some("EN-US")));
        assert!(!DictationLanguage::Automatic.accepts_detected_tag(Some("fr")));
        assert!(!DictationLanguage::Automatic.accepts_detected_tag(None));
    }

    #[test]
    fn dictation_language_dword_parse_labels_and_live_paths() {
        assert_eq!(DictationLanguage::Automatic.to_registry_dword(), 0);
        assert_eq!(DictationLanguage::English.to_registry_dword(), 1);
        assert_eq!(DictationLanguage::German.to_registry_dword(), 2);

        assert_eq!(
            DictationLanguage::from_registry_dword(0),
            DictationLanguage::Automatic
        );
        assert_eq!(
            DictationLanguage::from_registry_dword(1),
            DictationLanguage::English
        );
        assert_eq!(
            DictationLanguage::from_registry_dword(2),
            DictationLanguage::German
        );
        assert_eq!(
            DictationLanguage::from_registry_dword(3),
            DictationLanguage::Automatic
        );
        assert_eq!(
            DictationLanguage::from_registry_dword(u32::MAX),
            DictationLanguage::Automatic
        );

        for lang in [
            DictationLanguage::Automatic,
            DictationLanguage::English,
            DictationLanguage::German,
        ] {
            assert_eq!(DictationLanguage::parse(lang.as_str()), Some(lang));
            assert_eq!(
                DictationLanguage::parse(&lang.to_registry_dword().to_string()),
                Some(lang)
            );
            assert!(!lang.label().is_empty());
            assert!(lang.status_text().starts_with("Dictation language: "));
            assert!(lang.deepgram_live_listen_path().contains("encoding=linear16"));
            assert!(lang.deepgram_live_listen_path().contains("sample_rate=16000"));
            assert!(lang.deepgram_live_listen_path().contains("tag=postbox"));
        }
        assert_eq!(DictationLanguage::parse("french"), None);
        assert!(
            DictationLanguage::Automatic
                .deepgram_live_listen_path()
                .contains("language=multi&language_hint=en&language_hint=de")
        );
        assert!(
            DictationLanguage::English
                .deepgram_live_listen_path()
                .contains("language=en&tag=postbox")
        );
        assert!(
            DictationLanguage::German
                .deepgram_live_listen_path()
                .contains("language=de&tag=postbox")
        );
    }

    #[test]
    fn dictation_padding_matches_zig_edit_contract_and_handles_utf8() {
        assert_eq!(
            dictation_padding("one two", 0, 0),
            DictationPadding {
                before: false,
                after: true
            }
        );
        assert_eq!(
            dictation_padding("one two", 3, 4),
            DictationPadding {
                before: true,
                after: true
            }
        );
        assert_eq!(
            dictation_padding("one two", 7, 7),
            DictationPadding {
                before: true,
                after: false
            }
        );
        assert_eq!(
            dictation_padding("", 0, 0),
            DictationPadding {
                before: false,
                after: false
            }
        );
        assert_eq!(
            dictation_padding("one  two", 4, 4),
            DictationPadding {
                before: false,
                after: false
            }
        );
        // UTF-8 multi-byte characters and mid-char clamping never panic.
        let s = "Grüße 😀 Welt";
        let pad = dictation_padding(s, 3, 3); // inside 'ü'
        assert!(pad.before && pad.after);
    }

    #[test]
    fn prepare_dictation_normalizes_validates_and_enforces_bounds() {
        let p = prepare_dictation("one two", 3, 4, "hi\r\nthere\rnow").unwrap();
        assert_eq!(p.insertion, " hi\nthere\nnow ");
        assert_eq!(p.whole, "one hi\nthere\nnow two");
        assert_eq!(p.new_end, 3 + p.insertion.len());

        // Unicode whitespace-only rejected
        assert_eq!(
            prepare_dictation("hi", 2, 2, " \t\r\n\u{00A0}\u{3000} "),
            Err(PrepareDictationError::EmptyTranscript)
        );
        // NUL bytes rejected
        assert_eq!(
            prepare_dictation("hi\0there", 0, 0, "ok"),
            Err(PrepareDictationError::InvalidText)
        );
        assert_eq!(
            prepare_dictation("hi", 0, 0, "ok\0"),
            Err(PrepareDictationError::InvalidTranscript)
        );
        // Invalid selection / non-char-boundary rejected
        let utf8 = "Grüße";
        assert_eq!(
            prepare_dictation(utf8, 3, 3, "ok"),
            Err(PrepareDictationError::InvalidSelection)
        );
        assert_eq!(
            prepare_dictation("abc", 3, 2, "ok"),
            Err(PrepareDictationError::InvalidSelection)
        );
        // 64 KiB draft cap enforced
        let near_max = "a".repeat(MAX_DRAFT_TEXT_BYTES - 1);
        assert_eq!(
            prepare_dictation(&near_max, near_max.len(), near_max.len(), "hi"),
            Err(PrepareDictationError::DraftTooLong)
        );
    }

    #[test]
    fn truncate_mic_name_preserves_utf8_boundaries() {
        let short = "  USB Mic  ";
        assert_eq!(truncate_mic_name(short), "USB Mic");

        // 40 * 4 = 160 bytes -> capped cleanly at 128 bytes (32 emoji)
        let emojis = "🎙".repeat(40);
        let cut = truncate_mic_name(&emojis);
        assert_eq!(cut.len(), 128);
        assert_eq!(cut.chars().count(), 32);

        // 127 ASCII + 2-byte 'ü' = 129 bytes -> cut to 127 bytes
        let mixed = format!("{}ü", "a".repeat(127));
        let cut_mixed = truncate_mic_name(&mixed);
        assert_eq!(cut_mixed.len(), 127);
        assert!(cut_mixed.is_ascii());
    }
}

