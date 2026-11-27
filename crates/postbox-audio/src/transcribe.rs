//! Deepgram Nova-3 REST voice-note transcription, EN/DE fail-closed Auto policy,
//! single `audio/ogg` `NoSpeech` `language=multi` fallback, single `401`/`403` key-refresh retry,
//! immutable dual `.txt` / atomic `.raw.txt` sidecars, and raw marked transcript export.

use crate::MAX_IDLE_CONNECTIONS_PER_HOST;
use crate::format::looks_formatted;
use postbox_core::dictation::DictationLanguage;
use postbox_core::voice::TranscriptError;
use postbox_store::Store;
use reqwest::Client;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const DEFAULT_DEEPGRAM_BASE_URL: &str = "https://api.deepgram.com";
pub const MAX_TRANSCRIBE_AUDIO_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_SIDECAR_TEXT_BYTES: usize = 64 * 1024;
pub const DEEPGRAM_HTTP_TIMEOUT: Duration = Duration::from_secs(120);

static ATOMIC_RAW_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionResult {
    pub text: String,
    pub language: String,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
struct ParsedDeepgramResponse {
    text: String,
    detected_en_de: Option<DictationLanguage>,
    detected_raw: Option<String>,
    confidence: f64,
}

#[derive(Debug, Clone)]
pub struct DeepgramRestClient {
    client: Client,
    base_url: String,
}

impl Default for DeepgramRestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepgramRestClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(DEEPGRAM_HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            base_url: DEFAULT_DEEPGRAM_BASE_URL.to_owned(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Reads an audio file (enforcing `<= 24 MiB`) and transcribes it using the exact
    /// EN/DE fail-closed policy, single `NoSpeech` `audio/ogg` `language=multi` fallback,
    /// and single `401`/`403` key-refresh callback retry.
    pub async fn transcribe_file<F>(
        &self,
        path: &Path,
        content_type: &str,
        language: DictationLanguage,
        initial_api_key: &str,
        refresh_key: F,
    ) -> Result<TranscriptionResult, TranscriptError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let meta = std::fs::metadata(path).map_err(|_| TranscriptError::LocalFileUnavailable)?;
        if meta.len() == 0 || meta.len() as usize > MAX_TRANSCRIBE_AUDIO_BYTES {
            return Err(TranscriptError::LocalFileUnavailable);
        }
        let body = std::fs::read(path).map_err(|_| TranscriptError::LocalFileUnavailable)?;
        self.transcribe_bytes(&body, content_type, language, initial_api_key, refresh_key)
            .await
    }

    pub async fn transcribe_bytes<F>(
        &self,
        body: &[u8],
        content_type: &str,
        language: DictationLanguage,
        initial_api_key: &str,
        refresh_key: F,
    ) -> Result<TranscriptionResult, TranscriptError>
    where
        F: Fn(&str) -> Option<String>,
    {
        if body.is_empty() || body.len() > MAX_TRANSCRIBE_AUDIO_BYTES {
            return Err(TranscriptError::LocalFileUnavailable);
        }
        if content_type.eq_ignore_ascii_case("audio/wav") && !crate::dictation::wav_has_speech(body)
        {
            return Err(TranscriptError::NoSpeech);
        }
        let mut active_key = initial_api_key.trim().to_owned();
        if active_key.is_empty() {
            return Err(TranscriptError::MissingKey);
        }
        let mut refreshed_once = false;

        let primary = self
            .transcribe_supported(
                body,
                content_type,
                language,
                &mut active_key,
                &mut refreshed_once,
                &refresh_key,
            )
            .await;

        match primary {
            Ok(res) => Ok(res),
            Err(TranscriptError::NoSpeech)
                if language == DictationLanguage::Automatic
                    && content_type.eq_ignore_ascii_case("audio/ogg") =>
            {
                let resp = self
                    .send_at(
                        DictationLanguage::deepgram_multilingual_path(),
                        content_type,
                        body,
                        &mut active_key,
                        &mut refreshed_once,
                        &refresh_key,
                    )
                    .await?;
                Ok(TranscriptionResult {
                    text: resp.text,
                    language: resp.detected_raw.unwrap_or_else(|| "multi".to_owned()),
                    confidence: resp.confidence,
                })
            }
            Err(err) => Err(err),
        }
    }

    async fn transcribe_supported<F>(
        &self,
        body: &[u8],
        content_type: &str,
        language: DictationLanguage,
        active_key: &mut String,
        refreshed_once: &mut bool,
        refresh_key: &F,
    ) -> Result<TranscriptionResult, TranscriptError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let first = self
            .send_at(
                language.deepgram_listen_path(),
                content_type,
                body,
                active_key,
                refreshed_once,
                refresh_key,
            )
            .await?;

        if language != DictationLanguage::Automatic || first.detected_en_de.is_some() {
            let lang_code = match (language, first.detected_en_de) {
                (DictationLanguage::English, _) | (_, Some(DictationLanguage::English)) => "en",
                (DictationLanguage::German, _) | (_, Some(DictationLanguage::German)) => "de",
                _ => "en",
            };
            return Ok(TranscriptionResult {
                text: first.text,
                language: lang_code.to_owned(),
                confidence: first.confidence,
            });
        }

        // Fail-closed Automatic fallback: discard unsupported detected language,
        // retry English and German, and pick higher confidence (tie -> English).
        let en = match self
            .send_at(
                DictationLanguage::English.deepgram_listen_path(),
                content_type,
                body,
                active_key,
                refreshed_once,
                refresh_key,
            )
            .await
        {
            Ok(v) => Some(v),
            Err(TranscriptError::NoSpeech) => None,
            Err(err) => return Err(err),
        };

        let de = match self
            .send_at(
                DictationLanguage::German.deepgram_listen_path(),
                content_type,
                body,
                active_key,
                refreshed_once,
                refresh_key,
            )
            .await
        {
            Ok(v) => Some(v),
            Err(TranscriptError::NoSpeech) => None,
            Err(err) => return Err(err),
        };

        match (en, de) {
            (Some(en_res), Some(de_res)) => {
                if de_res.confidence > en_res.confidence {
                    Ok(TranscriptionResult {
                        text: de_res.text,
                        language: "de".to_owned(),
                        confidence: de_res.confidence,
                    })
                } else {
                    Ok(TranscriptionResult {
                        text: en_res.text,
                        language: "en".to_owned(),
                        confidence: en_res.confidence,
                    })
                }
            }
            (Some(en_res), None) => Ok(TranscriptionResult {
                text: en_res.text,
                language: "en".to_owned(),
                confidence: en_res.confidence,
            }),
            (None, Some(de_res)) => Ok(TranscriptionResult {
                text: de_res.text,
                language: "de".to_owned(),
                confidence: de_res.confidence,
            }),
            (None, None) => Err(TranscriptError::NoSpeech),
        }
    }

    async fn send_at<F>(
        &self,
        path: &str,
        content_type: &str,
        body: &[u8],
        active_key: &mut String,
        refreshed_once: &mut bool,
        refresh_key: &F,
    ) -> Result<ParsedDeepgramResponse, TranscriptError>
    where
        F: Fn(&str) -> Option<String>,
    {
        match self.send_once(path, content_type, body, active_key).await {
            Ok(v) => Ok(v),
            Err(TranscriptError::Unauthorized) if !*refreshed_once => {
                *refreshed_once = true;
                if let Some(new_key) = refresh_key(active_key) {
                    let trimmed = new_key.trim();
                    if !trimmed.is_empty() && trimmed != active_key.as_str() {
                        *active_key = trimmed.to_owned();
                        return self.send_once(path, content_type, body, active_key).await;
                    }
                }
                Err(TranscriptError::Unauthorized)
            }
            Err(err) => Err(err),
        }
    }

    async fn send_once(
        &self,
        path: &str,
        content_type: &str,
        body: &[u8],
        api_key: &str,
    ) -> Result<ParsedDeepgramResponse, TranscriptError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("Token {api_key}"))
            .header("content-type", content_type)
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| TranscriptError::NetworkFailed)?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(map_http_status(status));
        }

        let payload = resp
            .text()
            .await
            .map_err(|_| TranscriptError::NetworkFailed)?;
        parse_deepgram_response(&payload)
    }
}

pub fn map_http_status(status: u16) -> TranscriptError {
    match status {
        400 => TranscriptError::BadAudio,
        401 | 403 => TranscriptError::Unauthorized,
        402 => TranscriptError::PaymentRequired,
        429 => TranscriptError::RateLimited,
        500..=599 => TranscriptError::TranscriptionUnavailable,
        _ => TranscriptError::ApiFailed,
    }
}

fn parse_deepgram_response(body: &str) -> Result<ParsedDeepgramResponse, TranscriptError> {
    let parsed: Value = serde_json::from_str(body).map_err(|_| TranscriptError::ApiFailed)?;
    let channels = parsed
        .get("results")
        .and_then(|r| r.get("channels"))
        .and_then(Value::as_array)
        .ok_or(TranscriptError::ApiFailed)?;
    let Some(channel0) = channels.first() else {
        return Err(TranscriptError::NoSpeech);
    };
    let alternatives = channel0
        .get("alternatives")
        .and_then(Value::as_array)
        .ok_or(TranscriptError::ApiFailed)?;
    let Some(alt0) = alternatives.first() else {
        return Err(TranscriptError::NoSpeech);
    };
    let transcript = alt0
        .get("transcript")
        .and_then(Value::as_str)
        .ok_or(TranscriptError::ApiFailed)?;
    if transcript.trim().is_empty() {
        return Err(TranscriptError::NoSpeech);
    }

    let detected_raw = channel0
        .get("detected_language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let detected_en_de = detected_raw.as_deref().and_then(parse_en_de_primary_subtag);
    let confidence = alt0
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    Ok(ParsedDeepgramResponse {
        text: transcript.to_owned(),
        detected_en_de,
        detected_raw,
        confidence,
    })
}

fn parse_en_de_primary_subtag(tag: &str) -> Option<DictationLanguage> {
    let primary = tag.split('-').next().unwrap_or(tag);
    if primary.eq_ignore_ascii_case("en") {
        Some(DictationLanguage::English)
    } else if primary.eq_ignore_ascii_case("de") {
        Some(DictationLanguage::German)
    } else {
        None
    }
}

/// Returns `<media_path>.txt`.
pub fn transcript_txt_path(media_path: &Path) -> PathBuf {
    let mut os = media_path.as_os_str().to_os_string();
    os.push(".txt");
    PathBuf::from(os)
}

/// Returns `<media_path>.raw.txt`.
pub fn transcript_raw_path(media_path: &Path) -> PathBuf {
    let mut os = media_path.as_os_str().to_os_string();
    os.push(".raw.txt");
    PathBuf::from(os)
}

/// Writes `<media_path>.txt` and atomically writes `<media_path>.raw.txt`.
/// Never overwrites an existing `.raw.txt` once created.
pub fn write_transcript_sidecars(media_path: &Path, raw_text: &str) -> std::io::Result<()> {
    let raw_path = transcript_raw_path(media_path);
    if !raw_path.exists() {
        atomic_write_text(&raw_path, raw_text)?;
    }
    let txt_path = transcript_txt_path(media_path);
    atomic_write_text(&txt_path, raw_text)?;
    Ok(())
}

/// Updates ONLY `<media_path>.txt` with optional formatted text while leaving `.raw.txt` untouched.
pub fn write_formatted_txt_sidecar(media_path: &Path, formatted_text: &str) -> std::io::Result<()> {
    let txt_path = transcript_txt_path(media_path);
    atomic_write_text(&txt_path, formatted_text)
}

fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = ATOMIC_RAW_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(format!(".{seq:x}.tmp"));
    let tmp = PathBuf::from(tmp_os);
    std::fs::write(&tmp, text.as_bytes())?;
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}

fn read_valid_raw_sidecar(path: &Path, reject_formatted: bool) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 || meta.len() as usize > MAX_SIDECAR_TEXT_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if reject_formatted && looks_formatted(trimmed) {
        return None;
    }
    // Preserve original raw whitespace/newlines verbatim.
    Some(text)
}

/// Loads verified raw transcript from disk sidecars:
/// 1. Prefers `<media_path>.raw.txt` (trusted as raw even if it starts with `"1. "`).
/// 2. Falls back to `<media_path>.txt`, REJECTING it if `looks_formatted(trimmed)` is true!
///
/// Preserves original raw whitespace verbatim and rejects NUL / invalid UTF-8 / >64 KiB.
pub fn load_verified_raw_transcript(media_path: &Path) -> Option<String> {
    if let Some(raw) = read_valid_raw_sidecar(&transcript_raw_path(media_path), false) {
        return Some(raw);
    }
    read_valid_raw_sidecar(&transcript_txt_path(media_path), true)
}

/// Resolves cached transcript from SQLite (by `media_sha256`) or local sidecars WITHOUT
/// ever making a network call. Returns `(raw_text, formatted_text, language)`.
pub fn load_cached_transcript(
    store: &Store,
    media_sha256: Option<&str>,
    media_path: Option<&Path>,
) -> Option<(String, Option<String>, String)> {
    if let Some(sha) = media_sha256.filter(|s| !s.is_empty())
        && let Ok(Some(rec)) = store.get_transcript_record(sha)
        && !rec.raw_text.trim().is_empty()
    {
        return Some((rec.raw_text, rec.formatted_text, rec.language));
    }
    if let Some(path) = media_path {
        if let Some(raw) = load_verified_raw_transcript(path) {
            return Some((raw, None, "auto".to_owned()));
        }
        // If only a formatted `.txt` exists (no `.raw.txt`), still show it in the UI without
        // re-requesting Deepgram (while raw copy guard continues to refuse exporting it as raw!).
        if let Some(txt) = read_valid_raw_sidecar(&transcript_txt_path(path), false) {
            return Some((String::new(), Some(txt), "auto".to_owned()));
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct MarkedVoiceExportInput<'a> {
    pub ts: &'a str,
    pub original_index: usize,
    pub display_label: &'a str,
    pub raw_from_store: Option<&'a str>,
    pub local_path: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkedTranscriptExport {
    pub clipboard_text: Option<String>,
    pub copied_count: usize,
    pub missing_labels: Vec<String>,
    pub notice: String,
}

/// Exports verified raw transcripts for marked voice messages:
/// - Sorts oldest-first by `(ts, original_index)`
/// - Uses verified raw text ONLY (SQLite raw or `.raw.txt` or non-formatted `.txt`, never formatted summary)
/// - Joins with `"\r\n\r\n"`
/// - Never overwrites clipboard when `copied_count == 0`
/// - Formats exact notice (`"Copied N transcripts"` or `"Copied N transcripts. Missing: ..."`).
pub fn export_marked_raw_transcripts(
    items: &[MarkedVoiceExportInput<'_>],
) -> MarkedTranscriptExport {
    let mut sorted: Vec<&MarkedVoiceExportInput<'_>> = items.iter().collect();
    sorted.sort_by(|a, b| {
        a.ts.cmp(b.ts)
            .then_with(|| a.original_index.cmp(&b.original_index))
    });

    let mut raw_parts: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for item in sorted {
        let verified = item
            .raw_from_store
            .filter(|s| !s.trim().is_empty() && !s.as_bytes().contains(&0))
            .map(str::to_owned)
            .or_else(|| item.local_path.and_then(load_verified_raw_transcript));

        if let Some(raw) = verified {
            raw_parts.push(raw);
        } else {
            missing.push(item.display_label.to_owned());
        }
    }

    let copied_count = raw_parts.len();
    let clipboard_text = if copied_count > 0 {
        Some(raw_parts.join("\r\n\r\n"))
    } else {
        None
    };

    let notice = if missing.is_empty() {
        format!("Copied {copied_count} transcripts")
    } else {
        format!(
            "Copied {copied_count} transcripts. Missing: {}",
            missing.join(", ")
        )
    };

    MarkedTranscriptExport {
        clipboard_text,
        copied_count,
        missing_labels: missing,
        notice,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        path: String,
        auth: String,
        content_type: String,
    }

    async fn spawn_deepgram_mock(
        responses: Vec<(u16, String)>,
    ) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec_clone = Arc::clone(&recorded);
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 16_384];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let mut path = String::new();
                let mut auth = String::new();
                let mut content_type = String::new();
                for (i, line) in head.lines().enumerate() {
                    if i == 0 {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            path = parts[1].to_owned();
                        }
                    } else if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("authorization") {
                            auth = v.trim().to_owned();
                        } else if k.eq_ignore_ascii_case("content-type") {
                            content_type = v.trim().to_owned();
                        }
                    }
                }
                rec_clone.lock().unwrap().push(RecordedRequest {
                    path,
                    auth,
                    content_type,
                });
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), recorded)
    }

    fn dg_json(transcript: &str, detected: Option<&str>, confidence: f64) -> String {
        let det_field = match detected {
            Some(d) => format!(r#","detected_language":"{d}""#),
            None => String::new(),
        };
        format!(
            r#"{{"results":{{"channels":[{{"alternatives":[{{"transcript":"{transcript}","confidence":{confidence}}}]{det_field}}}]}}}}"#
        )
    }

    #[tokio::test]
    async fn deepgram_auto_accepts_en_and_de_in_one_request() {
        let (base, reqs) =
            spawn_deepgram_mock(vec![(200, dg_json("Hallo Welt", Some("de-DE"), 0.98))]).await;
        let client = DeepgramRestClient::new().with_base_url(base);
        let res = client
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "key-1",
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(res.text, "Hallo Welt");
        assert_eq!(res.language, "de");
        let calls = reqs.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .path
                .contains("detect_language=en&detect_language=de&tag=postbox")
        );
        assert_eq!(calls[0].auth, "Token key-1");
        assert_eq!(calls[0].content_type, "audio/ogg");
    }

    #[tokio::test]
    async fn deepgram_auto_retries_en_and_de_on_non_en_de_and_ties_go_to_english() {
        // First returns Turkish ("tr") -> discarded; EN=0.80, DE=0.80 -> tie goes to English!
        let (base, reqs) = spawn_deepgram_mock(vec![
            (200, dg_json("Merhaba", Some("tr"), 0.99)),
            (200, dg_json("English tie winner", Some("en"), 0.80)),
            (200, dg_json("Deutscher Text", Some("de"), 0.80)),
        ])
        .await;
        let client = DeepgramRestClient::new().with_base_url(base);
        let res = client
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "key-1",
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(res.text, "English tie winner");
        assert_eq!(res.language, "en");
        assert_eq!(reqs.lock().unwrap().len(), 3);

        // Now DE strictly higher confidence -> picks German.
        let (base2, _) = spawn_deepgram_mock(vec![
            (200, dg_json("Bonjour", Some("fr"), 0.95)),
            (200, dg_json("English lower", Some("en"), 0.60)),
            (200, dg_json("Deutsch gewinnt", Some("de"), 0.92)),
        ])
        .await;
        let client2 = DeepgramRestClient::new().with_base_url(base2);
        let res2 = client2
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "key-1",
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(res2.text, "Deutsch gewinnt");
        assert_eq!(res2.language, "de");
    }

    #[tokio::test]
    async fn deepgram_nospeech_audio_ogg_falls_back_once_to_language_multi() {
        let (base, reqs) = spawn_deepgram_mock(vec![
            (200, dg_json("", None, 0.0)),
            (200, dg_json("Recovered multi speech", Some("en"), 0.85)),
        ])
        .await;
        let client = DeepgramRestClient::new().with_base_url(base);
        let res = client
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "key-1",
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(res.text, "Recovered multi speech");
        let calls = reqs.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].path.contains("language=multi&tag=postbox"));
    }

    #[tokio::test]
    async fn deepgram_401_refreshes_key_once_only_if_changed() {
        let (base, reqs) = spawn_deepgram_mock(vec![
            (401, r#"{"err":"unauthorized"}"#.into()),
            (200, dg_json("Worked with refreshed key", Some("en"), 0.95)),
        ])
        .await;
        let client = DeepgramRestClient::new().with_base_url(base);
        let res = client
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "old-key",
                |curr| {
                    assert_eq!(curr, "old-key");
                    Some("new-key".into())
                },
            )
            .await
            .unwrap();
        assert_eq!(res.text, "Worked with refreshed key");
        {
            let calls = reqs.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].auth, "Token old-key");
            assert_eq!(calls[1].auth, "Token new-key");
        }

        // If refreshed key is unchanged, do NOT retry.
        let (base2, reqs2) = spawn_deepgram_mock(vec![(401, r#"{"err":"unauth"}"#.into())]).await;
        let client2 = DeepgramRestClient::new().with_base_url(base2);
        let err = client2
            .transcribe_bytes(
                b"OggS-fake",
                "audio/ogg",
                DictationLanguage::Automatic,
                "same-key",
                |_| Some("same-key".into()),
            )
            .await
            .unwrap_err();
        assert_eq!(err, TranscriptError::Unauthorized);
        assert_eq!(reqs2.lock().unwrap().len(), 1);
    }

    #[test]
    fn raw_sidecars_and_marked_export_enforce_legacy_guard_and_oldest_first_order() {
        let dir = std::env::temp_dir().join(format!(
            "postbox-audio-transcript-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Note 1: dual sidecar where raw genuinely starts with "1. " (must be trusted from .raw.txt!)
        let p1 = dir.join("note1.ogg");
        std::fs::write(&p1, b"ogg1").unwrap();
        write_transcript_sidecars(&p1, "1. Genuine numbered raw text\n").unwrap();
        write_formatted_txt_sidecar(&p1, "1. Formatted summary").unwrap();
        assert_eq!(
            load_verified_raw_transcript(&p1).as_deref(),
            Some("1. Genuine numbered raw text\n")
        );

        // Note 2: legacy formatted .txt WITHOUT .raw.txt -> MUST BE REJECTED as raw!
        let p2 = dir.join("note2.ogg");
        std::fs::write(&p2, b"ogg2").unwrap();
        std::fs::write(transcript_txt_path(&p2), "1. Legacy formatted summary").unwrap();
        assert_eq!(load_verified_raw_transcript(&p2), None);

        // Note 3: plain unformatted .txt without .raw.txt -> accepted verbatim.
        let p3 = dir.join("note3.ogg");
        std::fs::write(&p3, b"ogg3").unwrap();
        std::fs::write(transcript_txt_path(&p3), "  Second older note  ").unwrap();

        let export = export_marked_raw_transcripts(&[
            MarkedVoiceExportInput {
                ts: "2026-11-27T10:05:00Z",
                original_index: 2,
                display_label: "10:05",
                raw_from_store: None,
                local_path: Some(&p1),
            },
            MarkedVoiceExportInput {
                ts: "2026-11-27T10:03:00Z",
                original_index: 1,
                display_label: "10:03",
                raw_from_store: None,
                local_path: Some(&p2),
            },
            MarkedVoiceExportInput {
                ts: "2026-11-27T10:01:00Z",
                original_index: 0,
                display_label: "10:01",
                raw_from_store: None,
                local_path: Some(&p3),
            },
        ]);

        assert_eq!(export.copied_count, 2);
        assert_eq!(
            export.clipboard_text.as_deref(),
            Some("  Second older note  \r\n\r\n1. Genuine numbered raw text\n")
        );
        assert_eq!(export.missing_labels, vec!["10:03".to_owned()]);
        assert_eq!(export.notice, "Copied 2 transcripts. Missing: 10:03");

        // Immutable cache check via SQLite store.
        let store = Store::open_in_memory().unwrap();
        store
            .cache_raw_transcript("sha-immutable", "en", "Cached once forever", 100)
            .unwrap();
        let cached = load_cached_transcript(&store, Some("sha-immutable"), Some(&p2)).unwrap();
        assert_eq!(cached.0, "Cached once forever");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
