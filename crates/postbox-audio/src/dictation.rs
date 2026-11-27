//! Pure-Rust Deepgram Nova-3 live streaming WebSocket dictation (`tokio-tungstenite` + `rustls` ring),
//! exact `LiveTranscript` state machine, peak audio meter, RMS silence gate, and loopback WS tests.

use crate::voice_note::Resampler;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use postbox_core::dictation::{
    DICTATION_BYTES_PER_SEC, DICTATION_CLOSE_STREAM_TIMEOUT_MS, DICTATION_MAX_DURATION_SECS,
    DICTATION_SAMPLE_RATE, DICTATION_TRANSCRIPT_CAPACITY, DictationLanguage,
};
use postbox_platform::{AudioBackend, AudioCaptureStream, CaptureFormat};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use url::{Host, Url};

pub const DEFAULT_DEEPGRAM_WS_BASE_URL: &str = "wss://api.deepgram.com";
pub const LIVE_SAMPLE_RATE: u32 = DICTATION_SAMPLE_RATE;
pub const LIVE_BYTES_PER_SECOND: usize = DICTATION_BYTES_PER_SEC;
pub const MAX_LIVE_AUDIO_BYTES: u64 =
    (DICTATION_BYTES_PER_SEC as u64) * DICTATION_MAX_DURATION_SECS;
pub const MAX_LIVE_WS_FRAME_BYTES: usize = 32_000;
pub const MAX_LIVE_WS_MESSAGE_BYTES: usize = 64 * 1024;
pub const LIVE_TRANSCRIPT_CAPACITY: usize = DICTATION_TRANSCRIPT_CAPACITY;
pub const TIMESTAMP_TOLERANCE: f64 = 1e-6;
pub const SILENCE_RMS_THRESHOLD: f64 = 0.002;
pub const SILENCE_WINDOW_MS: u32 = 20;
pub const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const CLOSE_STREAM_DRAIN_TIMEOUT: Duration =
    Duration::from_millis(DICTATION_CLOSE_STREAM_TIMEOUT_MS);
pub const DICTATION_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(10);

const CLOSE_STREAM_PAYLOAD: &str = r#"{"type":"CloseStream"}"#;
const MAX_PACKETS_PER_TICK: usize = 64;
const ERROR_PEEK_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DictationError {
    #[error("Dictation needs a Deepgram key")]
    MissingKey,
    #[error("Could not start the microphone")]
    MicUnavailable,
    #[error("Dictation endpoint is invalid")]
    InvalidEndpoint,
    #[error("Dictation connection failed")]
    NetworkFailed,
    #[error("Dictation service rejected the request")]
    ApiFailed,
    #[error("Dictation failed")]
    TranscriptionFailed,
    #[error("Dictation only supports English and German in Automatic mode")]
    UnsupportedLanguage,
    #[error("Dictation ended before final transcript arrived")]
    IncompleteTranscript,
    #[error("Dictation timed out waiting for final transcript")]
    FinalTranscriptTimeout,
    #[error("No speech recognized")]
    NoSpeech,
    #[error("Dictation response was invalid")]
    BadResponse,
    #[error("Dictation transcript exceeded maximum length")]
    TranscriptTooLong,
    #[error("Could not capture audio from microphone")]
    CaptureFailed,
}

impl DictationError {
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::MissingKey => "Set DEEPGRAM_API_KEY to enable dictation",
            Self::MicUnavailable => "Could not start microphone capture",
            Self::InvalidEndpoint => "Dictation endpoint is invalid",
            Self::NetworkFailed => "Dictation failed; check the microphone and DEEPGRAM_API_KEY",
            Self::ApiFailed => "Dictation failed; check the microphone and DEEPGRAM_API_KEY",
            Self::TranscriptionFailed => {
                "Dictation failed; check the microphone and DEEPGRAM_API_KEY"
            }
            Self::UnsupportedLanguage => "Automatic dictation supports English and German only",
            Self::IncompleteTranscript => "Dictation ended before final transcript arrived",
            Self::FinalTranscriptTimeout => "Dictation timed out waiting for final transcript",
            Self::NoSpeech => "No speech detected",
            Self::BadResponse => "Dictation response was invalid",
            Self::TranscriptTooLong => "Dictation needs a shorter draft",
            Self::CaptureFailed => "Could not capture audio from microphone",
        }
    }
}

/// Validates a Deepgram WebSocket base URL and constructs the full Nova-3 live listen URI:
/// - `wss://` is accepted for any valid host.
/// - `ws://` is accepted ONLY for strict loopback (`127.0.0.1`, `localhost`, `[::1]`).
/// - Rejects non-WS schemes, userinfo, extra path, query, or fragment.
pub fn build_deepgram_ws_url(
    base_url: &str,
    language: DictationLanguage,
) -> Result<String, DictationError> {
    let trimmed = base_url.trim();
    let parsed = Url::parse(trimmed).map_err(|_| DictationError::InvalidEndpoint)?;

    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(DictationError::InvalidEndpoint);
    }
    if parsed.path() != "" && parsed.path() != "/" {
        return Err(DictationError::InvalidEndpoint);
    }

    let Some(host) = parsed.host() else {
        return Err(DictationError::InvalidEndpoint);
    };

    match parsed.scheme() {
        "wss" => {}
        "ws" => {
            let is_loopback = match host {
                Host::Ipv4(ip) => ip.is_loopback(),
                Host::Ipv6(ip) => ip.is_loopback(),
                Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
            };
            if !is_loopback {
                return Err(DictationError::InvalidEndpoint);
            }
        }
        _ => return Err(DictationError::InvalidEndpoint),
    }

    let base = trimmed.trim_end_matches('/');
    Ok(format!("{base}{}", language.deepgram_live_listen_path()))
}

fn trim_ws_api_key(raw: &str) -> Result<&str, DictationError> {
    let trimmed = raw.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    if trimmed.is_empty() {
        Err(DictationError::MissingKey)
    } else {
        Ok(trimmed)
    }
}

fn build_ws_request(
    ws_url: &str,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, DictationError> {
    let key = trim_ws_api_key(api_key)?;
    let mut req = ws_url
        .into_client_request()
        .map_err(|_| DictationError::InvalidEndpoint)?;
    let auth =
        HeaderValue::from_str(&format!("Token {key}")).map_err(|_| DictationError::MissingKey)?;
    req.headers_mut().insert(AUTHORIZATION, auth);
    Ok(req)
}

/// Builds an explicit pure-Rust `rustls` `ring` connector with bundled WebPKI roots and
/// OS native certificates (zero OpenSSL, zero `aws-lc-rs`, never relies on uninitialized
/// global default CryptoProvider).
fn build_rustls_connector() -> Connector {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    let _ = root_store.add_parsable_certificates(native.certs);

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring provider supports safe default TLS protocol versions")
    .with_root_certificates(root_store)
    .with_no_client_auth();

    Connector::Rustls(Arc::new(config))
}

/// Exact Rust port of Zig `dictation_live.zig` (`LiveTranscript`):
/// - Replay / out-of-order protection (`TIMESTAMP_TOLERANCE = 1e-6`)
/// - Preserves newer interims on fractional boundaries (e.g., `0.1 + 0.2` vs `0.3`)
/// - Fail-closed Automatic EN/DE validation (`words[*].language` checked first without fallback)
/// - Atomic `8192`-byte UTF-8 capacity guard (leaves state intact on `TranscriptTooLong`).
#[derive(Debug, Clone, Default)]
pub struct LiveTranscript {
    committed: String,
    interim: String,
    interim_start: f64,
    final_end: f64,
    final_start: Option<f64>,
}

impl LiveTranscript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed(&self) -> &str {
        &self.committed
    }

    pub fn interim(&self) -> &str {
        &self.interim
    }

    pub fn has_unfinalized_interim(&self) -> bool {
        !self.interim.is_empty()
    }

    pub fn text(&self) -> String {
        if self.committed.is_empty() {
            return self.interim.clone();
        }
        if self.interim.is_empty() {
            return self.committed.clone();
        }
        let mut out = String::with_capacity(self.committed.len() + 1 + self.interim.len());
        out.push_str(&self.committed);
        out.push(' ');
        out.push_str(&self.interim);
        out
    }

    pub fn apply(
        &mut self,
        text: &str,
        is_final: bool,
        start: f64,
        duration: f64,
    ) -> Result<(), DictationError> {
        let end = start + duration;
        if !start.is_finite()
            || !duration.is_finite()
            || !end.is_finite()
            || start < 0.0
            || duration < 0.0
        {
            return Err(DictationError::BadResponse);
        }
        if text.as_bytes().contains(&0) {
            return Err(DictationError::BadResponse);
        }

        // Ignore out-of-order or replayed messages at or before committed timeline.
        if start < self.final_end - TIMESTAMP_TOLERANCE
            || self.final_start.is_some_and(|fs| start <= fs)
        {
            return Ok(());
        }

        if is_final {
            let sep = usize::from(!self.committed.is_empty() && !text.is_empty());
            // Critical IEEE-754 boundary check (`0.1 + 0.2 = 0.30000000000000004` vs `0.3`):
            // strict `>` with `+ TIMESTAMP_TOLERANCE` ensures a late final does not clear
            // a newer interim starting at `0.3`.
            let clears_interim = (end > self.interim_start + TIMESTAMP_TOLERANCE)
                || (start - self.interim_start).abs() <= TIMESTAMP_TOLERANCE;
            let remaining = if clears_interim {
                0
            } else {
                self.interim.len()
            };
            let new_committed_len = self.committed.len() + sep + text.len();
            let sep_remaining = usize::from(remaining > 0 && new_committed_len > 0);
            if new_committed_len + remaining + sep_remaining > LIVE_TRANSCRIPT_CAPACITY {
                return Err(DictationError::TranscriptTooLong);
            }

            if sep == 1 {
                self.committed.push(' ');
            }
            self.committed.push_str(text);
            self.final_start = Some(start);
            self.final_end = end;
            if clears_interim {
                self.interim.clear();
            }
        } else {
            if !self.interim.is_empty() && start < self.interim_start {
                return Ok(());
            }
            let sep = usize::from(!self.committed.is_empty() && !text.is_empty());
            if self.committed.len() + sep + text.len() > LIVE_TRANSCRIPT_CAPACITY {
                return Err(DictationError::TranscriptTooLong);
            }
            self.interim.clear();
            self.interim.push_str(text);
            self.interim_start = start;
        }
        Ok(())
    }

    pub fn receive_json(
        &mut self,
        json: &str,
        language: DictationLanguage,
    ) -> Result<bool, DictationError> {
        let value: Value = serde_json::from_str(json).map_err(|_| DictationError::BadResponse)?;
        let root = value.as_object().ok_or(DictationError::BadResponse)?;
        let msg_type = root
            .get("type")
            .and_then(Value::as_str)
            .ok_or(DictationError::BadResponse)?;

        if msg_type == "Error" {
            return Err(DictationError::TranscriptionFailed);
        }
        if msg_type != "Results" {
            return Ok(false);
        }

        let channel = root
            .get("channel")
            .and_then(Value::as_object)
            .ok_or(DictationError::BadResponse)?;
        let alternatives = channel
            .get("alternatives")
            .and_then(Value::as_array)
            .ok_or(DictationError::BadResponse)?;
        let alt0 = alternatives
            .first()
            .and_then(Value::as_object)
            .ok_or(DictationError::BadResponse)?;
        let transcript = alt0
            .get("transcript")
            .and_then(Value::as_str)
            .ok_or(DictationError::BadResponse)?;

        if language == DictationLanguage::Automatic
            && !transcript.is_empty()
            && !allowed_auto_languages(alt0)
        {
            return Err(DictationError::UnsupportedLanguage);
        }

        let is_final = root
            .get("is_final")
            .and_then(Value::as_bool)
            .ok_or(DictationError::BadResponse)?;
        let start = root
            .get("start")
            .and_then(Value::as_f64)
            .ok_or(DictationError::BadResponse)?;
        let duration = root
            .get("duration")
            .and_then(Value::as_f64)
            .ok_or(DictationError::BadResponse)?;

        self.apply(transcript, is_final, start, duration)?;
        Ok(true)
    }
}

/// Exact fail-closed Automatic EN/DE policy matching `dictation_live.zig:104-124`:
/// - If `words` exists: must be an array; if non-empty, EVERY word must be an object with
///   a `"language"` string accepted by `DictationLanguage::Automatic` (NEVER falls back to
///   `languages` when `words` is non-empty).
/// - Otherwise (`words` absent or empty): `languages` must be a non-empty array whose EVERY
///   element is an accepted language tag.
fn allowed_auto_languages(alt0: &serde_json::Map<String, Value>) -> bool {
    if let Some(words_val) = alt0.get("words") {
        let Some(words) = words_val.as_array() else {
            return false;
        };
        if !words.is_empty() {
            return words.iter().all(|w| {
                w.as_object()
                    .and_then(|obj| obj.get("language"))
                    .and_then(Value::as_str)
                    .is_some_and(|tag| DictationLanguage::Automatic.accepts_detected_tag(Some(tag)))
            });
        }
    }

    let Some(langs) = alt0.get("languages").and_then(Value::as_array) else {
        return false;
    };
    if langs.is_empty() {
        return false;
    }
    langs.iter().all(|l| {
        l.as_str()
            .is_some_and(|tag| DictationLanguage::Automatic.accepts_detected_tag(Some(tag)))
    })
}

/// Peak/RMS audio meter matching Zig `dictation_meter.zig:1-32`.
pub struct DictationMeter;

impl DictationMeter {
    /// Computes peak meter level in `0..=255` (`(peak * 510.0).min(255.0) as u8`, NaN/Inf -> 0).
    pub fn level_from_pcm_bytes(bytes: &[u8], bits: u16, is_float: bool) -> u8 {
        if !bits.is_multiple_of(8) {
            return 0;
        }
        let width = usize::from(bits / 8);
        if width == 0 || width > 4 {
            return 0;
        }
        let mut peak = 0.0f64;
        for chunk in bytes.chunks_exact(width) {
            let sample = if is_float && bits == 32 {
                let raw = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if raw.is_finite() {
                    f64::from(raw.abs())
                } else {
                    0.0
                }
            } else if !is_float && bits == 8 {
                f64::from((i32::from(chunk[0]) - 128).abs()) / 128.0
            } else if !is_float && matches!(bits, 16 | 24 | 32) {
                let raw_u32 = match bits {
                    16 => u32::from(u16::from_le_bytes([chunk[0], chunk[1]])),
                    24 => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]),
                    32 => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                    _ => 0,
                };
                let shifted = raw_u32.wrapping_shl(32 - u32::from(bits)) as i32;
                (i64::from(shifted).abs() as f64) / 2_147_483_648.0
            } else {
                0.0
            };
            if sample > peak {
                peak = sample;
            }
        }
        (peak * 510.0).min(255.0) as u8
    }

    pub fn level_from_i16(samples: &[i16]) -> u8 {
        let mut peak = 0.0f64;
        for &s in samples {
            let amp = f64::from(i32::from(s).abs()) / 32768.0;
            if amp > peak {
                peak = amp;
            }
        }
        (peak * 510.0).min(255.0) as u8
    }

    pub fn rms_i16(samples: &[i16]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut sum_sq = 0.0f64;
        for &s in samples {
            let norm = f64::from(s) / 32768.0;
            sum_sq += norm * norm;
        }
        (sum_sq / samples.len() as f64).sqrt()
    }
}

/// 20 ms windowed RMS silence gate (`>= 0.002` threshold matching `dictation_policy.zig:128-145`).
#[derive(Debug, Clone)]
pub struct SilenceGate {
    window_samples: usize,
    acc_sum_sq: f64,
    acc_count: usize,
    speech_detected: bool,
    last_window_rms: f64,
}

impl SilenceGate {
    pub fn new(sample_rate: u32) -> Self {
        let window_samples = ((sample_rate / (1000 / SILENCE_WINDOW_MS)).max(1)) as usize;
        Self {
            window_samples,
            acc_sum_sq: 0.0,
            acc_count: 0,
            speech_detected: false,
            last_window_rms: 0.0,
        }
    }

    pub fn push_i16(&mut self, samples: &[i16]) {
        for &s in samples {
            let norm = f64::from(s) / 32768.0;
            self.acc_sum_sq += norm * norm;
            self.acc_count += 1;
            if self.acc_count >= self.window_samples {
                self.finish_window();
            }
        }
    }

    pub fn flush(&mut self) {
        if self.acc_count > 0 {
            self.finish_window();
        }
    }

    pub fn speech_detected(&self) -> bool {
        self.speech_detected
    }

    pub fn last_window_rms(&self) -> f64 {
        self.last_window_rms
    }

    fn finish_window(&mut self) {
        let rms = (self.acc_sum_sq / self.acc_count as f64).sqrt();
        self.last_window_rms = rms;
        if !rms.is_finite() || rms >= SILENCE_RMS_THRESHOLD {
            self.speech_detected = true;
        }
        self.acc_sum_sq = 0.0;
        self.acc_count = 0;
    }
}

/// Evaluates 20 ms RMS windows over mono PCM16 samples; returns `true` iff any window
/// (including trailing partial window) has `rms >= 0.002`.
pub fn pcm16_has_speech(samples: &[i16], sample_rate: u32) -> bool {
    if samples.is_empty() {
        return false;
    }
    let mut gate = SilenceGate::new(sample_rate.max(1));
    gate.push_i16(samples);
    gate.flush();
    gate.speech_detected()
}

/// Exact RIFF/WAVE silence gate matching `dictation_policy.zig:81-146`.
/// Fails open (`true`) on malformed or unsupported WAV formats so valid audio is never dropped;
/// returns `false` only for valid PCM16/f32 WAV audio whose every 20 ms window has `rms < 0.002`.
pub fn wav_has_speech(wav: &[u8]) -> bool {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return true;
    }

    let mut pos = 12usize;
    let mut fmt_tag: Option<u16> = None;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut data_slice: Option<&[u8]> = None;

    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        pos += 8;
        if pos + size > wav.len() {
            return true;
        }
        let chunk = &wav[pos..pos + size];
        if id == b"fmt " {
            if chunk.len() < 16 {
                return true;
            }
            let raw_tag = u16::from_le_bytes([chunk[0], chunk[1]]);
            channels = u16::from_le_bytes([chunk[2], chunk[3]]);
            sample_rate = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            bits_per_sample = u16::from_le_bytes([chunk[14], chunk[15]]);
            let resolved_tag = if raw_tag == 0xFFFE {
                if chunk.len() < 40 {
                    return true;
                }
                u16::from_le_bytes([chunk[24], chunk[25]])
            } else {
                raw_tag
            };
            fmt_tag = Some(resolved_tag);
        } else if id == b"data" {
            data_slice = Some(chunk);
        }
        pos += size + (size & 1);
    }

    let (Some(tag), Some(data)) = (fmt_tag, data_slice) else {
        return true;
    };
    if channels == 0 || sample_rate == 0 {
        return true;
    }
    let is_float = match (tag, bits_per_sample) {
        (1, 16) => false,
        (3, 32) => true,
        _ => return true,
    };

    let sample_bytes = usize::from(bits_per_sample / 8);
    let frame_bytes = sample_bytes * usize::from(channels);
    if frame_bytes == 0 {
        return true;
    }
    let total_frames = data.len() / frame_bytes;
    if total_frames == 0 {
        return false;
    }

    let frames_per_window = ((sample_rate / (1000 / SILENCE_WINDOW_MS)).max(1)) as usize;
    let mut frame_idx = 0usize;
    while frame_idx < total_frames {
        let end_frame = (frame_idx + frames_per_window).min(total_frames);
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;
        for frame in frame_idx..end_frame {
            let base = frame * frame_bytes;
            for ch in 0..usize::from(channels) {
                let off = base + ch * sample_bytes;
                let norm = if is_float {
                    let s = f32::from_le_bytes([
                        data[off],
                        data[off + 1],
                        data[off + 2],
                        data[off + 3],
                    ]);
                    if !s.is_finite() {
                        return true;
                    }
                    f64::from(s)
                } else {
                    let s = i16::from_le_bytes([data[off], data[off + 1]]);
                    f64::from(s) / 32768.0
                };
                sum_sq += norm * norm;
                count += 1;
            }
        }
        if count > 0 {
            let rms = (sum_sq / count as f64).sqrt();
            if !rms.is_finite() || rms >= SILENCE_RMS_THRESHOLD {
                return true;
            }
        }
        frame_idx = end_frame;
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveDictationPhase {
    Idle,
    Recording,
    Transcribing,
    Ready,
    Failed,
    Cancelled,
}

impl LiveDictationPhase {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct LiveDictationConfig {
    pub api_key: String,
    pub language: DictationLanguage,
    pub preferred_mic_name: Option<String>,
    pub ws_base_url: String,
    pub connect_timeout: Duration,
    pub drain_timeout: Duration,
    pub capture_poll_interval: Duration,
    pub max_audio_bytes: u64,
}

impl LiveDictationConfig {
    pub fn new(api_key: impl Into<String>, language: DictationLanguage) -> Self {
        Self {
            api_key: api_key.into(),
            language,
            preferred_mic_name: None,
            ws_base_url: DEFAULT_DEEPGRAM_WS_BASE_URL.to_owned(),
            connect_timeout: WS_CONNECT_TIMEOUT,
            drain_timeout: CLOSE_STREAM_DRAIN_TIMEOUT,
            capture_poll_interval: DICTATION_CAPTURE_POLL_INTERVAL,
            max_audio_bytes: MAX_LIVE_AUDIO_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDictationSnapshot {
    pub phase: LiveDictationPhase,
    pub text: String,
    pub revision: u64,
    pub elapsed_ms: u64,
    pub meter_level: u8,
    pub speech_detected: bool,
    pub error: Option<DictationError>,
}

impl Default for LiveDictationSnapshot {
    fn default() -> Self {
        Self {
            phase: LiveDictationPhase::Idle,
            text: String::new(),
            revision: 0,
            elapsed_ms: 0,
            meter_level: 0,
            speech_detected: false,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DictationCommand {
    Stop,
    Cancel,
}

/// Active Deepgram Nova-3 live dictation session backed by a single Tokio task.
/// Dropping the handle automatically sends `Cancel` so no orphan mic/WS task leaks.
pub struct LiveDictationSession {
    cmd_tx: mpsc::UnboundedSender<DictationCommand>,
    state_rx: watch::Receiver<LiveDictationSnapshot>,
}

impl LiveDictationSession {
    pub fn start(
        backend: Arc<dyn AudioBackend>,
        config: LiveDictationConfig,
    ) -> Result<Self, DictationError> {
        let _ = trim_ws_api_key(&config.api_key)?;
        let ws_url = build_deepgram_ws_url(&config.ws_base_url, config.language)?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (state_tx, state_rx) = watch::channel(LiveDictationSnapshot::default());

        tokio::spawn(async move {
            run_dictation_session(backend, config, ws_url, cmd_rx, state_tx).await;
        });

        Ok(Self { cmd_tx, state_rx })
    }

    pub fn snapshot(&self) -> LiveDictationSnapshot {
        self.state_rx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<LiveDictationSnapshot> {
        self.state_rx.clone()
    }

    pub fn request_stop(&self) {
        let _ = self.cmd_tx.send(DictationCommand::Stop);
    }

    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(DictationCommand::Cancel);
    }

    pub async fn wait_for_terminal(&self) -> LiveDictationSnapshot {
        let mut rx = self.state_rx.clone();
        loop {
            let current = rx.borrow().clone();
            if current.phase.is_terminal() {
                return current;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    }
}

impl Drop for LiveDictationSession {
    fn drop(&mut self) {
        if !self.state_rx.borrow().phase.is_terminal() {
            let _ = self.cmd_tx.send(DictationCommand::Cancel);
        }
    }
}

type WsConn = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn publish_snapshot(tx: &watch::Sender<LiveDictationSnapshot>, snap: &LiveDictationSnapshot) {
    let _ = tx.send(snap.clone());
}

fn publish_failed(
    tx: &watch::Sender<LiveDictationSnapshot>,
    snap: &mut LiveDictationSnapshot,
    err: DictationError,
) {
    snap.phase = LiveDictationPhase::Failed;
    snap.meter_level = 0;
    snap.error = Some(err);
    publish_snapshot(tx, snap);
}

fn publish_cancelled(tx: &watch::Sender<LiveDictationSnapshot>, snap: &mut LiveDictationSnapshot) {
    snap.phase = LiveDictationPhase::Cancelled;
    snap.meter_level = 0;
    snap.error = None;
    publish_snapshot(tx, snap);
}

async fn connect_deepgram_ws(
    ws_url: &str,
    api_key: &str,
    connect_timeout: Duration,
) -> Result<WsConn, DictationError> {
    let req = build_ws_request(ws_url, api_key)?;
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_LIVE_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_LIVE_WS_MESSAGE_BYTES));
    let connector = build_rustls_connector();

    let connect_res = tokio::time::timeout(
        connect_timeout,
        tokio_tungstenite::connect_async_tls_with_config(
            req,
            Some(ws_config),
            false,
            Some(connector),
        ),
    )
    .await
    .map_err(|_| DictationError::NetworkFailed)?;

    match connect_res {
        Ok((ws, resp)) => {
            if resp.status().as_u16() != 101 {
                return Err(DictationError::ApiFailed);
            }
            Ok(ws)
        }
        Err(WsError::Http(_)) => Err(DictationError::ApiFailed),
        Err(_) => Err(DictationError::NetworkFailed),
    }
}

fn process_capture_packet(
    pkt: postbox_platform::CapturePacket,
    format: CaptureFormat,
    resampler: &mut Resampler,
    silence_gate: &mut SilenceGate,
    pcm_buf: &mut Vec<i16>,
    out_bytes: &mut Vec<u8>,
) -> u8 {
    let zero_buf;
    let raw_bytes: &[u8] = if pkt.silent {
        let len = if pkt.frames > 0 {
            pkt.frames.saturating_mul(format.block_align())
        } else {
            pkt.data.len()
        };
        zero_buf = vec![0u8; len];
        &zero_buf
    } else {
        &pkt.data
    };

    let meter_level = if pkt.silent {
        0
    } else {
        DictationMeter::level_from_pcm_bytes(raw_bytes, format.bits_per_sample, format.is_float)
    };

    pcm_buf.clear();
    resampler.push_bytes(raw_bytes, pcm_buf);
    silence_gate.push_i16(pcm_buf);

    out_bytes.reserve(pcm_buf.len() * 2);
    for &sample in pcm_buf.iter() {
        out_bytes.extend_from_slice(&sample.to_le_bytes());
    }

    meter_level
}

async fn peek_server_error<S>(
    ws_stream: &mut S,
    transcript: &mut LiveTranscript,
    language: DictationLanguage,
) -> Option<DictationError>
where
    S: futures_util::Stream<Item = Result<Message, WsError>> + Unpin,
{
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(ERROR_PEEK_TIMEOUT, ws_stream.next()).await {
        if let Message::Text(txt) = msg {
            if txt.len() > MAX_LIVE_WS_MESSAGE_BYTES {
                return Some(DictationError::BadResponse);
            }
            if let Err(err) = transcript.receive_json(txt.as_str(), language) {
                return Some(err);
            }
        } else if matches!(msg, Message::Close(_)) {
            break;
        }
    }
    None
}

async fn run_dictation_session(
    backend: Arc<dyn AudioBackend>,
    config: LiveDictationConfig,
    ws_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<DictationCommand>,
    state_tx: watch::Sender<LiveDictationSnapshot>,
) {
    let mut snap = LiveDictationSnapshot::default();

    // 1. Open microphone capture first (fail fast with 0 network calls on MicUnavailable).
    let mut capture: Box<dyn AudioCaptureStream> =
        match backend.open_capture(config.preferred_mic_name.as_deref()) {
            Ok(stream) => stream,
            Err(_) => {
                publish_failed(&state_tx, &mut snap, DictationError::MicUnavailable);
                return;
            }
        };
    let format = capture.format();

    // 2. Connect WebSocket, racing against Cancel.
    let ws = tokio::select! {
        biased;
        cmd = cmd_rx.recv() => {
            match cmd {
                Some(DictationCommand::Cancel) | None => {
                    let _ = capture.stop();
                    publish_cancelled(&state_tx, &mut snap);
                    return;
                }
                Some(DictationCommand::Stop) => {
                    // User requested stop before connect finished: stop mic and fail with NoSpeech.
                    let _ = capture.stop();
                    publish_failed(&state_tx, &mut snap, DictationError::NoSpeech);
                    return;
                }
            }
        }
        res = connect_deepgram_ws(&ws_url, &config.api_key, config.connect_timeout) => {
            match res {
                Ok(conn) => conn,
                Err(err) => {
                    let _ = capture.stop();
                    publish_failed(&state_tx, &mut snap, err);
                    return;
                }
            }
        }
    };

    let (mut ws_sink, mut ws_stream) = ws.split();
    let mut resampler = Resampler::with_output_rate(format, LIVE_SAMPLE_RATE);
    let mut silence_gate = SilenceGate::new(LIVE_SAMPLE_RATE);
    let mut transcript = LiveTranscript::new();
    let mut pcm_buf: Vec<i16> = Vec::with_capacity(2048);
    let mut audio_bytes: Vec<u8> = Vec::with_capacity(4096);
    let mut bytes_sent: u64 = 0;

    snap.phase = LiveDictationPhase::Recording;
    publish_snapshot(&state_tx, &snap);

    let mut capture_interval =
        tokio::time::interval(config.capture_poll_interval.max(Duration::from_millis(1)));
    capture_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 3. Recording loop: capture -> resample -> stream PCM16 + receive live interims/finals.
    'recording: loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(DictationCommand::Cancel) | None => {
                        let _ = capture.stop();
                        let _ = ws_sink.send(Message::Close(None)).await;
                        publish_cancelled(&state_tx, &mut snap);
                        return;
                    }
                    Some(DictationCommand::Stop) => {
                        // Drain any already-buffered packets up to max_audio_bytes before CloseStream.
                        audio_bytes.clear();
                        let mut drained = 0usize;
                        while drained < MAX_PACKETS_PER_TICK && bytes_sent < config.max_audio_bytes {
                            match capture.read_packet() {
                                Ok(Some(pkt)) => {
                                    drained += 1;
                                    let _ = process_capture_packet(
                                        pkt,
                                        format,
                                        &mut resampler,
                                        &mut silence_gate,
                                        &mut pcm_buf,
                                        &mut audio_bytes,
                                    );
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    let _ = capture.stop();
                                    publish_failed(&state_tx, &mut snap, DictationError::CaptureFailed);
                                    return;
                                }
                            }
                        }
                        if !audio_bytes.is_empty() && bytes_sent < config.max_audio_bytes {
                            let remaining = (config.max_audio_bytes - bytes_sent) as usize;
                            let send_len = audio_bytes.len().min(remaining);
                            for chunk in audio_bytes[..send_len].chunks(MAX_LIVE_WS_FRAME_BYTES) {
                                if ws_sink.send(Message::Binary(Bytes::copy_from_slice(chunk))).await.is_err() {
                                    let err = peek_server_error(&mut ws_stream, &mut transcript, config.language)
                                        .await
                                        .unwrap_or(DictationError::NetworkFailed);
                                    let _ = capture.stop();
                                    publish_failed(&state_tx, &mut snap, err);
                                    return;
                                }
                                bytes_sent += chunk.len() as u64;
                            }
                            snap.elapsed_ms = (bytes_sent * 1000) / (LIVE_BYTES_PER_SECOND as u64);
                            snap.speech_detected = silence_gate.speech_detected();
                        }
                        break 'recording;
                    }
                }
            }
            ws_item = ws_stream.next() => {
                match ws_item {
                    Some(Ok(Message::Text(txt))) => {
                        if txt.len() > MAX_LIVE_WS_MESSAGE_BYTES {
                            let _ = capture.stop();
                            publish_failed(&state_tx, &mut snap, DictationError::BadResponse);
                            return;
                        }
                        match transcript.receive_json(txt.as_str(), config.language) {
                            Ok(true) => {
                                let next_text = transcript.text();
                                if next_text != snap.text {
                                    snap.text = next_text;
                                    snap.revision = snap.revision.saturating_add(1);
                                    publish_snapshot(&state_tx, &snap);
                                }
                            }
                            Ok(false) => {}
                            Err(err) => {
                                let _ = capture.stop();
                                let _ = ws_sink.send(Message::Close(None)).await;
                                publish_failed(&state_tx, &mut snap, err);
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws_sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        let _ = capture.stop();
                        publish_failed(&state_tx, &mut snap, DictationError::BadResponse);
                        return;
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(WsError::ConnectionClosed)) => {
                        let _ = capture.stop();
                        publish_failed(&state_tx, &mut snap, DictationError::IncompleteTranscript);
                        return;
                    }
                    Some(Err(_)) => {
                        let _ = capture.stop();
                        publish_failed(&state_tx, &mut snap, DictationError::NetworkFailed);
                        return;
                    }
                }
            }
            _ = capture_interval.tick() => {
                audio_bytes.clear();
                let mut tick_level: u8 = 0;
                let mut read_count = 0usize;
                while read_count < MAX_PACKETS_PER_TICK && bytes_sent < config.max_audio_bytes {
                    match capture.read_packet() {
                        Ok(Some(pkt)) => {
                            read_count += 1;
                            let lvl = process_capture_packet(
                                pkt,
                                format,
                                &mut resampler,
                                &mut silence_gate,
                                &mut pcm_buf,
                                &mut audio_bytes,
                            );
                            if lvl > tick_level {
                                tick_level = lvl;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            let _ = capture.stop();
                            publish_failed(&state_tx, &mut snap, DictationError::CaptureFailed);
                            return;
                        }
                    }
                }

                if read_count > 0 {
                    let remaining = config.max_audio_bytes.saturating_sub(bytes_sent) as usize;
                    let send_len = audio_bytes.len().min(remaining);
                    for chunk in audio_bytes[..send_len].chunks(MAX_LIVE_WS_FRAME_BYTES) {
                        if ws_sink.send(Message::Binary(Bytes::copy_from_slice(chunk))).await.is_err() {
                            let err = peek_server_error(&mut ws_stream, &mut transcript, config.language)
                                .await
                                .unwrap_or(DictationError::NetworkFailed);
                            let _ = capture.stop();
                            publish_failed(&state_tx, &mut snap, err);
                            return;
                        }
                        bytes_sent += chunk.len() as u64;
                    }
                    snap.elapsed_ms = (bytes_sent * 1000) / (LIVE_BYTES_PER_SECOND as u64);
                    snap.meter_level = tick_level;
                    snap.speech_detected = silence_gate.speech_detected();
                    publish_snapshot(&state_tx, &snap);

                    if bytes_sent >= config.max_audio_bytes {
                        break 'recording;
                    }
                }
            }
        }
    }

    // 4. Stop microphone and drain Deepgram CloseStream up to drain_timeout (default 10s).
    let _ = capture.stop();
    silence_gate.flush();
    snap.speech_detected = silence_gate.speech_detected();
    snap.phase = LiveDictationPhase::Transcribing;
    snap.meter_level = 0;
    publish_snapshot(&state_tx, &snap);

    if ws_sink
        .send(Message::Text(CLOSE_STREAM_PAYLOAD.into()))
        .await
        .is_err()
    {
        let err = peek_server_error(&mut ws_stream, &mut transcript, config.language)
            .await
            .unwrap_or(DictationError::NetworkFailed);
        publish_failed(&state_tx, &mut snap, err);
        return;
    }

    let deadline = Instant::now() + config.drain_timeout;
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(DictationCommand::Cancel) | None => {
                        let _ = ws_sink.send(Message::Close(None)).await;
                        publish_cancelled(&state_tx, &mut snap);
                        return;
                    }
                    Some(DictationCommand::Stop) => {}
                }
            }
            _ = &mut sleep => {
                let _ = ws_sink.send(Message::Close(None)).await;
                publish_failed(&state_tx, &mut snap, DictationError::FinalTranscriptTimeout);
                return;
            }
            ws_item = ws_stream.next() => {
                match ws_item {
                    Some(Ok(Message::Text(txt))) => {
                        if txt.len() > MAX_LIVE_WS_MESSAGE_BYTES {
                            publish_failed(&state_tx, &mut snap, DictationError::BadResponse);
                            return;
                        }
                        match transcript.receive_json(txt.as_str(), config.language) {
                            Ok(true) => {
                                let next_text = transcript.text();
                                if next_text != snap.text {
                                    snap.text = next_text;
                                    snap.revision = snap.revision.saturating_add(1);
                                    publish_snapshot(&state_tx, &snap);
                                }
                            }
                            Ok(false) => {}
                            Err(err) => {
                                let _ = ws_sink.send(Message::Close(None)).await;
                                publish_failed(&state_tx, &mut snap, err);
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws_sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        publish_failed(&state_tx, &mut snap, DictationError::BadResponse);
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(WsError::ConnectionClosed)) => {
                        if transcript.has_unfinalized_interim() {
                            publish_failed(&state_tx, &mut snap, DictationError::IncompleteTranscript);
                            return;
                        }
                        break;
                    }
                    Some(Err(_)) => {
                        publish_failed(&state_tx, &mut snap, DictationError::NetworkFailed);
                        return;
                    }
                }
            }
        }
    }

    let final_text = transcript.text();
    if final_text.trim().is_empty() {
        publish_failed(&state_tx, &mut snap, DictationError::NoSpeech);
        return;
    }

    if final_text != snap.text {
        snap.text = final_text;
        snap.revision = snap.revision.saturating_add(1);
    }
    snap.phase = LiveDictationPhase::Ready;
    snap.meter_level = 0;
    snap.error = None;
    publish_snapshot(&state_tx, &snap);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::DeepgramRestClient;
    use postbox_core::voice::TranscriptError;
    use postbox_platform::{CapturePacket, MockAudioBackend};
    use std::future::Future;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse, Request as ServerRequest, Response as ServerResponse,
    };
    use tokio_tungstenite::tungstenite::http::StatusCode;

    struct CapturedHandshake {
        uri: String,
        auth: Option<String>,
    }

    async fn spawn_mock_deepgram_ws<F, Fut>(
        reject_status: Option<u16>,
        handler: F,
    ) -> (String, Arc<Mutex<Option<CapturedHandshake>>>)
    where
        F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);

        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            // Tungstenite requires its unboxed HTTP ErrorResponse in this callback.
            #[allow(clippy::result_large_err)]
            let accept_res = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &ServerRequest, resp: ServerResponse| {
                    let uri = req.uri().to_string();
                    let auth = req
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    *captured_clone.lock().unwrap() = Some(CapturedHandshake { uri, auth });
                    if let Some(code) = reject_status {
                        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::UNAUTHORIZED);
                        let mut err = ErrorResponse::new(Some("rejected".to_owned()));
                        *err.status_mut() = status;
                        return Err(err);
                    }
                    Ok(resp)
                },
            )
            .await;

            if let Ok(ws) = accept_res {
                handler(ws).await;
            }
        });

        (format!("ws://127.0.0.1:{}", addr.port()), captured)
    }

    fn make_results_json(
        transcript: &str,
        is_final: bool,
        start: f64,
        duration: f64,
        words_langs: Option<&[Option<&str>]>,
        alt_langs: Option<&[&str]>,
    ) -> String {
        let mut alt = serde_json::Map::new();
        alt.insert("transcript".into(), Value::String(transcript.to_owned()));
        if let Some(words) = words_langs {
            let arr: Vec<Value> = words
                .iter()
                .map(|lang| {
                    let mut w = serde_json::Map::new();
                    w.insert("word".into(), Value::String("w".into()));
                    if let Some(l) = lang {
                        w.insert("language".into(), Value::String((*l).to_owned()));
                    }
                    Value::Object(w)
                })
                .collect();
            alt.insert("words".into(), Value::Array(arr));
        }
        if let Some(langs) = alt_langs {
            let arr: Vec<Value> = langs
                .iter()
                .map(|l| Value::String((*l).to_owned()))
                .collect();
            alt.insert("languages".into(), Value::Array(arr));
        }
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "start": start,
            "duration": duration,
            "channel": {
                "alternatives": [Value::Object(alt)]
            }
        })
        .to_string()
    }

    fn build_pcm16_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let riff_size = 36 + data_len;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn ws_url_builder_validates_schemes_loopback_and_language_paths() {
        let auto =
            build_deepgram_ws_url("wss://api.deepgram.com", DictationLanguage::Automatic).unwrap();
        assert_eq!(
            auto,
            "wss://api.deepgram.com/v1/listen?model=nova-3&smart_format=true&interim_results=true&encoding=linear16&sample_rate=16000&channels=1&language=multi&language_hint=en&language_hint=de&tag=postbox"
        );
        let en =
            build_deepgram_ws_url("wss://api.deepgram.com/", DictationLanguage::English).unwrap();
        assert!(en.ends_with("&language=en&tag=postbox"));
        let de =
            build_deepgram_ws_url("wss://api.deepgram.com", DictationLanguage::German).unwrap();
        assert!(de.ends_with("&language=de&tag=postbox"));

        // Loopback ws:// accepted
        assert!(build_deepgram_ws_url("ws://127.0.0.1:9000", DictationLanguage::Automatic).is_ok());
        assert!(build_deepgram_ws_url("ws://localhost:9000", DictationLanguage::English).is_ok());
        assert!(build_deepgram_ws_url("ws://[::1]:9000", DictationLanguage::German).is_ok());

        // Non-loopback ws://, http://, userinfo, extra path/query/fragment rejected
        for bad in [
            "ws://api.deepgram.com",
            "ws://10.0.0.1:8080",
            "ws://127.0.0.1.evil.com:8080",
            "ws://[::ffff:127.0.0.1]:8080",
            "http://127.0.0.1:8080",
            "https://api.deepgram.com",
            "wss://user:pass@api.deepgram.com",
            "wss://api.deepgram.com/custom",
            "wss://api.deepgram.com/?foo=bar",
            "wss://api.deepgram.com/#frag",
        ] {
            assert_eq!(
                build_deepgram_ws_url(bad, DictationLanguage::Automatic),
                Err(DictationError::InvalidEndpoint),
                "expected InvalidEndpoint for {bad}"
            );
        }
    }

    #[test]
    fn live_transcript_unit_parity() {
        // 1. Interim replace + final commit + replay dedup
        let mut t = LiveTranscript::new();
        t.apply("hel", false, 0.0, 0.5).unwrap();
        assert_eq!(t.text(), "hel");
        t.apply("hello world", true, 0.0, 1.0).unwrap();
        assert_eq!(t.text(), "hello world");
        assert!(!t.has_unfinalized_interim());
        // Duplicate replay ignored
        t.apply("hello world", true, 0.0, 1.0).unwrap();
        assert_eq!(t.text(), "hello world");

        // 2. Late final preserves newer interim
        let mut t2 = LiveTranscript::new();
        t2.apply("first", false, 0.0, 1.0).unwrap();
        t2.apply("second", false, 1.0, 0.5).unwrap();
        assert_eq!(t2.text(), "second");
        t2.apply("first", true, 0.0, 1.0).unwrap();
        assert_eq!(t2.text(), "first second");
        assert!(t2.has_unfinalized_interim());

        // 3. Genuine repeated speech + shrinking interim
        let mut t3 = LiveTranscript::new();
        t3.apply("yes", true, 0.0, 0.5).unwrap();
        t3.apply("yes", true, 0.5, 0.5).unwrap();
        assert_eq!(t3.text(), "yes yes");
        t3.apply("maybe later", false, 1.0, 0.5).unwrap();
        t3.apply("maybe", false, 1.0, 0.3).unwrap();
        assert_eq!(t3.text(), "yes yes maybe");
        t3.apply("", false, 1.0, 0.1).unwrap();
        assert_eq!(t3.text(), "yes yes");

        // 4. Bad timestamps, NUL bytes, and atomic 8192-byte UTF-8 capacity bound
        let mut t4 = LiveTranscript::new();
        assert_eq!(
            t4.apply("bad", true, f64::NAN, 1.0),
            Err(DictationError::BadResponse)
        );
        assert_eq!(
            t4.apply("bad", true, -0.1, 1.0),
            Err(DictationError::BadResponse)
        );
        assert_eq!(
            t4.apply("bad\0nul", true, 0.0, 1.0),
            Err(DictationError::BadResponse)
        );
        let fill = "ä".repeat(LIVE_TRANSCRIPT_CAPACITY / 2); // exact 8192 UTF-8 bytes
        t4.apply(&fill, true, 0.0, 1.0).unwrap();
        assert_eq!(t4.text().len(), LIVE_TRANSCRIPT_CAPACITY);
        assert_eq!(
            t4.apply("x", false, 1.0, 0.5),
            Err(DictationError::TranscriptTooLong)
        );
        // Atomic: original committed text remains intact
        assert_eq!(t4.text().len(), LIVE_TRANSCRIPT_CAPACITY);
        assert!(!t4.has_unfinalized_interim());

        // 5. Fractional IEEE-754 boundary 0.1 + 0.2 vs 0.3 preserves newer interim
        let mut t5 = LiveTranscript::new();
        t5.apply("next", false, 0.3, 0.2).unwrap();
        t5.apply("prev", true, 0.1, 0.2).unwrap();
        assert_eq!(t5.text(), "prev next");
        assert!(t5.has_unfinalized_interim());

        // 6. Metadata ignored, Error -> TranscriptionFailed, Auto fail-closed (words overrides languages!)
        let mut t6 = LiveTranscript::new();
        assert_eq!(
            t6.receive_json(
                r#"{"type":"Metadata","duration":1.2}"#,
                DictationLanguage::Automatic
            ),
            Ok(false)
        );
        assert_eq!(
            t6.receive_json(
                r#"{"type":"Error","description":"boom"}"#,
                DictationLanguage::Automatic
            ),
            Err(DictationError::TranscriptionFailed)
        );
        // Non-empty words containing "fr" MUST reject even when languages says ["en"]!
        let fr_words_en_alt = make_results_json(
            "bonjour",
            true,
            0.0,
            0.5,
            Some(&[Some("fr")]),
            Some(&["en"]),
        );
        assert_eq!(
            t6.receive_json(&fr_words_en_alt, DictationLanguage::Automatic),
            Err(DictationError::UnsupportedLanguage)
        );
        // Non-empty words missing language MUST reject even when languages says ["en"]!
        let missing_word_lang =
            make_results_json("hello", true, 0.0, 0.5, Some(&[None]), Some(&["en"]));
        assert_eq!(
            t6.receive_json(&missing_word_lang, DictationLanguage::Automatic),
            Err(DictationError::UnsupportedLanguage)
        );
        // Empty words falls back to languages=["de-DE"] -> accepted!
        let empty_words_de =
            make_results_json("hallo", true, 0.0, 0.5, Some(&[]), Some(&["de-DE"]));
        assert_eq!(
            t6.receive_json(&empty_words_de, DictationLanguage::Automatic),
            Ok(true)
        );
        assert_eq!(t6.text(), "hallo");
    }

    #[tokio::test]
    async fn dictation_meter_and_silence_gate_parity() {
        // Exact dictation_meter.zig:24-31 parity
        assert_eq!(DictationMeter::level_from_pcm_bytes(&[], 16, false), 0);
        assert_eq!(
            DictationMeter::level_from_pcm_bytes(&[0x00, 0x00, 0x00, 0x00], 16, false),
            0
        );
        assert_eq!(
            DictationMeter::level_from_pcm_bytes(&0x4000i16.to_le_bytes(), 16, false),
            255
        );
        assert_eq!(
            DictationMeter::level_from_pcm_bytes(&0.5f32.to_le_bytes(), 32, true),
            255
        );
        assert_eq!(
            DictationMeter::level_from_pcm_bytes(&f32::NAN.to_le_bytes(), 32, true),
            0
        );
        assert_eq!(
            DictationMeter::level_from_pcm_bytes(&f32::INFINITY.to_le_bytes(), 32, true),
            0
        );
        // 8-bit unsigned silence = 128, half-scale = 192 -> 255
        assert_eq!(DictationMeter::level_from_pcm_bytes(&[128], 8, false), 0);
        assert_eq!(DictationMeter::level_from_pcm_bytes(&[192], 8, false), 255);

        // Exact 0.002 RMS threshold: 65 / 32768 = 0.0019836 < 0.002; 66 / 32768 = 0.002014 >= 0.002
        let below = vec![65i16; 320];
        let above = vec![66i16; 320];
        assert!(!pcm16_has_speech(&below, 16_000));
        assert!(pcm16_has_speech(&above, 16_000));

        let silent_wav = build_pcm16_wav(16_000, &below);
        let speech_wav = build_pcm16_wav(16_000, &above);
        assert!(!wav_has_speech(&silent_wav));
        assert!(wav_has_speech(&speech_wav));
        // Malformed WAV fails open
        assert!(wav_has_speech(b"not a wav"));

        // Silent WAV in DeepgramRestClient makes ZERO HTTP calls!
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((_s, _)) = listener.accept().await {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            }
        });
        let client = DeepgramRestClient::new().with_base_url(format!("http://{addr}"));
        let res = client
            .transcribe_bytes(
                &silent_wav,
                "audio/wav",
                DictationLanguage::Automatic,
                "test-key",
                |_| None,
            )
            .await;
        assert_eq!(res, Err(TranscriptError::NoSpeech));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn live_session_streams_resampled_48k_stereo_and_drains_closestream_without_duplication()
    {
        let received_bytes = Arc::new(AtomicUsize::new(0));
        let received_bytes_clone = Arc::clone(&received_bytes);
        let saw_close_stream = Arc::new(AtomicUsize::new(0));
        let saw_close_clone = Arc::clone(&saw_close_stream);

        let (base_url, handshake) = spawn_mock_deepgram_ws(None, move |mut ws| async move {
            // Wait for at least one binary frame, then send evolving interims
            let mut sent_interims = false;
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(data) => {
                        received_bytes_clone.fetch_add(data.len(), Ordering::SeqCst);
                        if !sent_interims {
                            sent_interims = true;
                            let _ = ws
                                .send(Message::Text(
                                    make_results_json(
                                        "hello",
                                        false,
                                        0.0,
                                        0.3,
                                        Some(&[Some("en")]),
                                        None,
                                    )
                                    .into(),
                                ))
                                .await;
                            let _ = ws
                                .send(Message::Text(
                                    make_results_json(
                                        "hello world",
                                        false,
                                        0.0,
                                        0.6,
                                        Some(&[Some("en"), Some("en")]),
                                        None,
                                    )
                                    .into(),
                                ))
                                .await;
                        }
                    }
                    Message::Text(txt) if txt.as_str() == CLOSE_STREAM_PAYLOAD => {
                        saw_close_clone.fetch_add(1, Ordering::SeqCst);
                        let _ = ws
                            .send(Message::Text(
                                make_results_json(
                                    "Hello world.",
                                    true,
                                    0.0,
                                    0.8,
                                    Some(&[Some("en"), Some("en")]),
                                    None,
                                )
                                .into(),
                            ))
                            .await;
                        let _ = ws.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;

        let backend = Arc::new(MockAudioBackend::new());
        backend.set_capture_format(CaptureFormat {
            sample_rate: 48_000,
            channels: 2,
            is_float: true,
            bits_per_sample: 32,
        });
        // 4800 stereo frames @ 48 kHz = 100 ms -> ~1600 mono i16 samples = ~3200 bytes @ 16 kHz
        let mut stereo_f32 = Vec::with_capacity(4800 * 2);
        for i in 0..4800 {
            let v = ((i as f32 * 0.05).sin()) * 0.4;
            stereo_f32.push(v);
            stereo_f32.push(v);
        }
        backend.push_capture_f32(&stereo_f32, 2, 480);

        let mut cfg = LiveDictationConfig::new("dg-live-secret", DictationLanguage::Automatic);
        cfg.ws_base_url = base_url;
        cfg.preferred_mic_name = Some("Studio USB".into());
        let session = LiveDictationSession::start(backend.clone(), cfg).unwrap();

        let mut rx = session.subscribe();
        let mut saw_interim = false;
        let mut max_level = 0u8;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let s = rx.borrow().clone();
                if s.meter_level > max_level {
                    max_level = s.meter_level;
                }
                if s.text == "hello world" {
                    saw_interim = true;
                    break;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        assert!(saw_interim);
        assert!(max_level > 0);
        session.request_stop();

        let terminal = tokio::time::timeout(Duration::from_secs(3), session.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(terminal.phase, LiveDictationPhase::Ready);
        assert_eq!(terminal.text, "Hello world.");
        assert!(terminal.speech_detected);
        assert_eq!(saw_close_stream.load(Ordering::SeqCst), 1);
        assert_eq!(backend.last_capture_mic().as_deref(), Some("Studio USB"));
        // 100 ms of 16 kHz mono PCM16 = 3200 bytes
        let got_bytes = received_bytes.load(Ordering::SeqCst);
        assert!(
            (3000..=3400).contains(&got_bytes),
            "expected ~3200 resampled bytes, got {got_bytes}"
        );

        let hs = handshake.lock().unwrap().take().unwrap();
        assert_eq!(hs.auth.as_deref(), Some("Token dg-live-secret"));
        assert!(
            hs.uri
                .contains("language=multi&language_hint=en&language_hint=de&tag=postbox")
        );
    }

    #[tokio::test]
    async fn live_session_replayed_and_out_of_order_finals_over_ws() {
        let (base_url, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(txt) = msg
                    && txt.as_str() == CLOSE_STREAM_PAYLOAD
                {
                    let frames = [
                        make_results_json("next", false, 0.3, 0.2, Some(&[Some("en")]), None),
                        make_results_json("prev", true, 0.1, 0.2, Some(&[Some("en")]), None),
                        make_results_json("prev", true, 0.1, 0.2, Some(&[Some("en")]), None), // replay
                        make_results_json("next", true, 0.3, 0.2, Some(&[Some("en")]), None),
                    ];
                    for f in frames {
                        let _ = ws.send(Message::Text(f.into())).await;
                    }
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
        })
        .await;

        let backend = Arc::new(MockAudioBackend::new());
        backend.push_capture_i16(&vec![1000i16; 320], 1, 320);
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = base_url;
        let session = LiveDictationSession::start(backend, cfg).unwrap();

        let mut rx = session.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while rx.borrow().phase != LiveDictationPhase::Recording {
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        session.request_stop();
        let terminal = tokio::time::timeout(Duration::from_secs(2), session.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(terminal.phase, LiveDictationPhase::Ready);
        assert_eq!(terminal.text, "prev next");
    }

    #[tokio::test]
    async fn live_session_all_three_language_modes_and_auto_fail_closed_reject() {
        // Automatic mode rejects French -> UnsupportedLanguage
        let (base_url, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            let _ = ws
                .send(Message::Text(
                    make_results_json("bonjour", true, 0.0, 0.4, Some(&[Some("fr")]), None).into(),
                ))
                .await;
            let _ = ws.send(Message::Close(None)).await;
        })
        .await;

        let backend = Arc::new(MockAudioBackend::new());
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = base_url;
        let session = LiveDictationSession::start(backend, cfg).unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(2), session.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(terminal.phase, LiveDictationPhase::Failed);
        assert_eq!(terminal.error, Some(DictationError::UnsupportedLanguage));

        // English & German modes verify exact paths and accept empty words/no language tags
        for (lang, expected_param) in [
            (DictationLanguage::English, "language=en&tag=postbox"),
            (DictationLanguage::German, "language=de&tag=postbox"),
        ] {
            let (url, hs) = spawn_mock_deepgram_ws(None, |mut ws| async move {
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(txt) = msg
                        && txt.as_str() == CLOSE_STREAM_PAYLOAD
                    {
                        let _ = ws
                            .send(Message::Text(
                                make_results_json("ok", true, 0.0, 0.3, Some(&[]), None).into(),
                            ))
                            .await;
                        let _ = ws.send(Message::Close(None)).await;
                        break;
                    }
                }
            })
            .await;

            let backend = Arc::new(MockAudioBackend::new());
            let mut cfg = LiveDictationConfig::new("key", lang);
            cfg.ws_base_url = url;
            let session = LiveDictationSession::start(backend, cfg).unwrap();
            let mut rx = session.subscribe();
            while rx.borrow().phase != LiveDictationPhase::Recording {
                rx.changed().await.unwrap();
            }
            session.request_stop();
            let term = tokio::time::timeout(Duration::from_secs(2), session.wait_for_terminal())
                .await
                .unwrap();
            assert_eq!(term.phase, LiveDictationPhase::Ready);
            assert_eq!(term.text, "ok");
            assert!(
                hs.lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .uri
                    .contains(expected_param)
            );
        }
    }

    #[tokio::test]
    async fn live_session_error_mappings() {
        // 1. Empty key -> MissingKey synchronously
        let backend = Arc::new(MockAudioBackend::new());
        assert!(matches!(
            LiveDictationSession::start(
                backend.clone(),
                LiveDictationConfig::new("   ", DictationLanguage::Automatic)
            ),
            Err(DictationError::MissingKey)
        ));

        // 2. MicUnavailable
        backend.set_fail_capture(true);
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = "ws://127.0.0.1:1".into();
        let mic_sess = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        let term = mic_sess.wait_for_terminal().await;
        assert_eq!(term.error, Some(DictationError::MicUnavailable));
        backend.set_fail_capture(false);

        // 3. Non-101 HTTP 401 -> ApiFailed
        let (url_401, _) = spawn_mock_deepgram_ws(Some(401), |_| async {}).await;
        let mut cfg = LiveDictationConfig::new("bad-key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_401;
        let s401 = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        assert_eq!(
            s401.wait_for_terminal().await.error,
            Some(DictationError::ApiFailed)
        );

        // 4. Server sends {"type":"Error"} then closes -> TranscriptionFailed
        let (url_err, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            let _ = ws
                .send(Message::Text(
                    r#"{"type":"Error","description":"bad audio"}"#.into(),
                ))
                .await;
            let _ = ws.send(Message::Close(None)).await;
        })
        .await;
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_err;
        let serr = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        assert_eq!(
            serr.wait_for_terminal().await.error,
            Some(DictationError::TranscriptionFailed)
        );

        // 5. Premature close during Recording -> IncompleteTranscript
        let (url_premature, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            let _ = ws.send(Message::Close(None)).await;
        })
        .await;
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_premature;
        let sprem = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        assert_eq!(
            sprem.wait_for_terminal().await.error,
            Some(DictationError::IncompleteTranscript)
        );

        // 6. Close after CloseStream with unfinalized interim -> IncompleteTranscript
        let (url_unfinal, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(txt) = msg
                    && txt.as_str() == CLOSE_STREAM_PAYLOAD
                {
                    let _ = ws
                        .send(Message::Text(
                            make_results_json(
                                "dangling",
                                false,
                                0.0,
                                0.4,
                                Some(&[Some("en")]),
                                None,
                            )
                            .into(),
                        ))
                        .await;
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
        })
        .await;
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_unfinal;
        let sunf = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        let mut rx = sunf.subscribe();
        while rx.borrow().phase != LiveDictationPhase::Recording {
            rx.changed().await.unwrap();
        }
        sunf.request_stop();
        assert_eq!(
            sunf.wait_for_terminal().await.error,
            Some(DictationError::IncompleteTranscript)
        );

        // 7. CloseStream drain timeout -> FinalTranscriptTimeout
        let (url_timeout, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(_)) = ws.next().await {
                // Intentionally never close after CloseStream
            }
        })
        .await;
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_timeout;
        cfg.drain_timeout = Duration::from_millis(80);
        let sto = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        let mut rx = sto.subscribe();
        while rx.borrow().phase != LiveDictationPhase::Recording {
            rx.changed().await.unwrap();
        }
        sto.request_stop();
        assert_eq!(
            sto.wait_for_terminal().await.error,
            Some(DictationError::FinalTranscriptTimeout)
        );

        // 8. Empty final -> NoSpeech
        let (url_empty, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(txt) = msg
                    && txt.as_str() == CLOSE_STREAM_PAYLOAD
                {
                    let _ = ws
                        .send(Message::Text(
                            make_results_json("", true, 0.0, 0.4, None, None).into(),
                        ))
                        .await;
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            }
        })
        .await;
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url_empty;
        let sempty = LiveDictationSession::start(backend, cfg).unwrap();
        let mut rx = sempty.subscribe();
        while rx.borrow().phase != LiveDictationPhase::Recording {
            rx.changed().await.unwrap();
        }
        sempty.request_stop();
        assert_eq!(
            sempty.wait_for_terminal().await.error,
            Some(DictationError::NoSpeech)
        );
    }

    #[tokio::test]
    async fn live_session_enforces_max_audio_bytes_cap_and_silent_packets() {
        let recorded = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded_clone = Arc::clone(&recorded);

        let (base_url, _) = spawn_mock_deepgram_ws(None, move |mut ws| async move {
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(b) => recorded_clone.lock().unwrap().extend_from_slice(&b),
                    Message::Text(txt) if txt.as_str() == CLOSE_STREAM_PAYLOAD => {
                        let _ = ws
                            .send(Message::Text(
                                make_results_json(
                                    "capped",
                                    true,
                                    0.0,
                                    0.1,
                                    Some(&[Some("en")]),
                                    None,
                                )
                                .into(),
                            ))
                            .await;
                        let _ = ws.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;

        let backend = Arc::new(MockAudioBackend::new());
        backend.set_capture_format(CaptureFormat {
            sample_rate: 16_000,
            channels: 1,
            is_float: false,
            bits_per_sample: 16,
        });
        // Push a silent packet with noisy garbage data -> MUST send pure zero bytes & meter_level == 0!
        backend.push_capture_packet(CapturePacket {
            data: vec![0x7F; 2000 * 2],
            frames: 2000,
            silent: true,
        });

        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = base_url;
        cfg.max_audio_bytes = 3200; // exact 100 ms cap
        let session = LiveDictationSession::start(backend, cfg).unwrap();

        let term = tokio::time::timeout(Duration::from_secs(3), session.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(term.phase, LiveDictationPhase::Ready);
        assert_eq!(term.text, "capped");
        assert_eq!(term.elapsed_ms, 100);
        assert!(!term.speech_detected);

        let bytes = recorded.lock().unwrap();
        assert_eq!(bytes.len(), 3200);
        assert!(
            bytes.iter().all(|&b| b == 0),
            "silent packet must emit pure zeros"
        );
    }

    #[tokio::test]
    async fn live_session_cancel_mid_recording_and_mid_drain() {
        // Cancel mid-recording
        let (url1, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(_)) = ws.next().await {}
        })
        .await;
        let backend = Arc::new(MockAudioBackend::new());
        let mut cfg = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg.ws_base_url = url1;
        let s1 = LiveDictationSession::start(backend.clone(), cfg).unwrap();
        let mut rx1 = s1.subscribe();
        while rx1.borrow().phase != LiveDictationPhase::Recording {
            rx1.changed().await.unwrap();
        }
        s1.cancel();
        let term1 = tokio::time::timeout(Duration::from_secs(2), s1.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(term1.phase, LiveDictationPhase::Cancelled);

        // Cancel mid-drain (must return immediately without waiting for 10s drain_timeout)
        let (url2, _) = spawn_mock_deepgram_ws(None, |mut ws| async move {
            while let Some(Ok(_)) = ws.next().await {}
        })
        .await;
        let mut cfg2 = LiveDictationConfig::new("key", DictationLanguage::Automatic);
        cfg2.ws_base_url = url2;
        cfg2.drain_timeout = Duration::from_secs(10);
        let s2 = LiveDictationSession::start(backend, cfg2).unwrap();
        let mut rx2 = s2.subscribe();
        while rx2.borrow().phase != LiveDictationPhase::Recording {
            rx2.changed().await.unwrap();
        }
        s2.request_stop();
        while rx2.borrow().phase != LiveDictationPhase::Transcribing {
            rx2.changed().await.unwrap();
        }
        let started = Instant::now();
        s2.cancel();
        let term2 = tokio::time::timeout(Duration::from_secs(2), s2.wait_for_terminal())
            .await
            .unwrap();
        assert_eq!(term2.phase, LiveDictationPhase::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
