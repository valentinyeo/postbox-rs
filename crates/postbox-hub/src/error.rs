use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HubError {
    #[error("invalid_url")]
    InvalidUrl,
    #[error("insecure_url")]
    InsecureUrl,
    #[error("invalid_token")]
    InvalidToken,
    #[error("invalid_id")]
    InvalidId,
    #[error("invalid_team")]
    InvalidTeam,
    #[error("invalid_client_id")]
    InvalidClientId,
    #[error("invalid_send")]
    InvalidSend,
    #[error("invalid_boundary")]
    InvalidBoundary,
    #[error("file_too_large")]
    FileTooLarge,
    #[error("bad_response")]
    BadResponse,
    #[error("duplicate_id")]
    DuplicateId,
    #[error("bad_provider")]
    BadProvider,
    #[error("wrong_chat")]
    WrongChat,
    #[error("no_incoming_message")]
    NoIncomingMessage,
    #[error("event_too_large")]
    EventTooLarge,
    #[error("bad_event_id")]
    BadEventId,
    #[error("response_too_large")]
    ResponseTooLarge,
    #[error("partial_media")]
    PartialMedia,
    #[error("send_not_confirmed")]
    SendNotConfirmed,
    #[error("http_status_{status}")]
    HttpStatus { status: u16, body: String },
    #[error("transport_error: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalSendFailure {
    RateLimited { retry_after: Option<u64> },
    CaptchaRequired,
}

impl SignalSendFailure {
    pub fn reason(self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "signal rate limited",
            Self::CaptchaRequired => "signal captcha required",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramSendFailure {
    RateLimited { retry_after: Option<u64> },
}

impl TelegramSendFailure {
    pub fn reason(self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "telegram rate limited",
        }
    }
}

/// Exact port of `hub.zig:371-380`: classifies Signal `429` (`signal rate limited`) and
/// `403` (`signal captcha required`) JSON responses.
pub fn classify_signal_send_failure(status: u16, body: &str) -> Option<SignalSendFailure> {
    if status != 429 && status != 403 {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = val.as_object()?;
    let err = obj.get("error")?.as_str()?;
    let retry_after = obj.get("retry_after").and_then(|v| v.as_u64());
    match (status, err) {
        (429, "signal rate limited") => Some(SignalSendFailure::RateLimited { retry_after }),
        (403, "signal captcha required") => Some(SignalSendFailure::CaptchaRequired),
        _ => None,
    }
}

/// Exact port of `hub.zig:400-408`: classifies Telegram `429` (`telegram rate limited`) responses.
pub fn classify_telegram_send_failure(status: u16, body: &str) -> Option<TelegramSendFailure> {
    if status != 429 {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = val.as_object()?;
    let err = obj.get("error")?.as_str()?;
    if err == "telegram rate limited" {
        let retry_after = obj.get("retry_after").and_then(|v| v.as_u64());
        Some(TelegramSendFailure::RateLimited { retry_after })
    } else {
        None
    }
}
