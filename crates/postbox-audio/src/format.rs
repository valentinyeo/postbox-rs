//! Optional ADHD LLM transcript formatting, v2 collision-safe disk cache, refusal detection,
//! dominant EN/DE language detection, and 512-slot once-per-session claim ring.
//! Strictly dormant unless the user explicitly configures an Anthropic or OpenRouter API key.

use postbox_platform::FormatProviderConfig;
use reqwest::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

use crate::MAX_IDLE_CONNECTIONS_PER_HOST;

pub const FORMAT_CACHE_MAX_BYTES: usize = 64 * 1024;
pub const FORMAT_CACHE_HEADER_V2: &str = "wazig-voice-format v2\n";
pub const FORMAT_REQUEST_RING_CAP: usize = 512;
pub const FORMAT_HTTP_TIMEOUT: Duration = Duration::from_secs(180);
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai";

const GERMAN_WORDS: &[&str] = &[
    "ich", "du", "wir", "der", "die", "das", "und", "ist", "im", "mit", "für", "morgen", "bitte",
    "nicht", "heute", "weil", "am", "um", "den", "dem", "zum", "dass", "aber", "noch", "uns",
    "ein", "eine", "auf", "haben", "müssen",
];

const ENGLISH_WORDS: &[&str] = &[
    "i", "you", "we", "the", "and", "is", "in", "with", "for", "tomorrow", "please", "not",
    "today", "because", "at", "to", "that", "but", "still", "us", "a", "an", "on", "have", "must",
    "will", "would",
];

const REFUSAL_OPENERS: &[&str] = &[
    "i cannot",
    "i can't",
    "i can not",
    "i'm unable",
    "i am unable",
    "i'm sorry",
    "i am sorry",
    "sorry",
    "i apologize",
    "unfortunately",
    "as an ai",
    "i won't",
    "i will not",
    "i'm not able",
    "i am not able",
    "entschuldigung",
    "leider",
    "ich kann nicht",
    "ich kann dieses",
    "ich kann diese",
    "there is no",
    "there's no",
    "this transcript",
    "the transcript",
    "it looks like",
    "it seems",
    "could you",
    "can you",
    "please provide",
    "please share",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DominantLanguage {
    German,
    English,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("LLM refused to format transcript")]
    Refused,
    #[error("bad formatting response")]
    BadResponse,
    #[error("formatting HTTP error: {0}")]
    HttpStatus(u16),
    #[error("formatting network error: {0}")]
    Network(String),
    #[error("invalid cache envelope or key")]
    InvalidCache,
}

/// Returns true if `text` already looks like an ADHD-formatted summary (`1. ...`, `1) ...`, or `Gist...`).
pub fn looks_formatted(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'1' && (bytes[1] == b'.' || bytes[1] == b')') {
        return true;
    }
    if bytes.len() >= 4 && bytes[..4].eq_ignore_ascii_case(b"gist") {
        return true;
    }
    false
}

/// Detects LLM refusals and non-numbered malformed outputs (matching `format_request.zig:131-156`).
pub fn is_refusal(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\r', '\n', '\t']);
    if trimmed.is_empty() {
        return true;
    }
    let head_raw: String = trimmed.chars().take(96).collect();
    let head = head_raw.to_lowercase();
    let stripped =
        head.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ' ' | '-'));
    for opener in REFUSAL_OPENERS {
        if stripped.starts_with(opener) {
            return true;
        }
    }
    let head_bytes = head.as_bytes();
    !(head_bytes.len() >= 2
        && head_bytes[0] == b'1'
        && (head_bytes[1] == b'.' || head_bytes[1] == b')'))
}

/// Word-count dominant language hint (`30` German vs `27` English tokens; strictly higher score wins).
pub fn dominant_language(text: &str) -> Option<DominantLanguage> {
    let mut de_score = 0usize;
    let mut en_score = 0usize;
    for raw_tok in text.split(|c: char| {
        matches!(
            c,
            ' ' | '\t' | '\r' | '\n' | '.' | ',' | '!' | '?' | ';' | ':' | '"' | '(' | ')'
        )
    }) {
        if raw_tok.is_empty() {
            continue;
        }
        let tok = raw_tok.to_lowercase();
        if GERMAN_WORDS.contains(&tok.as_str()) {
            de_score += 1;
        }
        if ENGLISH_WORDS.contains(&tok.as_str()) {
            en_score += 1;
        }
    }
    if de_score > en_score {
        Some(DominantLanguage::German)
    } else if en_score > de_score {
        Some(DominantLanguage::English)
    } else {
        None
    }
}

/// Builds speaker perspective instructions for ADHD transcript formatting.
pub fn speaker_instruction(from_me: bool, contact_name: &str, owner_name: &str) -> String {
    let contact = contact_name.trim();
    let other = if contact.is_empty() {
        "the other person"
    } else {
        contact
    };
    let owner = owner_name.trim();
    let reader = if owner.is_empty() {
        "the reader"
    } else {
        owner
    };
    if from_me {
        format!(
            "Speaker perspective: {reader} recorded this voice note to {other}. \
             First-person pronouns (\"I\"/\"ich\") refer to {reader}; \"you\"/\"du\" refers to {other}."
        )
    } else {
        format!(
            "Speaker perspective: {other} recorded this voice note to {reader}. \
             First-person pronouns (\"I\"/\"ich\") refer to {other}; \"you\"/\"du\" refers to {reader}."
        )
    }
}

pub fn build_format_system_prompt(raw_text: &str, from_me: bool, contact_name: &str) -> String {
    let mut prompt = String::from(
        "Format the voice note transcript for quick ADHD-friendly scanning as a concise numbered list starting with \"1. \". \
         Preserve every concrete detail, name, time, number, and action item. Do not add preamble or disclaimers. ",
    );
    prompt.push_str(&speaker_instruction(from_me, contact_name, ""));
    match dominant_language(raw_text) {
        Some(DominantLanguage::German) => {
            prompt.push_str(
                " OUTPUT LANGUAGE: German. Write the entire formatted summary in German.",
            );
        }
        Some(DominantLanguage::English) => {
            prompt.push_str(
                " OUTPUT LANGUAGE: English. Write the entire formatted summary in English.",
            );
        }
        None => {}
    }
    prompt
}

/// Deterministic 64-bit hash used for format cache filenames and the 512-slot request ring.
fn stable_hash64(parts: &[&[u8]]) -> u64 {
    // FNV-1a 64-bit (deterministic across Rust versions and platforms).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatCacheKey {
    pub account: String,
    pub chat: String,
    pub message: String,
}

impl FormatCacheKey {
    pub fn new(
        account: impl Into<String>,
        chat: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            chat: chat.into(),
            message: message.into(),
        }
    }

    pub fn is_valid(&self) -> bool {
        let valid_part = |s: &str| !s.is_empty() && !s.contains('\r') && !s.contains('\n');
        valid_part(&self.account) && valid_part(&self.chat) && valid_part(&self.message)
    }

    pub fn filename(&self) -> String {
        let h = stable_hash64(&[
            self.account.as_bytes(),
            b"\n",
            self.chat.as_bytes(),
            b"\n",
            self.message.as_bytes(),
            b"\n",
        ]);
        format!("{h:016x}.txt")
    }
}

/// v2 collision-safe format cache envelope (`"wazig-voice-format v2\n"`, max `64 KiB`, rejects v1).
pub struct FormatCache;

impl FormatCache {
    pub fn build_envelope(
        key: &FormatCacheKey,
        formatted_text: &str,
    ) -> Result<Vec<u8>, FormatError> {
        if !key.is_valid() || !looks_formatted(formatted_text) {
            return Err(FormatError::InvalidCache);
        }
        let mut out = Vec::with_capacity(
            FORMAT_CACHE_HEADER_V2.len()
                + key.account.len()
                + key.chat.len()
                + key.message.len()
                + formatted_text.len()
                + 3,
        );
        out.extend_from_slice(FORMAT_CACHE_HEADER_V2.as_bytes());
        out.extend_from_slice(key.account.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(key.chat.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(key.message.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(formatted_text.as_bytes());
        if out.len() > FORMAT_CACHE_MAX_BYTES {
            return Err(FormatError::InvalidCache);
        }
        Ok(out)
    }

    pub fn parse_envelope<'a>(data: &'a [u8], key: &FormatCacheKey) -> Option<&'a str> {
        if !key.is_valid() || data.len() > FORMAT_CACHE_MAX_BYTES {
            return None;
        }
        let text = std::str::from_utf8(data).ok()?;
        let rest = text.strip_prefix(FORMAT_CACHE_HEADER_V2)?;
        let (acc, rest) = rest.split_once('\n')?;
        let (chat, rest) = rest.split_once('\n')?;
        let (msg, body) = rest.split_once('\n')?;
        if acc != key.account || chat != key.chat || msg != key.message {
            return None;
        }
        if !looks_formatted(body) {
            return None;
        }
        Some(body)
    }

    pub fn cache_path(root_dir: &Path, key: &FormatCacheKey) -> PathBuf {
        let safe_account: String = key
            .account
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        root_dir
            .join(format!("voice-format-{safe_account}"))
            .join(key.filename())
    }

    pub fn save_to_disk(
        root_dir: &Path,
        key: &FormatCacheKey,
        formatted_text: &str,
    ) -> Result<PathBuf, FormatError> {
        let envelope = Self::build_envelope(key, formatted_text)?;
        let path = Self::cache_path(root_dir, key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| FormatError::InvalidCache)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &envelope).map_err(|_| FormatError::InvalidCache)?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&path);
            std::fs::rename(&tmp, &path).map_err(|_| FormatError::InvalidCache)?;
        }
        Ok(path)
    }

    pub fn load_from_disk(root_dir: &Path, key: &FormatCacheKey) -> Option<String> {
        let path = Self::cache_path(root_dir, key);
        let bytes = std::fs::read(&path).ok()?;
        Self::parse_envelope(&bytes, key).map(str::to_owned)
    }
}

/// 512-slot ring guaranteeing at most 1 LLM formatting request per message per session.
#[derive(Debug, Default, Clone)]
pub struct FormatRequests {
    ring: Vec<u64>,
    cursor: usize,
}

impl FormatRequests {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn claim(&mut self, msg_id: &str, transcript: &str) -> bool {
        if msg_id.is_empty() || transcript.trim().is_empty() || looks_formatted(transcript.trim()) {
            return false;
        }
        let h = stable_hash64(&[msg_id.as_bytes()]);
        if self.ring.contains(&h) {
            return false;
        }
        if self.ring.len() < FORMAT_REQUEST_RING_CAP {
            self.ring.push(h);
        } else {
            self.ring[self.cursor] = h;
            self.cursor = (self.cursor + 1) % FORMAT_REQUEST_RING_CAP;
        }
        true
    }
}

/// Async optional ADHD transcript formatter supporting Anthropic and OpenRouter endpoints.
#[derive(Debug, Clone)]
pub struct TranscriptFormatter {
    client: Client,
    anthropic_base_url: String,
    openrouter_base_url: String,
}

impl Default for TranscriptFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptFormatter {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(FORMAT_HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_owned(),
            openrouter_base_url: DEFAULT_OPENROUTER_BASE_URL.to_owned(),
        }
    }

    pub fn with_base_urls(
        mut self,
        anthropic_base_url: impl Into<String>,
        openrouter_base_url: impl Into<String>,
    ) -> Self {
        self.anthropic_base_url = anthropic_base_url.into().trim_end_matches('/').to_owned();
        self.openrouter_base_url = openrouter_base_url.into().trim_end_matches('/').to_owned();
        self
    }

    pub async fn format_transcript(
        &self,
        provider: &FormatProviderConfig,
        raw_text: &str,
        from_me: bool,
        contact_name: &str,
    ) -> Result<String, FormatError> {
        let system_prompt = build_format_system_prompt(raw_text, from_me, contact_name);
        let user_content = format!("<transcript>\n{raw_text}\n</transcript>");

        let formatted = match provider {
            FormatProviderConfig::Anthropic {
                api_key,
                model,
                workspace_id,
            } => {
                let url = format!("{}/v1/messages", self.anthropic_base_url);
                let body = json!({
                    "model": model,
                    "max_tokens": 8192,
                    "thinking": { "type": "disabled" },
                    "system": system_prompt,
                    "messages": [{ "role": "user", "content": user_content }],
                });
                let mut req = self
                    .client
                    .post(&url)
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if let Some(ws) = workspace_id.as_deref().filter(|s| !s.is_empty()) {
                    req = req.header("anthropic-workspace-id", ws);
                }
                let resp = req
                    .body(body.to_string())
                    .send()
                    .await
                    .map_err(|e| FormatError::Network(e.to_string()))?;
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(FormatError::HttpStatus(status));
                }
                let text = resp
                    .text()
                    .await
                    .map_err(|e| FormatError::Network(e.to_string()))?;
                parse_anthropic_response(&text)?
            }
            FormatProviderConfig::OpenRouter { api_key, model } => {
                let url = format!("{}/api/v1/chat/completions", self.openrouter_base_url);
                let body = json!({
                    "model": model,
                    "reasoning": { "effort": "medium" },
                    "temperature": 0.2,
                    "messages": [
                        { "role": "system", "content": system_prompt },
                        { "role": "user", "content": user_content }
                    ],
                });
                let resp = self
                    .client
                    .post(&url)
                    .header("authorization", format!("Bearer {api_key}"))
                    .header("content-type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                    .map_err(|e| FormatError::Network(e.to_string()))?;
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(FormatError::HttpStatus(status));
                }
                let text = resp
                    .text()
                    .await
                    .map_err(|e| FormatError::Network(e.to_string()))?;
                parse_openrouter_response(&text)?
            }
        };

        if is_refusal(&formatted) {
            return Err(FormatError::Refused);
        }
        Ok(formatted)
    }
}

fn parse_anthropic_response(body: &str) -> Result<String, FormatError> {
    let parsed: Value = serde_json::from_str(body).map_err(|_| FormatError::BadResponse)?;
    let content = parsed
        .get("content")
        .and_then(Value::as_array)
        .ok_or(FormatError::BadResponse)?;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = item.get("text").and_then(Value::as_str)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
    }
    Err(FormatError::BadResponse)
}

fn parse_openrouter_response(body: &str) -> Result<String, FormatError> {
    let parsed: Value = serde_json::from_str(body).map_err(|_| FormatError::BadResponse)?;
    let text = parsed
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(FormatError::BadResponse)?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn looks_formatted_and_refusal_detection_cover_all_cases() {
        assert!(looks_formatted("1. Meeting at 3pm"));
        assert!(looks_formatted("1) Call Alice"));
        assert!(looks_formatted("Gist: call tomorrow"));
        assert!(!looks_formatted("Hey can we meet tomorrow at 3?"));

        assert!(!is_refusal("1. Meet tomorrow at 3pm\n- Bring slides"));
        assert!(is_refusal("1. I cannot process this transcript."));
        assert!(is_refusal(
            "Entschuldigung, ich kann dieses Audio nicht verarbeiten."
        ));
        assert!(is_refusal("Sure, here is a summary without numbering."));
        assert!(is_refusal("   "));
    }

    #[test]
    fn dominant_language_and_format_cache_v2_work_correctly() {
        assert_eq!(
            dominant_language("Hallo, wir müssen morgen bitte noch das Dokument schicken."),
            Some(DominantLanguage::German)
        );
        assert_eq!(
            dominant_language("Please send the document tomorrow because we have a meeting."),
            Some(DominantLanguage::English)
        );
        assert_eq!(dominant_language("12345"), None);

        let key = FormatCacheKey::new("main", "123@s.whatsapp.net", "m1");
        let env = FormatCache::build_envelope(&key, "1. Summary line").unwrap();
        assert_eq!(
            FormatCache::parse_envelope(&env, &key),
            Some("1. Summary line")
        );

        // Collision / wrong key rejected.
        let other = FormatCacheKey::new("main", "999@s.whatsapp.net", "m1");
        assert_eq!(FormatCache::parse_envelope(&env, &other), None);

        // Legacy v1 rejected.
        let v1 = b"wazig-voice-format v1\nmain\n123@s.whatsapp.net\nm1\n1. Old";
        assert_eq!(FormatCache::parse_envelope(v1, &key), None);

        // FormatRequests claims once per session and skips already-formatted text.
        let mut reqs = FormatRequests::new();
        assert!(!reqs.claim("m1", "1. Already formatted"));
        assert!(reqs.claim("m1", "Raw transcript text"));
        assert!(!reqs.claim("m1", "Raw transcript text"));
    }

    async fn spawn_mock_json_server(status: u16, response_json: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn loopback_formatter_supports_anthropic_openrouter_and_rejects_refusals() {
        let anth_url = spawn_mock_json_server(
            200,
            r#"{"content":[{"type":"text","text":"1. Meet at 14:00\n- Bring prototype"}]}"#,
        )
        .await;
        let or_url = spawn_mock_json_server(
            200,
            r#"{"choices":[{"message":{"content":"1. Tomorrow call\n- Confirm venue"}}]}"#,
        )
        .await;
        let formatter = TranscriptFormatter::new().with_base_urls(&anth_url, &or_url);

        let anth_cfg = FormatProviderConfig::Anthropic {
            api_key: "sk-ant-test".into(),
            model: "claude-haiku-5-5".into(),
            workspace_id: Some("ws-1".into()),
        };
        let out_a = formatter
            .format_transcript(
                &anth_cfg,
                "Let us meet at 14:00 and bring the prototype",
                false,
                "Ada",
            )
            .await
            .unwrap();
        assert!(out_a.starts_with("1. Meet at 14:00"));

        let or_cfg = FormatProviderConfig::OpenRouter {
            api_key: "or-test".into(),
            model: "openai/gpt-5.6-luna".into(),
        };
        let out_o = formatter
            .format_transcript(
                &or_cfg,
                "We have a call tomorrow to confirm the venue",
                true,
                "Ada",
            )
            .await
            .unwrap();
        assert!(out_o.starts_with("1. Tomorrow call"));

        // Refusal from LLM returns FormatError::Refused and never overwrites raw text.
        let refuse_url = spawn_mock_json_server(
            200,
            r#"{"content":[{"type":"text","text":"I'm sorry, I cannot format this transcript."}]}"#,
        )
        .await;
        let refusing = TranscriptFormatter::new().with_base_urls(&refuse_url, &or_url);
        assert_eq!(
            refusing
                .format_transcript(&anth_cfg, "Raw text", false, "Ada")
                .await,
            Err(FormatError::Refused)
        );
    }

    #[test]
    fn build_format_system_prompt_uses_neutral_reader_perspective() {
        let prompt = build_format_system_prompt("Let us meet tomorrow at 14:00", false, "Ada");
        assert!(prompt.contains("Ada recorded this voice note to the reader."));
        assert!(prompt.contains("OUTPUT LANGUAGE: English."));
    }
}
