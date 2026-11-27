use crate::{ChatKey, Provider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const CLIENT_ID_LEN: usize = 32;
pub const MAX_OUTBOX_ENTRIES: usize = 64;
pub const MAX_PENDING_SENDS: usize = 32;
pub const MAX_ATTEMPTS: u8 = 4;
pub const MAX_TEXT_LEN: usize = 4095;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_FAILURE_REASON_LEN: usize = 95;
pub const MAX_ACCOUNT_LEN: usize = 32;
pub const MAX_JID_LEN: usize = 191;
pub const MAX_FILE_PATH_LEN: usize = 519;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OutboxError {
    #[error("invalid_client_id")]
    InvalidClientId,
    #[error("invalid_identity")]
    InvalidIdentity,
    #[error("contains_nul")]
    ContainsNul,
    #[error("text_too_long")]
    TextTooLong,
    #[error("empty_payload")]
    EmptyPayload,
    #[error("file_too_large")]
    FileTooLarge,
    #[error("invalid_filename")]
    InvalidFilename,
    #[error("invalid_broadcast")]
    InvalidBroadcast,
    #[error("invalid_phase")]
    InvalidPhase,
    #[error("invalid_attachment_state")]
    InvalidAttachmentState,
    #[error("entropy_unavailable")]
    EntropyUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxPhase {
    Ready,
    Uncertain,
    Held,
}

impl OutboxPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Uncertain => "uncertain",
            Self::Held => "held",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "ready" => Self::Ready,
            "uncertain" => Self::Uncertain,
            _ => Self::Held,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxKind {
    Text,
    File,
    Voice,
    React,
    Delete,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::File => "file",
            Self::Voice => "voice",
            Self::React => "react",
            Self::Delete => "delete",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "file" => Self::File,
            "voice" => Self::Voice,
            "react" => Self::React,
            "delete" => Self::Delete,
            _ => Self::Text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Acknowledged,
    Retryable,
    Held,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedOutcome {
    pub outcome: SendOutcome,
    pub ambiguous: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub client_id: String,
    pub chat: ChatKey,
    pub kind: OutboxKind,
    pub phase: OutboxPhase,
    pub attempt_serial: u64,
    pub attempts: u8,
    pub ambiguous: bool,
    pub text_body: String,
    pub reply_to_id: Option<String>,
    pub reply_broadcast: bool,
    pub staged_file_path: Option<String>,
    pub wire_filename: String,
    pub file_sha256: Option<[u8; 32]>,
    pub file_bytes: u64,
    pub ptt: bool,
    pub voice_seconds: u32,
    pub attachment_ready: bool,
    pub failure_reason: Option<String>,
    pub not_before_ms: i64,
    pub created_at_ms: i64,
}

impl OutboxRecord {
    pub fn new_text(
        client_id: impl Into<String>,
        chat: ChatKey,
        text_body: impl Into<String>,
        reply_to_id: Option<String>,
        reply_broadcast: bool,
        created_at_ms: i64,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            chat,
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: text_body.into(),
            reply_to_id,
            reply_broadcast,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: created_at_ms,
            created_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_file(
        client_id: impl Into<String>,
        chat: ChatKey,
        caption: impl Into<String>,
        reply_to_id: Option<String>,
        staged_file_path: impl Into<String>,
        wire_filename: impl Into<String>,
        file_sha256: [u8; 32],
        file_bytes: u64,
        created_at_ms: i64,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            chat,
            kind: OutboxKind::File,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: caption.into(),
            reply_to_id,
            reply_broadcast: false,
            staged_file_path: Some(staged_file_path.into()),
            wire_filename: wire_filename.into(),
            file_sha256: Some(file_sha256),
            file_bytes,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: true,
            failure_reason: None,
            not_before_ms: created_at_ms,
            created_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_voice(
        client_id: impl Into<String>,
        chat: ChatKey,
        reply_to_id: Option<String>,
        staged_file_path: impl Into<String>,
        file_sha256: [u8; 32],
        file_bytes: u64,
        voice_seconds: u32,
        created_at_ms: i64,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            chat,
            kind: OutboxKind::Voice,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: String::new(),
            reply_to_id,
            reply_broadcast: false,
            staged_file_path: Some(staged_file_path.into()),
            wire_filename: "voice.ogg".to_owned(),
            file_sha256: Some(file_sha256),
            file_bytes,
            ptt: true,
            voice_seconds,
            attachment_ready: true,
            failure_reason: None,
            not_before_ms: created_at_ms,
            created_at_ms,
        }
    }
}

/// Validates a 32-character lowercase hexadecimal `client_id` (`outbox.zig:57-68`).
pub fn is_valid_client_id(id: &str) -> bool {
    id.len() == CLIENT_ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Formats 16 random bytes as 32 lowercase hex characters (`outbox.zig:70-72`).
pub fn format_client_id(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(CLIENT_ID_LEN);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// Generates a cryptographically random 32-hex `client_id`.
pub fn generate_client_id() -> Result<String, OutboxError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| OutboxError::EntropyUnavailable)?;
    Ok(format_client_id(bytes))
}

/// Retry schedule (`main.zig:107-114`): 3s, 10s, 30s, then give up on attempt 4 (`AutoSendsBounded`).
pub fn retry_delay_ms(attempts_after_failure: u8) -> Option<u64> {
    match attempts_after_failure {
        1 => Some(3_000),
        2 => Some(10_000),
        3 => Some(30_000),
        _ => None,
    }
}

/// Exact port of `main.zig:24847`: delivery outcome is ambiguous if failure is not a local
/// attachment error and HTTP status is 0 (network/timeout), 409, >= 500, or unconfirmed 2xx.
pub fn is_ambiguous_http_failure(ok: bool, http_status: u16, attachment_error: bool) -> bool {
    !ok && !attachment_error
        && (http_status == 0
            || http_status == 409
            || http_status >= 500
            || (200..300).contains(&http_status))
}

pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Exact port of `main.zig:24840-24851`.
pub fn classify_http_outcome(
    provider: Provider,
    ok: bool,
    http_status: u16,
    attachment_error: bool,
    detail: &str,
) -> ClassifiedOutcome {
    let terminal = attachment_error
        || matches!(http_status, 401 | 403 | 409)
        || (http_status == 429 && matches!(provider, Provider::Signal | Provider::Telegram));
    let outcome = if ok {
        SendOutcome::Acknowledged
    } else if terminal {
        SendOutcome::Held
    } else {
        SendOutcome::Retryable
    };
    let ambiguous = is_ambiguous_http_failure(ok, http_status, attachment_error);
    let reason = if ok {
        None
    } else if attachment_error {
        Some("attachment unavailable".to_owned())
    } else if !detail.is_empty() {
        Some(truncate_utf8(detail, MAX_FAILURE_REASON_LEN))
    } else {
        Some("send failed".to_owned())
    };
    ClassifiedOutcome {
        outcome,
        ambiguous,
        reason,
    }
}

pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Pre-send attachment integrity guard (`unified_snapshot.zig:544-549`).
pub fn verify_attachment_bytes(
    bytes: &[u8],
    expected_len: u64,
    expected_sha256: &[u8; 32],
) -> bool {
    bytes.len() as u64 == expected_len && sha256_bytes(bytes) == *expected_sha256
}

fn is_safe_wire_filename(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if name == "." || name == ".." {
        return false;
    }
    !name
        .bytes()
        .any(|b| b < 0x20 || matches!(b, b'\\' | b'/' | b':' | b'\r' | b'\n'))
}

/// Validates an `OutboxRecord` against all `unified_snapshot.zig:77-102` and `SendQueue.tla` invariants.
pub fn validate_record(rec: &OutboxRecord) -> Result<(), OutboxError> {
    if !is_valid_client_id(&rec.client_id) {
        return Err(OutboxError::InvalidClientId);
    }
    if rec.chat.account.is_empty()
        || rec.chat.account.len() > MAX_ACCOUNT_LEN
        || rec.chat.id.is_empty()
        || rec.chat.id.len() > MAX_JID_LEN
    {
        return Err(OutboxError::InvalidIdentity);
    }
    if rec.chat.account.as_bytes().contains(&0)
        || rec.chat.id.as_bytes().contains(&0)
        || rec.text_body.as_bytes().contains(&0)
        || rec
            .reply_to_id
            .as_deref()
            .is_some_and(|s| s.as_bytes().contains(&0))
        || rec
            .staged_file_path
            .as_deref()
            .is_some_and(|s| s.as_bytes().contains(&0))
        || rec
            .failure_reason
            .as_deref()
            .is_some_and(|s| s.as_bytes().contains(&0))
    {
        return Err(OutboxError::ContainsNul);
    }
    if rec.text_body.len() > MAX_TEXT_LEN {
        return Err(OutboxError::TextTooLong);
    }
    if rec.file_bytes > MAX_FILE_BYTES {
        return Err(OutboxError::FileTooLarge);
    }
    if !is_safe_wire_filename(&rec.wire_filename) {
        return Err(OutboxError::InvalidFilename);
    }
    if rec.reply_broadcast
        && (rec.reply_to_id.as_deref().is_none_or(str::is_empty) || rec.staged_file_path.is_some())
    {
        return Err(OutboxError::InvalidBroadcast);
    }
    if rec.phase == OutboxPhase::Uncertain && rec.attempt_serial == 0 {
        return Err(OutboxError::InvalidPhase);
    }
    if rec.staged_file_path.is_none() {
        if rec.attachment_ready
            || rec.file_bytes != 0
            || rec.file_sha256.is_some()
            || rec.ptt
            || rec.voice_seconds != 0
        {
            return Err(OutboxError::InvalidAttachmentState);
        }
        if matches!(rec.kind, OutboxKind::Text) && rec.text_body.is_empty() {
            return Err(OutboxError::EmptyPayload);
        }
        if matches!(rec.kind, OutboxKind::File | OutboxKind::Voice) {
            return Err(OutboxError::EmptyPayload);
        }
    } else {
        if rec
            .staged_file_path
            .as_deref()
            .is_some_and(|p| p.is_empty() || p.len() > MAX_FILE_PATH_LEN)
        {
            return Err(OutboxError::InvalidAttachmentState);
        }
        if !rec.attachment_ready && rec.phase != OutboxPhase::Held {
            return Err(OutboxError::InvalidAttachmentState);
        }
    }
    Ok(())
}

/// Applies a send outcome to an in-flight `OutboxRecord` (`main.zig:24780-24805`).
/// Returns `None` on `Acknowledged` (caller deletes from SQLite), or `Some(updated)` with
/// identical `client_id` preserved (`NoDouble`).
pub fn apply_outcome(
    rec: &OutboxRecord,
    outcome: SendOutcome,
    ambiguous: bool,
    reason: Option<&str>,
    now_ms: i64,
) -> Option<OutboxRecord> {
    if outcome == SendOutcome::Acknowledged {
        return None;
    }
    let mut updated = rec.clone();
    updated.attempts = updated.attempts.saturating_add(1);
    updated.ambiguous |= ambiguous;
    if let Some(r) = reason.filter(|s| !s.is_empty()) {
        updated.failure_reason = Some(truncate_utf8(r, MAX_FAILURE_REASON_LEN));
    }

    if outcome == SendOutcome::Retryable
        && let Some(delay) = retry_delay_ms(updated.attempts)
    {
        updated.phase = OutboxPhase::Ready;
        updated.not_before_ms = now_ms.saturating_add(delay as i64);
        return Some(updated);
    }

    updated.phase = OutboxPhase::Held;
    updated.not_before_ms = 0;
    if updated.ambiguous {
        updated.failure_reason = Some("delivery unconfirmed".to_owned());
    } else if updated.failure_reason.is_none() {
        updated.failure_reason = Some("send failed".to_owned());
    }
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_validation_and_generation() {
        let cid = generate_client_id().unwrap();
        assert_eq!(cid.len(), 32);
        assert!(is_valid_client_id(&cid));
        assert!(!is_valid_client_id("ABCDEF0123456789abcdef0123456789")); // uppercase rejected
        assert!(!is_valid_client_id("short"));
    }

    #[test]
    fn retry_schedule_enforces_auto_sends_bounded_and_preserves_client_id() {
        assert_eq!(retry_delay_ms(1), Some(3_000));
        assert_eq!(retry_delay_ms(2), Some(10_000));
        assert_eq!(retry_delay_ms(3), Some(30_000));
        assert_eq!(retry_delay_ms(4), None);

        let chat = ChatKey::new(Provider::Whatsapp, "personal", "1@s.whatsapp.net");
        let cid = format_client_id([0xab; 16]);
        let mut rec = OutboxRecord::new_text(cid.clone(), chat, "hello", None, false, 1_000);

        for (step, expected_delay) in [(1, 3_000), (2, 10_000), (3, 30_000)] {
            rec =
                apply_outcome(&rec, SendOutcome::Retryable, true, Some("timeout"), 10_000).unwrap();
            assert_eq!(rec.attempts, step);
            assert_eq!(rec.phase, OutboxPhase::Ready);
            assert_eq!(rec.not_before_ms, 10_000 + expected_delay);
            assert_eq!(rec.client_id, cid); // NoDouble!
        }

        // 4th failure gives up into Held (NoLoss + AutoSendsBounded).
        rec = apply_outcome(&rec, SendOutcome::Retryable, true, Some("timeout"), 50_000).unwrap();
        assert_eq!(rec.attempts, 4);
        assert_eq!(rec.phase, OutboxPhase::Held);
        assert!(rec.ambiguous);
        assert_eq!(rec.failure_reason.as_deref(), Some("delivery unconfirmed"));
        assert_eq!(rec.client_id, cid);
    }

    #[test]
    fn classify_http_outcomes_and_attachment_sha256_tamper_guard() {
        let c409 = classify_http_outcome(Provider::Whatsapp, false, 409, false, "conflict");
        assert_eq!(c409.outcome, SendOutcome::Held);
        assert!(c409.ambiguous);

        let c429_sig = classify_http_outcome(Provider::Signal, false, 429, false, "rate limited");
        assert_eq!(c429_sig.outcome, SendOutcome::Held);
        assert!(!c429_sig.ambiguous);

        let c429_wa = classify_http_outcome(Provider::Whatsapp, false, 429, false, "slow down");
        assert_eq!(c429_wa.outcome, SendOutcome::Retryable);

        let c500 = classify_http_outcome(Provider::Slack, false, 500, false, "oops");
        assert_eq!(c500.outcome, SendOutcome::Retryable);
        assert!(c500.ambiguous);

        let payload = b"hello attachment";
        let hash = sha256_bytes(payload);
        assert!(verify_attachment_bytes(
            payload,
            payload.len() as u64,
            &hash
        ));
        assert!(!verify_attachment_bytes(
            b"tampered payload",
            payload.len() as u64,
            &hash
        ));
    }

    #[test]
    fn validate_record_enforces_broadcast_and_filename_constraints() {
        let chat = ChatKey::new(Provider::Slack, "T1", "slack:T1:C1");
        let cid = format_client_id([0x11; 16]);
        let bad_broadcast = OutboxRecord::new_text(cid.clone(), chat.clone(), "hi", None, true, 1);
        assert_eq!(
            validate_record(&bad_broadcast),
            Err(OutboxError::InvalidBroadcast)
        );

        let ok_broadcast = OutboxRecord::new_text(
            cid.clone(),
            chat.clone(),
            "hi",
            Some("1700.1".into()),
            true,
            1,
        );
        assert!(validate_record(&ok_broadcast).is_ok());

        let bad_file = OutboxRecord::new_file(
            cid,
            chat,
            "",
            None,
            "/tmp/a.png",
            "../evil.png",
            [0u8; 32],
            10,
            1,
        );
        assert_eq!(
            validate_record(&bad_file),
            Err(OutboxError::InvalidFilename)
        );
    }
}
