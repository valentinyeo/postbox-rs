use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DictationLanguage {
    #[default]
    Automatic,
    English,
    German,
}

impl DictationLanguage {
    pub fn deepgram_listen_path(self) -> &'static str {
        match self {
            Self::Automatic => {
                "/v1/listen?model=nova-3&smart_format=true&detect_language=en&detect_language=de&tag=postbox"
            }
            Self::English => "/v1/listen?model=nova-3&smart_format=true&language=en&tag=postbox",
            Self::German => "/v1/listen?model=nova-3&smart_format=true&language=de&tag=postbox",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_mode_fails_closed_for_unsupported_or_missing_language() {
        assert!(DictationLanguage::Automatic.accepts_detected_tag(Some("en")));
        assert!(DictationLanguage::Automatic.accepts_detected_tag(Some("de-DE")));
        assert!(!DictationLanguage::Automatic.accepts_detected_tag(Some("fr")));
        assert!(!DictationLanguage::Automatic.accepts_detected_tag(None));
    }
}
