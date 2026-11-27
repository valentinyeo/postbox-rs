use crate::{Chat, FontScale, Message, Provider, ThemeChoice};
use serde::{Deserialize, Serialize};

pub const CONTROL_PIPE_PREFIX: &str = r"\\.\pipe\postbox-control-";
/// One request line is at most this long.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// One response line is at most this long.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Preview length for chat rows sent to `postboxctl`.
pub const MAX_PREVIEW_CHARS: usize = 160;
/// Body length for message rows sent to `postboxctl`.
pub const MAX_BODY_CHARS: usize = 1000;
/// Default page size when the caller gives no `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Hard page ceiling, so one command can never pull the whole history into one line.
pub const MAX_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum ControlRequest {
    Status,
    Theme {
        theme: ThemeChoice,
    },
    Font {
        percent: u16,
    },
    Chats {
        provider: Option<Provider>,
        archived: bool,
        all: bool,
        limit: Option<u32>,
    },
    Search {
        text: String,
    },
    View {
        target: String,
    },
    Select {
        id: String,
        provider: Option<Provider>,
        account: Option<String>,
    },
    Messages {
        id: Option<String>,
        limit: Option<u32>,
    },
    Archive {
        id: String,
    },
    Unarchive {
        id: String,
    },
    ArchiveMany {
        ids: Vec<String>,
        archived: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryCounters {
    pub frames_rendered: u64,
    pub working_set_kb: u64,
    /// Private commit charge (`PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`). Defaulted so
    /// `postboxctl status` from an older build still deserializes against a newer one.
    #[serde(default)]
    pub private_usage_kb: u64,
    pub gpu_texture_bytes: u64,
    pub pending_outbox: u32,
}

/// Live `postboxctl status` payload: the counters plus what the window is showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub version: String,
    pub active_view: String,
    pub selected_chat: Option<String>,
    /// Frames per second over the last one-second window; `0.0` while idle.
    pub fps: f64,
    pub frames_rendered: u64,
    pub working_set_kb: u64,
    pub private_usage_kb: u64,
    pub gpu_texture_bytes: u64,
    pub pending_outbox: u32,
}

impl StatusReport {
    pub fn from_counters(
        counters: &TelemetryCounters,
        fps: f64,
        version: &str,
        active_view: &str,
        selected_chat: Option<&str>,
    ) -> Self {
        Self {
            version: version.to_owned(),
            active_view: active_view.to_owned(),
            selected_chat: selected_chat.map(str::to_owned),
            fps: (fps * 10.0).round() / 10.0,
            frames_rendered: counters.frames_rendered,
            working_set_kb: counters.working_set_kb,
            private_usage_kb: counters.private_usage_kb,
            gpu_texture_bytes: counters.gpu_texture_bytes,
            pending_outbox: counters.pending_outbox,
        }
    }
}

/// One chat row as `postboxctl chats` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub provider: Provider,
    pub account: String,
    pub id: String,
    pub title: String,
    pub unread: u32,
    pub archived: bool,
    pub last_ts: String,
    pub preview: String,
}

impl ChatSummary {
    pub fn from_chat(chat: &Chat) -> Self {
        Self {
            provider: chat.key.provider,
            account: chat.key.account.clone(),
            id: chat.key.id.clone(),
            title: chat.title.clone(),
            unread: chat.unread_count,
            archived: chat.is_archived(),
            last_ts: chat.last_msg_ts.clone(),
            preview: clip_chars(&chat.last_msg_preview, MAX_PREVIEW_CHARS),
        }
    }

    /// `whatsapp/<account>/<chat id>`, the same triple the app selects by.
    pub fn key_label(&self) -> String {
        format!("{}/{}/{}", self.provider.as_str(), self.account, self.id)
    }
}

/// One message row as `postboxctl messages` and `postboxctl search` print it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: String,
    pub provider: Provider,
    pub account: String,
    pub chat_id: String,
    pub sender: String,
    pub ts: String,
    pub from_me: bool,
    pub body: String,
}

impl MessageSummary {
    pub fn from_message(msg: &Message) -> Self {
        Self {
            id: msg.msg_id.clone(),
            provider: msg.chat.provider,
            account: msg.chat.account.clone(),
            chat_id: msg.chat.id.clone(),
            sender: msg.sender_name.clone(),
            ts: msg.ts.clone(),
            from_me: msg.from_me,
            body: clip_chars(&msg.body, MAX_BODY_CHARS),
        }
    }
}

/// One JSON line back to `postboxctl`. Exactly one payload field is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chats: Option<Vec<ChatSummary>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<MessageSummary>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    pub fn with_message(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            ..Self::default()
        }
    }

    pub fn with_status(status: StatusReport) -> Self {
        Self {
            ok: true,
            status: Some(status),
            ..Self::default()
        }
    }

    pub fn with_chats(chats: Vec<ChatSummary>) -> Self {
        Self {
            ok: true,
            chats: Some(chats),
            ..Self::default()
        }
    }

    pub fn with_messages(messages: Vec<MessageSummary>) -> Self {
        Self {
            ok: true,
            messages: Some(messages),
            ..Self::default()
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// `true` when the line reports success and carries a payload.
    pub fn has_payload(&self) -> bool {
        self.status.is_some()
            || self.chats.is_some()
            || self.messages.is_some()
            || self.message.is_some()
    }
}

impl ControlRequest {
    /// Command word as typed on the command line (`archive-many`, `select`, ...).
    pub fn command_name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Theme { .. } => "theme",
            Self::Font { .. } => "font",
            Self::Chats { .. } => "chats",
            Self::Search { .. } => "search",
            Self::View { .. } => "view",
            Self::Select { .. } => "select",
            Self::Messages { .. } => "messages",
            Self::Archive { .. } => "archive",
            Self::Unarchive { .. } => "unarchive",
            Self::ArchiveMany { .. } => "archive-many",
        }
    }

    /// `true` when the request changes what the window shows, so the shell must be woken.
    pub fn needs_ui(&self) -> bool {
        match self {
            Self::Status
            | Self::Chats { .. }
            | Self::Search { .. }
            | Self::Messages { .. }
            | Self::Archive { .. }
            | Self::Unarchive { .. }
            | Self::ArchiveMany { .. } => false,
            Self::Theme { .. } | Self::Font { .. } | Self::View { .. } | Self::Select { .. } => {
                true
            }
        }
    }
}

/// Page size for a list command, clamped to [`MAX_LIMIT`].
pub fn page_limit(limit: Option<u32>) -> usize {
    match limit {
        Some(raw) if raw > 0 => (raw as usize).min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

/// Font percentage a `font` request means, clamped to the supported 60-160 range.
pub fn clamped_percent(percent: u16) -> u16 {
    FontScale::clamped(percent).percent()
}

/// Encodes one request as the single JSON line the server reads.
pub fn encode_request(request: &ControlRequest) -> String {
    serde_json::to_string(request).unwrap_or_else(|_| r#"{"cmd":"status"}"#.to_owned())
}

/// Encodes one response as the single JSON line the client reads.
pub fn encode_response(response: &ControlResponse) -> String {
    serde_json::to_string(response)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"encode"}"#.to_owned())
}

pub fn decode_request(line: &str) -> Result<ControlRequest, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("empty request".to_owned());
    }
    if trimmed.len() > MAX_REQUEST_BYTES {
        return Err("request too large".to_owned());
    }
    serde_json::from_str(trimmed).map_err(|err| format!("invalid request: {err}"))
}

pub fn decode_response(line: &str) -> Result<ControlResponse, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("empty response".to_owned());
    }
    serde_json::from_str(trimmed).map_err(|err| format!("invalid response: {err}"))
}

/// Frame counter that turns render ticks into a one-second FPS figure.
#[derive(Debug, Clone)]
pub struct FpsMeter {
    window_start_ms: u64,
    frames: u32,
    fps: f64,
}

impl Default for FpsMeter {
    fn default() -> Self {
        Self {
            window_start_ms: 0,
            frames: 0,
            fps: 0.0,
        }
    }
}

impl FpsMeter {
    pub const WINDOW_MS: u64 = 1_000;

    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one painted frame. The first frame opens a window instead of reporting a spike.
    pub fn on_frame(&mut self, now_ms: u64) {
        if self.window_start_ms == 0 {
            self.window_start_ms = now_ms;
            self.frames = 1;
            return;
        }
        self.frames = self.frames.saturating_add(1);
        let elapsed = now_ms.saturating_sub(self.window_start_ms);
        if elapsed >= Self::WINDOW_MS {
            self.fps = f64::from(self.frames) * 1000.0 / elapsed as f64;
            self.window_start_ms = now_ms;
            self.frames = 0;
        }
    }

    /// Last completed window's frames per second; `0.0` before the first window closes.
    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// Frames painted since the current window opened.
    pub fn frames_in_window(&self) -> u32 {
        self.frames
    }
}

fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_counters() -> TelemetryCounters {
        TelemetryCounters {
            frames_rendered: 120,
            working_set_kb: 61 * 1024,
            private_usage_kb: 54 * 1024,
            gpu_texture_bytes: 16 * 1024 * 1024,
            pending_outbox: 3,
        }
    }

    #[test]
    fn telemetry_counters_round_trip_working_set_and_private_usage() {
        let counters = full_counters();
        let json = serde_json::to_string(&counters).unwrap();
        assert!(json.contains("\"working_set_kb\":62464"));
        assert!(json.contains("\"private_usage_kb\":55296"));
        let back: TelemetryCounters = serde_json::from_str(&json).unwrap();
        assert_eq!(back, counters);
    }

    #[test]
    fn telemetry_counters_tolerate_older_payloads_without_private_usage() {
        // A `postboxctl status` written by a pre-M5.5 build must still deserialize.
        let legacy = r#"{
            "frames_rendered": 7,
            "working_set_kb": 4096,
            "gpu_texture_bytes": 1024,
            "pending_outbox": 0
        }"#;
        let parsed: TelemetryCounters = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.private_usage_kb, 0);
        assert_eq!(parsed.working_set_kb, 4096);
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::{ChatKey, MediaKind, ReadStatus};

    fn counters() -> TelemetryCounters {
        TelemetryCounters {
            frames_rendered: 42,
            working_set_kb: 61 * 1024,
            private_usage_kb: 54 * 1024,
            gpu_texture_bytes: 16 * 1024 * 1024,
            pending_outbox: 2,
        }
    }

    fn chat() -> Chat {
        Chat {
            key: ChatKey::new(Provider::Whatsapp, "biz", "491234@s.whatsapp.net"),
            title: "Ada".into(),
            is_group: false,
            archived_local: false,
            archived_remote: true,
            pinned: false,
            unread_count: 3,
            last_msg_ts: "2025-01-15T12:00:00Z".into(),
            last_msg_preview: "See you then".into(),
            avatar_hash: None,
        }
    }

    fn message() -> Message {
        Message {
            chat: chat().key,
            msg_id: "MSG-1".into(),
            sender_id: "491234".into(),
            sender_name: "Ada".into(),
            from_me: false,
            ts: "2025-01-15T12:00:00Z".into(),
            body: "Ping".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(MediaKind::Image),
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        }
    }

    #[test]
    fn every_request_variant_round_trips_through_one_json_line() {
        let requests = vec![
            ControlRequest::Status,
            ControlRequest::Theme {
                theme: ThemeChoice::Amoled,
            },
            ControlRequest::Font { percent: 130 },
            ControlRequest::Chats {
                provider: Some(Provider::Slack),
                archived: false,
                all: true,
                limit: Some(20),
            },
            ControlRequest::Search {
                text: "invoice".into(),
            },
            ControlRequest::View {
                target: "slack:T01".into(),
            },
            ControlRequest::Select {
                id: "C02".into(),
                provider: Some(Provider::Slack),
                account: Some("T01".into()),
            },
            ControlRequest::Messages {
                id: Some("C02".into()),
                limit: Some(5),
            },
            ControlRequest::Archive { id: "C02".into() },
            ControlRequest::Unarchive { id: "C02".into() },
            ControlRequest::ArchiveMany {
                ids: vec!["C02".into(), "C03".into()],
                archived: true,
            },
        ];
        for request in requests {
            let line = encode_request(&request);
            assert!(!line.contains('\n'), "one line only: {line}");
            assert_eq!(decode_request(&line).unwrap(), request);
        }
    }

    #[test]
    fn request_lines_carry_a_stable_command_word() {
        assert_eq!(ControlRequest::Status.command_name(), "status");
        assert_eq!(
            ControlRequest::ArchiveMany {
                ids: vec![],
                archived: true
            }
            .command_name(),
            "archive-many"
        );
        assert!(encode_request(&ControlRequest::Status).contains(r#""cmd":"status""#));
    }

    #[test]
    fn malformed_request_lines_are_refused_without_panicking() {
        assert!(decode_request("").is_err());
        assert!(decode_request("not json").is_err());
        assert!(decode_request(r#"{"cmd":"nope"}"#).is_err());
        let huge = format!(
            r#"{{"cmd":"search","text":"{}"}}"#,
            "x".repeat(MAX_REQUEST_BYTES)
        );
        assert_eq!(decode_request(&huge).unwrap_err(), "request too large");
    }

    #[test]
    fn status_response_reports_live_ram_and_fps() {
        let status = StatusReport::from_counters(
            &counters(),
            59.94,
            "0.1.0",
            "all",
            Some("whatsapp/biz/491234@s.whatsapp.net"),
        );
        let response = ControlResponse::with_status(status.clone());
        let line = encode_response(&response);
        assert!(line.contains(r#""working_set_kb":62464"#), "{line}");
        assert!(line.contains(r#""private_usage_kb":55296"#), "{line}");
        assert!(line.contains(r#""fps":59.9"#), "{line}");
        assert!(line.contains(r#""active_view":"all""#), "{line}");
        assert!(
            line.contains(r#""selected_chat":"whatsapp/biz/491234@s.whatsapp.net""#),
            "{line}"
        );
        let decoded = decode_response(&line).unwrap();
        assert_eq!(decoded, response);
        assert!(decoded.has_payload());
    }

    #[test]
    fn payload_fields_are_omitted_when_unset() {
        let line = encode_response(&ControlResponse::error("no such chat: C99"));
        assert_eq!(line, r#"{"ok":false,"error":"no such chat: C99"}"#);
        let decoded = decode_response(&line).unwrap();
        assert!(!decoded.ok);
        assert!(!decoded.has_payload());
        assert_eq!(decoded.error.as_deref(), Some("no such chat: C99"));
    }

    #[test]
    fn chat_and_message_rows_follow_the_domain_types() {
        let summary = ChatSummary::from_chat(&chat());
        assert_eq!(summary.provider, Provider::Whatsapp);
        assert_eq!(summary.account, "biz");
        assert_eq!(summary.id, "491234@s.whatsapp.net");
        assert!(summary.archived, "either archive flag means archived");
        assert_eq!(summary.unread, 3);
        assert_eq!(summary.key_label(), "whatsapp/biz/491234@s.whatsapp.net");

        let row = MessageSummary::from_message(&message());
        assert_eq!(row.id, "MSG-1");
        assert_eq!(row.sender, "Ada");
        assert!(!row.from_me);
        assert_eq!(row.provider, Provider::Whatsapp);
    }

    #[test]
    fn previews_are_clipped_so_one_response_line_stays_small() {
        let mut long = chat();
        long.last_msg_preview = "y".repeat(MAX_PREVIEW_CHARS * 2);
        let summary = ChatSummary::from_chat(&long);
        assert_eq!(summary.preview.chars().count(), MAX_PREVIEW_CHARS + 1);

        let mut huge = message();
        huge.body = "z".repeat(MAX_BODY_CHARS * 2);
        let row = MessageSummary::from_message(&huge);
        assert_eq!(row.body.chars().count(), MAX_BODY_CHARS + 1);
    }

    #[test]
    fn page_limits_and_percentages_are_clamped() {
        assert_eq!(page_limit(None), DEFAULT_LIMIT);
        assert_eq!(page_limit(Some(0)), DEFAULT_LIMIT);
        assert_eq!(page_limit(Some(7)), 7);
        assert_eq!(page_limit(Some(9_999)), MAX_LIMIT);

        assert_eq!(clamped_percent(10), 60);
        assert_eq!(clamped_percent(200), 160);
        assert_eq!(clamped_percent(110), 110);
    }

    #[test]
    fn only_view_changing_commands_need_the_window() {
        assert!(!ControlRequest::Status.needs_ui());
        assert!(!ControlRequest::Archive { id: "x".into() }.needs_ui());
        assert!(ControlRequest::Font { percent: 100 }.needs_ui());
        assert!(
            ControlRequest::Select {
                id: "x".into(),
                provider: None,
                account: None
            }
            .needs_ui()
        );
    }

    #[test]
    fn fps_meter_reports_a_one_second_window_and_zero_when_idle() {
        let mut meter = FpsMeter::new();
        assert_eq!(meter.fps(), 0.0);
        meter.on_frame(1_000);
        assert_eq!(meter.frames_in_window(), 1);
        for tick in 1..=70 {
            meter.on_frame(1_000 + tick * 16);
        }
        // 64 frames across 1008 ms, then the window reopened and counted the remaining 7.
        assert!((60.0..=66.0).contains(&meter.fps()), "{}", meter.fps());
        assert_eq!(meter.frames_in_window(), 7);
        // No further frames: the last completed window still reads back.
        assert!((60.0..=66.0).contains(&meter.fps()));
    }
}
