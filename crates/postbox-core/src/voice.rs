use crate::ChatKey;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const VOICE_MIN_DURATION_MS: u64 = 250;
pub const VOICE_MAX_DURATION_SECS: u32 = 300;
pub const PLAYED_VOICE_CAP: usize = 2048;
pub const VOICE_POS_CAP: usize = 64;
pub const VOICE_POS_END_CLEAR_MS: u64 = 500;
pub const VOICE_SENT_FIFO_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlaybackSpeed {
    #[default]
    X1 = 100,
    X1_5 = 150,
    X2 = 200,
}

impl PlaybackSpeed {
    pub const fn percent(self) -> u16 {
        self as u16
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::X1 => "1x",
            Self::X1_5 => "1.5x",
            Self::X2 => "2x",
        }
    }

    pub const fn cycle_next(self) -> Self {
        match self {
            Self::X1 => Self::X1_5,
            Self::X1_5 => Self::X2,
            Self::X2 => Self::X1,
        }
    }

    pub const fn from_percent(percent: u16) -> Self {
        match percent {
            150 => Self::X1_5,
            200 => Self::X2,
            _ => Self::X1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VoicePlaybackStatus {
    #[default]
    Empty,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePlaybackState {
    pub chat: ChatKey,
    pub msg_id: String,
    pub status: VoicePlaybackStatus,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub speed: PlaybackSpeed,
    pub waveform: Vec<u8>,
}

impl VoicePlaybackState {
    pub fn is_playing(&self) -> bool {
        self.status == VoicePlaybackStatus::Playing
    }

    pub fn is_active(&self) -> bool {
        self.status != VoicePlaybackStatus::Empty
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VoiceRecordState {
    #[default]
    Idle,
    Recording {
        origin_chat: ChatKey,
        started_at_ms: u64,
        elapsed_ms: u64,
        live_bars: Vec<u8>,
    },
    Retained {
        origin_chat: ChatKey,
        staged_path: PathBuf,
        sha256: [u8; 32],
        file_bytes: u64,
        duration_secs: u32,
        waveform: Vec<u8>,
    },
}

impl VoiceRecordState {
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }

    pub fn is_retained(&self) -> bool {
        matches!(self, Self::Retained { .. })
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn origin_chat(&self) -> Option<&ChatKey> {
        match self {
            Self::Idle => None,
            Self::Recording { origin_chat, .. } | Self::Retained { origin_chat, .. } => {
                Some(origin_chat)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    pub media_sha256: String,
    pub language: String,
    pub raw_text: String,
    pub formatted_text: Option<String>,
    pub updated_at: i64,
}

impl TranscriptRecord {
    /// Text shown in the UI: prefers optional formatted summary when present, else raw text.
    pub fn display_text(&self) -> &str {
        self.formatted_text
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.raw_text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptError {
    NoSpeech,
    LocalFileUnavailable,
    BadAudio,
    Unauthorized,
    PaymentRequired,
    RateLimited,
    NetworkFailed,
    TranscriptionUnavailable,
    MissingKey,
    ApiFailed,
}

impl TranscriptError {
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::NoSpeech => "No speech recognized",
            Self::LocalFileUnavailable => "Voice file could not be read",
            Self::BadAudio => "Voice audio was rejected",
            Self::Unauthorized => "Transcription key rejected",
            Self::PaymentRequired => "Transcription credits exhausted",
            Self::RateLimited => "Transcription limit reached",
            Self::NetworkFailed => "Transcription connection failed",
            Self::TranscriptionUnavailable => "Transcription service unavailable",
            Self::MissingKey => "Transcription needs a Deepgram key",
            Self::ApiFailed => "Transcription failed",
        }
    }
}

impl std::fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.user_message())
    }
}

impl std::error::Error for TranscriptError {}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TranscriptStatus {
    #[default]
    None,
    Transcribing,
    Ready(TranscriptRecord),
    Failed(TranscriptError),
}

impl TranscriptStatus {
    pub fn display_text(&self) -> Option<&str> {
        match self {
            Self::Ready(rec) => Some(rec.display_text()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub friendly_name: String,
    pub is_default: bool,
}

/// Formats milliseconds as `m:ss` (e.g., `0:07`, `2:05`).
pub fn format_voice_clock(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_speed_cycles_and_parses() {
        assert_eq!(PlaybackSpeed::X1.percent(), 100);
        assert_eq!(PlaybackSpeed::X1.label(), "1x");
        assert_eq!(PlaybackSpeed::X1.cycle_next(), PlaybackSpeed::X1_5);
        assert_eq!(PlaybackSpeed::X1_5.cycle_next(), PlaybackSpeed::X2);
        assert_eq!(PlaybackSpeed::X2.cycle_next(), PlaybackSpeed::X1);
        assert_eq!(PlaybackSpeed::from_percent(150), PlaybackSpeed::X1_5);
        assert_eq!(PlaybackSpeed::from_percent(200), PlaybackSpeed::X2);
        assert_eq!(PlaybackSpeed::from_percent(999), PlaybackSpeed::X1);
    }

    #[test]
    fn transcript_error_messages_match_spec_verbatim() {
        assert_eq!(
            TranscriptError::NoSpeech.user_message(),
            "No speech recognized"
        );
        assert_eq!(
            TranscriptError::LocalFileUnavailable.user_message(),
            "Voice file could not be read"
        );
        assert_eq!(
            TranscriptError::BadAudio.user_message(),
            "Voice audio was rejected"
        );
        assert_eq!(
            TranscriptError::Unauthorized.user_message(),
            "Transcription key rejected"
        );
        assert_eq!(
            TranscriptError::PaymentRequired.user_message(),
            "Transcription credits exhausted"
        );
        assert_eq!(
            TranscriptError::RateLimited.user_message(),
            "Transcription limit reached"
        );
        assert_eq!(
            TranscriptError::NetworkFailed.user_message(),
            "Transcription connection failed"
        );
        assert_eq!(
            TranscriptError::TranscriptionUnavailable.user_message(),
            "Transcription service unavailable"
        );
        assert_eq!(
            TranscriptError::MissingKey.user_message(),
            "Transcription needs a Deepgram key"
        );
        assert_eq!(
            TranscriptError::ApiFailed.user_message(),
            "Transcription failed"
        );
    }

    #[test]
    fn voice_clock_formats_minutes_and_padded_seconds() {
        assert_eq!(format_voice_clock(0), "0:00");
        assert_eq!(format_voice_clock(7_400), "0:07");
        assert_eq!(format_voice_clock(125_000), "2:05");
    }
}
