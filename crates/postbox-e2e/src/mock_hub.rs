#![forbid(unsafe_code)]

use crate::fixtures::minimal_png_1x1;
use crate::wire::{
    DEFAULT_MAX_BODY_BYTES, LoopbackServer, MultipartPart, RawHttpRequest, RawHttpResponse,
    WireError, parse_multipart, percent_decode, read_http_request,
};
use postbox_hub::{Endpoint, MAX_FILE_BYTES, SseEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubChatRow {
    pub jid: String,
    pub provider: String,
    pub name: String,
    pub kind: String,
    pub last_message_ts: String,
    pub unread_count: u32,
    pub unread: bool,
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct HubMessageRow {
    pub MsgID: String,
    pub ChatJID: String,
    pub Text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub DisplayText: Option<String>,
    pub Timestamp: String,
    pub SenderJID: String,
    pub SenderName: String,
    pub FromMe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub MediaType: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub MimeType: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub Filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub MediaSize: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ReactionToID: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ReactionEmoji: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ReadBy: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub Delivered: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub Revoked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub LocalPath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub SlackEmojiPaths: Option<HashMap<String, String>>,
}

impl HubMessageRow {
    pub fn simple(
        chat_jid: impl Into<String>,
        msg_id: impl Into<String>,
        sender_name: impl Into<String>,
        text: impl Into<String>,
        timestamp: impl Into<String>,
        from_me: bool,
    ) -> Self {
        let sender = sender_name.into();
        Self {
            MsgID: msg_id.into(),
            ChatJID: chat_jid.into(),
            Text: text.into(),
            DisplayText: None,
            Timestamp: timestamp.into(),
            SenderJID: format!("sender:{}", sender.to_ascii_lowercase()),
            SenderName: sender,
            FromMe: from_me,
            quoted_msg_id: None,
            thread_ts: None,
            reply_count: None,
            latest_reply: None,
            MediaType: None,
            MimeType: None,
            Filename: None,
            MediaSize: None,
            ReactionToID: None,
            ReactionEmoji: None,
            reactions: None,
            attachments: None,
            ReadBy: None,
            Delivered: None,
            Revoked: None,
            LocalPath: None,
            local_path: None,
            SlackEmojiPaths: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedHubRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub authorization: Option<String>,
    pub user_agent: Option<String>,
    pub content_type: Option<String>,
    pub last_event_id: Option<String>,
    pub body: Vec<u8>,
    pub multipart_parts: Option<Vec<MultipartPart>>,
}

#[derive(Debug, Clone)]
pub enum ChatsFault {
    Overflow501,
    DuplicateJid,
    UnknownProvider(String),
    HttpStatus(u16),
}

#[derive(Debug, Clone)]
pub enum MessagesFault {
    SameAccountLidAlias { lid_chat: String },
    CrossAccountLidMismatch { wrong_chat: String },
    GroupMismatch { group_chat: String },
    SlackAttachmentsAndReactions,
    LeakLocalPaths,
    ProtocolStub,
    DuplicateMsgId,
    Overflow501,
    HttpStatus(u16),
}

#[derive(Debug, Clone)]
pub enum SendFault {
    Ambiguous409 {
        commit_anyway: bool,
    },
    TooLarge413,
    SignalRateLimit429 {
        retry_after: u32,
    },
    TelegramRateLimit429 {
        retry_after: u32,
    },
    SignalCaptcha403,
    Transient500,
    Transient503,
    Unconfirmed200 {
        bad_client_id: String,
        bad_status: String,
    },
    DropConnection,
}

#[derive(Debug, Clone)]
pub enum ArchiveBatchFault {
    PartialFailure(HashSet<String>),
    MismatchedCount,
    HttpStatus(u16),
}

#[derive(Debug, Clone)]
pub enum MediaBehavior {
    Ready {
        bytes: Vec<u8>,
        content_type: String,
    },
    PendingThenReady {
        remaining_202s: usize,
        bytes: Vec<u8>,
        content_type: String,
    },
    Partial206 {
        bytes: Vec<u8>,
        content_type: String,
    },
    NotFound404,
    Gone410,
    RateLimit429,
    ServerError500,
}

#[derive(Debug, Clone)]
enum SseEntry {
    Event { id: u64, frame: Vec<u8> },
    Raw { frame: Vec<u8> },
}

struct MockHubState {
    token: String,
    chats: Vec<HubChatRow>,
    messages: HashMap<String, Vec<HubMessageRow>>,
    recorded: Vec<RecordedHubRequest>,
    client_id_replies: HashMap<String, Value>,
    client_id_deliveries: HashMap<String, usize>,
    next_msg_seq: u64,
    max_multipart_bytes: usize,
    chats_fault: Option<ChatsFault>,
    messages_faults: HashMap<String, MessagesFault>,
    send_faults: VecDeque<SendFault>,
    mark_read_faults: VecDeque<u16>,
    archive_overrides: HashMap<String, u16>,
    archive_batch_fault: Option<ArchiveBatchFault>,
    react_faults: VecDeque<u16>,
    delete_faults: VecDeque<u16>,
    crash_report_faults: VecDeque<u16>,
    crash_report_ids: Vec<String>,
    media: HashMap<(String, String), MediaBehavior>,
    avatars: HashMap<String, Result<Vec<u8>, u16>>,
    workspace_icons: HashMap<String, Result<Vec<u8>, u16>>,
    slack_emoji_index: HashMap<String, Value>,
    slack_emoji_media: HashMap<(String, String), Result<Vec<u8>, u16>>,
    sse_log: Vec<SseEntry>,
    sse_connect_faults: VecDeque<u16>,
    sse_connections: Vec<Option<String>>,
    sse_close_generation: u64,
}

impl MockHubState {
    fn new(token: String) -> Self {
        Self {
            token,
            chats: Vec::new(),
            messages: HashMap::new(),
            recorded: Vec::new(),
            client_id_replies: HashMap::new(),
            client_id_deliveries: HashMap::new(),
            next_msg_seq: 1,
            max_multipart_bytes: MAX_FILE_BYTES,
            chats_fault: None,
            messages_faults: HashMap::new(),
            send_faults: VecDeque::new(),
            mark_read_faults: VecDeque::new(),
            archive_overrides: HashMap::new(),
            archive_batch_fault: None,
            react_faults: VecDeque::new(),
            delete_faults: VecDeque::new(),
            crash_report_faults: VecDeque::new(),
            crash_report_ids: Vec::new(),
            media: HashMap::new(),
            avatars: HashMap::new(),
            workspace_icons: HashMap::new(),
            slack_emoji_index: HashMap::new(),
            slack_emoji_media: HashMap::new(),
            sse_log: Vec::new(),
            sse_connect_faults: VecDeque::new(),
            sse_connections: Vec::new(),
            sse_close_generation: 0,
        }
    }
}

pub struct MockHubServer {
    server: LoopbackServer,
    state: Arc<Mutex<MockHubState>>,
    sse_cv: Arc<Condvar>,
}

impl MockHubServer {
    pub fn start(token: impl Into<String>) -> Self {
        let state = Arc::new(Mutex::new(MockHubState::new(token.into())));
        let sse_cv = Arc::new(Condvar::new());

        let notify_cv = Arc::clone(&sse_cv);
        let notify = Arc::new(move || {
            notify_cv.notify_all();
        });

        let handler_state = Arc::clone(&state);
        let handler_cv = Arc::clone(&sse_cv);
        let server = LoopbackServer::start("mock-hub", notify, move |stream, shutdown| {
            handle_hub_connection(stream, shutdown, &handler_state, &handler_cv);
        })
        .expect("bind MockHubServer on 127.0.0.1:0");

        Self {
            server,
            state,
            sse_cv,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::parse(&self.base_url(), true).expect("loopback endpoint parses")
    }

    pub fn token(&self) -> String {
        self.state.lock().unwrap().token.clone()
    }

    pub fn set_chats(&self, chats: Vec<HubChatRow>) {
        self.state.lock().unwrap().chats = chats;
    }

    pub fn add_chat(&self, chat: HubChatRow) {
        self.state.lock().unwrap().chats.push(chat);
    }

    pub fn set_messages(&self, chat_jid: impl Into<String>, messages: Vec<HubMessageRow>) {
        self.state
            .lock()
            .unwrap()
            .messages
            .insert(chat_jid.into(), messages);
    }

    pub fn set_max_multipart_bytes(&self, limit: usize) {
        self.state.lock().unwrap().max_multipart_bytes = limit;
    }

    pub fn set_chats_fault(&self, fault: Option<ChatsFault>) {
        self.state.lock().unwrap().chats_fault = fault;
    }

    pub fn set_messages_fault(&self, chat_jid: impl Into<String>, fault: MessagesFault) {
        self.state
            .lock()
            .unwrap()
            .messages_faults
            .insert(chat_jid.into(), fault);
    }

    pub fn push_send_fault(&self, fault: SendFault) {
        self.state.lock().unwrap().send_faults.push_back(fault);
    }

    pub fn push_mark_read_fault(&self, status: u16) {
        self.state
            .lock()
            .unwrap()
            .mark_read_faults
            .push_back(status);
    }

    pub fn set_archive_override(&self, chat_jid: impl Into<String>, status: u16) {
        self.state
            .lock()
            .unwrap()
            .archive_overrides
            .insert(chat_jid.into(), status);
    }

    pub fn set_archive_batch_fault(&self, fault: Option<ArchiveBatchFault>) {
        self.state.lock().unwrap().archive_batch_fault = fault;
    }

    pub fn push_react_fault(&self, status: u16) {
        self.state.lock().unwrap().react_faults.push_back(status);
    }

    pub fn push_delete_fault(&self, status: u16) {
        self.state.lock().unwrap().delete_faults.push_back(status);
    }

    /// Makes the next crash-report upload answer with `status` instead of accepting it.
    pub fn push_crash_report_fault(&self, status: u16) {
        self.state
            .lock()
            .unwrap()
            .crash_report_faults
            .push_back(status);
    }

    /// Report ids the Hub accepted, in arrival order. A repeat would show up twice.
    pub fn accepted_crash_report_ids(&self) -> Vec<String> {
        self.state.lock().unwrap().crash_report_ids.clone()
    }

    pub fn set_media(
        &self,
        chat_jid: impl Into<String>,
        msg_id: impl Into<String>,
        behavior: MediaBehavior,
    ) {
        self.state
            .lock()
            .unwrap()
            .media
            .insert((chat_jid.into(), msg_id.into()), behavior);
    }

    pub fn set_avatar(&self, chat_jid: impl Into<String>, result: Result<Vec<u8>, u16>) {
        self.state
            .lock()
            .unwrap()
            .avatars
            .insert(chat_jid.into(), result);
    }

    pub fn set_workspace_icon(&self, team: impl Into<String>, result: Result<Vec<u8>, u16>) {
        self.state
            .lock()
            .unwrap()
            .workspace_icons
            .insert(team.into(), result);
    }

    pub fn set_slack_emoji_index(&self, team: impl Into<String>, index: Value) {
        self.state
            .lock()
            .unwrap()
            .slack_emoji_index
            .insert(team.into(), index);
    }

    pub fn set_slack_emoji_media(
        &self,
        team: impl Into<String>,
        name: impl Into<String>,
        result: Result<Vec<u8>, u16>,
    ) {
        self.state
            .lock()
            .unwrap()
            .slack_emoji_media
            .insert((team.into(), name.into()), result);
    }

    pub fn push_sse_event(&self, event: SseEvent) {
        let json_str = serde_json::to_string(&event).unwrap();
        let frame = format!("id: {}\ndata: {json_str}\n\n", event.id).into_bytes();
        self.state.lock().unwrap().sse_log.push(SseEntry::Event {
            id: event.id,
            frame,
        });
        self.sse_cv.notify_all();
    }

    pub fn push_sse_burst(&self, events: Vec<SseEvent>) {
        {
            let mut st = self.state.lock().unwrap();
            for event in events {
                let json_str = serde_json::to_string(&event).unwrap();
                let frame = format!("id: {}\ndata: {json_str}\n\n", event.id).into_bytes();
                st.sse_log.push(SseEntry::Event {
                    id: event.id,
                    frame,
                });
            }
        }
        self.sse_cv.notify_all();
    }

    pub fn push_sse_heartbeat(&self) {
        self.state.lock().unwrap().sse_log.push(SseEntry::Raw {
            frame: b": heartbeat\n\n".to_vec(),
        });
        self.sse_cv.notify_all();
    }

    pub fn push_sse_corrupt_id_mismatch(&self, wire_id: u64, json_id: u64) {
        let payload = json!({
            "id": json_id,
            "account": "personal",
            "chat": "personal:1@s.whatsapp.net",
            "ts": "2026-11-26T12:00:00Z",
            "kind": "msg"
        });
        let frame = format!("id: {wire_id}\ndata: {payload}\n\n").into_bytes();
        self.state
            .lock()
            .unwrap()
            .sse_log
            .push(SseEntry::Raw { frame });
        self.sse_cv.notify_all();
    }

    pub fn push_sse_oversized_frame(&self, payload_bytes: usize) {
        let pad = "x".repeat(payload_bytes);
        let frame =
            format!("id: 99999\ndata: {{\"id\":99999,\"kind\":\"msg\",\"pad\":\"{pad}\"}}\n\n")
                .into_bytes();
        self.state
            .lock()
            .unwrap()
            .sse_log
            .push(SseEntry::Raw { frame });
        self.sse_cv.notify_all();
    }

    pub fn close_all_sse_streams(&self) {
        self.state.lock().unwrap().sse_close_generation += 1;
        self.sse_cv.notify_all();
    }

    pub fn set_sse_connect_faults(&self, faults: VecDeque<u16>) {
        self.state.lock().unwrap().sse_connect_faults = faults;
    }

    pub fn sse_connection_last_event_ids(&self) -> Vec<Option<String>> {
        self.state.lock().unwrap().sse_connections.clone()
    }

    pub fn recorded_requests(&self) -> Vec<RecordedHubRequest> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn clear_recorded_requests(&self) {
        self.state.lock().unwrap().recorded.clear();
    }

    pub fn request_count(&self, method: &str, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .recorded
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .count()
    }

    pub fn client_id_delivery_count(&self, client_id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .client_id_deliveries
            .get(client_id)
            .copied()
            .unwrap_or(0)
    }
}

fn handle_hub_connection(
    mut stream: TcpStream,
    shutdown: Arc<AtomicBool>,
    state: &Arc<Mutex<MockHubState>>,
    sse_cv: &Arc<Condvar>,
) {
    let req = match read_http_request(&mut stream, DEFAULT_MAX_BODY_BYTES) {
        Ok(r) => r,
        Err(WireError::BodyTooLarge(_)) => {
            let _ = RawHttpResponse::json(413, &json!({"error": "payload too large"}))
                .write_to(&mut stream);
            return;
        }
        Err(_) => return,
    };

    let multipart_parts = req
        .header("content-type")
        .filter(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
        .and_then(|ct| parse_multipart(ct, &req.body).ok());

    let recorded = RecordedHubRequest {
        method: req.method.clone(),
        path: req.path.clone(),
        query: req.query_params.clone(),
        authorization: req.header("authorization").map(ToOwned::to_owned),
        user_agent: req.header("user-agent").map(ToOwned::to_owned),
        content_type: req.header("content-type").map(ToOwned::to_owned),
        last_event_id: req.header("last-event-id").map(ToOwned::to_owned),
        body: req.body.clone(),
        multipart_parts: multipart_parts.clone(),
    };

    let expected_auth = {
        let mut st = state.lock().unwrap();
        st.recorded.push(recorded);
        format!("Bearer {}", st.token)
    };

    if req.header("authorization") != Some(expected_auth.as_str()) {
        let _ = RawHttpResponse::json(401, &json!({"error": "unauthorized"})).write_to(&mut stream);
        return;
    }

    if req.method == "GET" && req.path == "/v1/wazig/events" {
        handle_sse_stream(stream, &req, shutdown, state, sse_cv);
        return;
    }

    let response = dispatch_hub_request(&req, multipart_parts.as_deref(), state);
    if let Some(resp) = response {
        let _ = resp.write_to(&mut stream);
    }
}

fn handle_sse_stream(
    mut stream: TcpStream,
    req: &RawHttpRequest,
    shutdown: Arc<AtomicBool>,
    state: &Arc<Mutex<MockHubState>>,
    sse_cv: &Arc<Condvar>,
) {
    let last_event_id_raw = req.header("last-event-id").map(ToOwned::to_owned);
    let last_id: u64 = last_event_id_raw
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let (close_gen, initial_frames, initial_cursor, fault_status) = {
        let mut st = state.lock().unwrap();
        st.sse_connections.push(last_event_id_raw);
        if let Some(status) = st.sse_connect_faults.pop_front() {
            (st.sse_close_generation, Vec::new(), 0, Some(status))
        } else {
            let mut frames = Vec::new();
            for entry in &st.sse_log {
                match entry {
                    SseEntry::Event { id, frame } if *id > last_id => frames.push(frame.clone()),
                    SseEntry::Raw { frame } => frames.push(frame.clone()),
                    _ => {}
                }
            }
            (st.sse_close_generation, frames, st.sse_log.len(), None)
        }
    };

    if let Some(status) = fault_status {
        let _ = RawHttpResponse::json(status, &json!({"error": "sse connect fault"}))
            .write_to(&mut stream);
        return;
    }

    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }

    for frame in initial_frames {
        if stream.write_all(&frame).is_err() || stream.flush().is_err() {
            return;
        }
    }

    let mut cursor = initial_cursor;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let guard = state.lock().unwrap();
        if guard.sse_close_generation != close_gen {
            break;
        }
        if cursor < guard.sse_log.len() {
            let pending: Vec<Vec<u8>> = guard.sse_log[cursor..]
                .iter()
                .filter_map(|entry| match entry {
                    SseEntry::Event { id, frame } if *id > last_id => Some(frame.clone()),
                    SseEntry::Raw { frame } => Some(frame.clone()),
                    _ => None,
                })
                .collect();
            cursor = guard.sse_log.len();
            drop(guard);
            for frame in pending {
                if stream.write_all(&frame).is_err() || stream.flush().is_err() {
                    return;
                }
            }
            continue;
        }
        let _ = sse_cv.wait_timeout(guard, Duration::from_millis(50));
    }
}

fn dispatch_hub_request(
    req: &RawHttpRequest,
    multipart: Option<&[MultipartPart]>,
    state: &Arc<Mutex<MockHubState>>,
) -> Option<RawHttpResponse> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/wazig/chats") => Some(handle_chats(req, state)),
        ("GET", "/v1/wazig/messages") => Some(handle_messages(req, state)),
        ("POST", "/v1/wazig/send") => handle_send(req, multipart, state),
        ("POST", "/v1/wazig/mark-read") => Some(handle_mark_read(req, state)),
        ("POST", "/v1/wazig/archive") => Some(handle_archive(req, state)),
        ("POST", "/v1/wazig/archive-batch") => Some(handle_archive_batch(req, state)),
        ("POST", "/v1/wazig/react") => Some(handle_react(req, state)),
        ("POST", "/v1/wazig/delete") => Some(handle_delete(req, state)),
        ("POST", "/v1/wazig/crash-reports") => Some(handle_crash_report(req, state)),
        ("GET", "/v1/wazig/slack/emoji") => Some(handle_slack_emoji_index(req, state)),
        ("GET", "/v1/wazig/slack/emoji/media") => Some(handle_slack_emoji_media(req, state)),
        ("GET", p) if p.starts_with("/v1/wazig/media/") => Some(handle_media(p, state)),
        ("GET", p) if p.starts_with("/v1/wazig/avatar/") => Some(handle_avatar(p, state)),
        ("GET", p) if p.starts_with("/v1/wazig/workspace-icon/") => {
            Some(handle_workspace_icon(p, state))
        }
        _ => Some(RawHttpResponse::json(404, &json!({"error": "not found"}))),
    }
}

fn handle_chats(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let archived_filter = req.query("archived") == Some("true");
    let unread_only = req.query("unread") == Some("true");
    let limit = req
        .query("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50)
        .min(500);

    let st = state.lock().unwrap();
    if let Some(fault) = &st.chats_fault {
        match fault {
            ChatsFault::HttpStatus(code) => {
                return RawHttpResponse::json(*code, &json!({"error": "chats fault"}));
            }
            ChatsFault::Overflow501 => {
                let rows: Vec<HubChatRow> = (0..501)
                    .map(|i| HubChatRow {
                        jid: format!("personal:1555{i:06}@s.whatsapp.net"),
                        provider: "whatsapp".into(),
                        name: format!("Overflow {i}"),
                        kind: "dm".into(),
                        last_message_ts: "2026-11-26T10:00:00Z".into(),
                        unread_count: 0,
                        unread: false,
                        archived: false,
                        account: Some("personal".into()),
                        team_name: None,
                        member_count: None,
                        saved: None,
                    })
                    .collect();
                return RawHttpResponse::json(200, &serde_json::to_value(rows).unwrap());
            }
            ChatsFault::DuplicateJid => {
                let dup = HubChatRow {
                    jid: "personal:dup@s.whatsapp.net".into(),
                    provider: "whatsapp".into(),
                    name: "Dup".into(),
                    kind: "dm".into(),
                    last_message_ts: "2026-11-26T10:00:00Z".into(),
                    unread_count: 1,
                    unread: true,
                    archived: false,
                    account: Some("personal".into()),
                    team_name: None,
                    member_count: None,
                    saved: None,
                };
                return RawHttpResponse::json(
                    200,
                    &serde_json::to_value(vec![dup.clone(), dup]).unwrap(),
                );
            }
            ChatsFault::UnknownProvider(prov) => {
                let row = HubChatRow {
                    jid: "irc:chan1".into(),
                    provider: prov.clone(),
                    name: "Unknown".into(),
                    kind: "group".into(),
                    last_message_ts: "2026-11-26T10:00:00Z".into(),
                    unread_count: 0,
                    unread: false,
                    archived: false,
                    account: None,
                    team_name: None,
                    member_count: None,
                    saved: None,
                };
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
        }
    }

    let rows: Vec<HubChatRow> = st
        .chats
        .iter()
        .filter(|c| c.archived == archived_filter)
        .filter(|c| !unread_only || c.unread || c.unread_count > 0)
        .take(limit)
        .cloned()
        .collect();
    RawHttpResponse::json(200, &serde_json::to_value(rows).unwrap())
}

fn handle_messages(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let Some(chat) = req.query("chat") else {
        return RawHttpResponse::json(400, &json!({"error": "missing chat"}));
    };
    let limit = req
        .query("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(80)
        .min(500);
    let before = req.query("before").filter(|s| !s.is_empty());

    let st = state.lock().unwrap();
    if let Some(fault) = st.messages_faults.get(chat) {
        match fault {
            MessagesFault::HttpStatus(code) => {
                return RawHttpResponse::json(*code, &json!({"error": "messages fault"}));
            }
            MessagesFault::SameAccountLidAlias { lid_chat } => {
                let row = HubMessageRow::simple(
                    lid_chat,
                    "M-LID-1",
                    "Ada",
                    "Alias message from LID",
                    "2026-11-26T11:00:00Z",
                    false,
                );
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::CrossAccountLidMismatch { wrong_chat } => {
                let row = HubMessageRow::simple(
                    wrong_chat,
                    "M-LID-WRONG",
                    "Mallory",
                    "Cross-account LID leak",
                    "2026-11-26T11:01:00Z",
                    false,
                );
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::GroupMismatch { group_chat } => {
                let row = HubMessageRow::simple(
                    group_chat,
                    "M-GRP-WRONG",
                    "Bob",
                    "Wrong group chat",
                    "2026-11-26T11:02:00Z",
                    false,
                );
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::SlackAttachmentsAndReactions => {
                let mut row = HubMessageRow::simple(
                    chat,
                    "1700000005.000100",
                    "Grace",
                    "Shipped the GIF",
                    "2026-11-26T11:05:00Z",
                    false,
                );
                row.attachments = Some(vec![json!({
                    "image_url": "https://media1.giphy.com/media/abc123/giphy.gif",
                    "service_name": "Giphy"
                })]);
                row.reactions = Some(vec![json!({
                    "name": "rocket",
                    "count": 2,
                    "users": ["U1", "U2"]
                })]);
                row.LocalPath = Some("/srv/hub/private/secret.png".into());
                row.local_path = Some("/srv/hub/private/secret.png".into());
                let mut emoji_paths = HashMap::new();
                emoji_paths.insert("shipit".into(), "/srv/hub/emoji/shipit.png".into());
                row.SlackEmojiPaths = Some(emoji_paths);
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::LeakLocalPaths => {
                let mut row = HubMessageRow::simple(
                    chat,
                    "M-LEAK-1",
                    "Hub",
                    "Contains local path",
                    "2026-11-26T11:06:00Z",
                    false,
                );
                row.LocalPath = Some("/var/lib/hub/media/foo.jpg".into());
                row.local_path = Some("/var/lib/hub/media/foo.jpg".into());
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::ProtocolStub => {
                let row = HubMessageRow::simple(
                    chat,
                    "M-STUB-1",
                    "System",
                    "(message)",
                    "2026-11-26T11:07:00Z",
                    false,
                );
                return RawHttpResponse::json(200, &serde_json::to_value(vec![row]).unwrap());
            }
            MessagesFault::DuplicateMsgId => {
                let row = HubMessageRow::simple(
                    chat,
                    "M-DUP-1",
                    "Ada",
                    "Duplicate ID",
                    "2026-11-26T11:08:00Z",
                    false,
                );
                return RawHttpResponse::json(
                    200,
                    &serde_json::to_value(vec![row.clone(), row]).unwrap(),
                );
            }
            MessagesFault::Overflow501 => {
                let rows: Vec<HubMessageRow> = (0..501)
                    .map(|i| {
                        HubMessageRow::simple(
                            chat,
                            format!("M-OVF-{i:04}"),
                            "Ada",
                            format!("Overflow msg {i}"),
                            format!("2026-11-26T10:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                            false,
                        )
                    })
                    .collect();
                return RawHttpResponse::json(200, &serde_json::to_value(rows).unwrap());
            }
        }
    }

    let mut msgs = st.messages.get(chat).cloned().unwrap_or_default();
    // Sort newest-first by (Timestamp DESC, MsgID DESC)
    msgs.sort_by(|a, b| {
        b.Timestamp
            .cmp(&a.Timestamp)
            .then_with(|| b.MsgID.cmp(&a.MsgID))
    });

    if let Some(cursor) = before {
        let (before_id, before_ts) = match cursor.split_once(':') {
            Some((id, ts)) => (Some(id), ts),
            None => (None, cursor),
        };
        msgs.retain(|m| match m.Timestamp.as_str().cmp(before_ts) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Equal => before_id.is_some_and(|id| m.MsgID.as_str() < id),
            std::cmp::Ordering::Greater => false,
        });
    }

    msgs.truncate(limit);
    RawHttpResponse::json(200, &serde_json::to_value(msgs).unwrap())
}

fn handle_send(
    req: &RawHttpRequest,
    multipart: Option<&[MultipartPart]>,
    state: &Arc<Mutex<MockHubState>>,
) -> Option<RawHttpResponse> {
    let fault = state.lock().unwrap().send_faults.pop_front();
    if let Some(f) = fault {
        match f {
            SendFault::DropConnection => return None,
            SendFault::Ambiguous409 { commit_anyway } => {
                if commit_anyway
                    && let Ok(v) = serde_json::from_slice::<Value>(&req.body)
                    && let Some(cid) = v.get("client_id").and_then(Value::as_str)
                {
                    *state
                        .lock()
                        .unwrap()
                        .client_id_deliveries
                        .entry(cid.to_string())
                        .or_insert(0) += 1;
                }
                return Some(RawHttpResponse::json(
                    409,
                    &json!({"error": "ambiguous send state"}),
                ));
            }
            SendFault::TooLarge413 => {
                return Some(RawHttpResponse::json(
                    413,
                    &json!({"error": "file exceeds 64 MiB"}),
                ));
            }
            SendFault::SignalRateLimit429 { retry_after } => {
                return Some(RawHttpResponse::json(
                    429,
                    &json!({"error": "signal rate limited", "retry_after": retry_after}),
                ));
            }
            SendFault::TelegramRateLimit429 { retry_after } => {
                return Some(RawHttpResponse::json(
                    429,
                    &json!({"error": "telegram rate limited", "retry_after": retry_after}),
                ));
            }
            SendFault::SignalCaptcha403 => {
                return Some(RawHttpResponse::json(
                    403,
                    &json!({"error": "signal captcha required"}),
                ));
            }
            SendFault::Transient500 => {
                return Some(RawHttpResponse::json(
                    500,
                    &json!({"error": "transient server error"}),
                ));
            }
            SendFault::Transient503 => {
                return Some(RawHttpResponse::json(
                    503,
                    &json!({"error": "service unavailable"}),
                ));
            }
            SendFault::Unconfirmed200 {
                bad_client_id,
                bad_status,
            } => {
                return Some(RawHttpResponse::json(
                    200,
                    &json!({
                        "client_id": bad_client_id,
                        "msg_id": "",
                        "ts": "",
                        "status": bad_status
                    }),
                ));
            }
        }
    }

    if let Some(parts) = multipart {
        let mut chat = String::new();
        let mut text = String::new();
        let mut client_id = String::new();
        let mut reply_broadcast = false;
        let mut file_part: Option<&MultipartPart> = None;

        for part in parts {
            match part.name.as_str() {
                "chat" => chat = String::from_utf8_lossy(&part.body).into_owned(),
                "text" => text = String::from_utf8_lossy(&part.body).into_owned(),
                "client_id" => client_id = String::from_utf8_lossy(&part.body).into_owned(),
                "reply_broadcast" => {
                    reply_broadcast = std::str::from_utf8(&part.body) == Ok("true");
                }
                "file" => file_part = Some(part),
                _ => {}
            }
        }

        if reply_broadcast {
            return Some(RawHttpResponse::json(
                400,
                &json!({"error": "reply_broadcast is text-only"}),
            ));
        }
        let Some(fp) = file_part else {
            return Some(RawHttpResponse::json(
                400,
                &json!({"error": "missing file part"}),
            ));
        };
        if fp.body.is_empty() {
            return Some(RawHttpResponse::json(
                400,
                &json!({"error": "empty file part"}),
            ));
        }
        let max_bytes = state.lock().unwrap().max_multipart_bytes;
        if fp.body.len() > max_bytes {
            return Some(RawHttpResponse::json(
                413,
                &json!({"error": "file exceeds max bytes"}),
            ));
        }
        if chat.is_empty() || chat.len() > 191 || client_id.is_empty() || client_id.len() > 128 {
            return Some(RawHttpResponse::json(
                400,
                &json!({"error": "invalid multipart metadata"}),
            ));
        }

        let reply = record_idempotent_send(state, &chat, &text, &client_id);
        return Some(RawHttpResponse::json(200, &reply));
    }

    let Ok(payload) = serde_json::from_slice::<Value>(&req.body) else {
        return Some(RawHttpResponse::json(
            400,
            &json!({"error": "invalid json"}),
        ));
    };
    let chat = payload.get("chat").and_then(Value::as_str).unwrap_or("");
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    let client_id = payload
        .get("client_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reply_to = payload
        .get("reply_to")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reply_broadcast = payload
        .get("reply_broadcast")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if chat.is_empty() || chat.len() > 191 {
        return Some(RawHttpResponse::json(
            400,
            &json!({"error": "invalid chat id"}),
        ));
    }
    if client_id.is_empty() || client_id.len() > 128 {
        return Some(RawHttpResponse::json(
            400,
            &json!({"error": "invalid client_id"}),
        ));
    }
    if text.len() > 4095 {
        return Some(RawHttpResponse::json(
            400,
            &json!({"error": "text exceeds 4095 bytes"}),
        ));
    }
    if reply_broadcast && reply_to.is_empty() {
        return Some(RawHttpResponse::json(
            400,
            &json!({"error": "reply_broadcast requires reply_to"}),
        ));
    }

    let reply = record_idempotent_send(state, chat, text, client_id);
    Some(RawHttpResponse::json(200, &reply))
}

fn record_idempotent_send(
    state: &Arc<Mutex<MockHubState>>,
    chat: &str,
    text: &str,
    client_id: &str,
) -> Value {
    let mut st = state.lock().unwrap();
    if let Some(existing) = st.client_id_replies.get(client_id) {
        return existing.clone();
    }
    let seq = st.next_msg_seq;
    st.next_msg_seq += 1;
    let msg_id = format!("HUB-MSG-{seq:06}");
    let ts = format!("2026-11-26T12:00:{:02}Z", seq % 60);
    let reply = json!({
        "client_id": client_id,
        "msg_id": msg_id,
        "ts": ts,
        "status": "sent"
    });
    st.client_id_replies
        .insert(client_id.to_string(), reply.clone());
    *st.client_id_deliveries
        .entry(client_id.to_string())
        .or_insert(0) += 1;
    st.messages
        .entry(chat.to_string())
        .or_default()
        .push(HubMessageRow::simple(chat, msg_id, "Me", text, ts, true));
    reply
}

fn handle_crash_report(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let mut st = state.lock().unwrap();
    if let Some(fault) = st.crash_report_faults.pop_front() {
        return RawHttpResponse::json(fault, &json!({"error": "crash report fault"}));
    }
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let report_id = v.get("report_id").and_then(Value::as_str).unwrap_or("");
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
    if report_id.is_empty() {
        return RawHttpResponse::json(400, &json!({"error": "missing report_id"}));
    }
    if !matches!(kind, "panic" | "native_exception" | "sudden_exit") {
        return RawHttpResponse::json(400, &json!({"error": "unknown kind"}));
    }
    st.crash_report_ids.push(report_id.to_owned());
    RawHttpResponse::json(200, &json!({"ok": true}))
}

fn handle_mark_read(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    if let Some(fault) = state.lock().unwrap().mark_read_faults.pop_front() {
        return RawHttpResponse::json(fault, &json!({"error": "mark-read fault"}));
    }
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let chat = v.get("chat").and_then(Value::as_str).unwrap_or("");
    if chat.is_empty() {
        return RawHttpResponse::json(400, &json!({"error": "missing chat"}));
    }
    let mut st = state.lock().unwrap();
    for c in &mut st.chats {
        if c.jid == chat {
            c.unread_count = 0;
            c.unread = false;
        }
    }
    RawHttpResponse::json(200, &json!({"ok": true}))
}

fn is_whatsapp_handle(chat: &str) -> bool {
    chat.starts_with("whatsapp:")
        || chat.ends_with("@s.whatsapp.net")
        || chat.ends_with("@g.us")
        || chat.ends_with("@lid")
}

fn handle_archive(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let chat = v.get("chat").and_then(Value::as_str).unwrap_or("");
    let archived = v.get("archived").and_then(Value::as_bool).unwrap_or(true);
    let mut st = state.lock().unwrap();
    if let Some(&status) = st.archive_overrides.get(chat) {
        return RawHttpResponse::json(status, &json!({"error": "archive override"}));
    }
    if !is_whatsapp_handle(chat) {
        return RawHttpResponse::json(502, &json!({"error": "archive unsupported"}));
    }
    for c in &mut st.chats {
        if c.jid == chat {
            c.archived = archived;
        }
    }
    RawHttpResponse::json(200, &json!({"ok": true, "archived": archived}))
}

fn handle_archive_batch(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let Some(chats) = v.get("chats").and_then(Value::as_array) else {
        return RawHttpResponse::json(400, &json!({"error": "missing chats"}));
    };
    if chats.is_empty() {
        return RawHttpResponse::json(400, &json!({"error": "empty chats"}));
    }
    let archived = v.get("archived").and_then(Value::as_bool).unwrap_or(true);
    let mut st = state.lock().unwrap();
    let fault = st.archive_batch_fault.clone();

    if let Some(ArchiveBatchFault::HttpStatus(code)) = fault {
        return RawHttpResponse::json(code, &json!({"error": "archive-batch fault"}));
    }
    if let Some(ArchiveBatchFault::MismatchedCount) = fault {
        return RawHttpResponse::json(200, &json!({"results": []}));
    }

    let failed_set = match fault {
        Some(ArchiveBatchFault::PartialFailure(set)) => set,
        _ => HashSet::new(),
    };

    let mut results = Vec::with_capacity(chats.len());
    for entry in chats {
        let handle = entry.get("chat").and_then(Value::as_str).unwrap_or("");
        if failed_set.contains(handle) {
            results.push(json!({
                "chat": handle,
                "ok": false,
                "status": 500,
                "error": "upstream error"
            }));
        } else {
            for c in &mut st.chats {
                if c.jid == handle {
                    c.archived = archived;
                }
            }
            results.push(json!({
                "chat": handle,
                "ok": true
            }));
        }
    }

    RawHttpResponse::json(200, &json!({"results": results}))
}

fn handle_react(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    if let Some(code) = state.lock().unwrap().react_faults.pop_front() {
        return RawHttpResponse::json(code, &json!({"error": "react fault"}));
    }
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let client_id = v.get("client_id").and_then(Value::as_str).unwrap_or("");
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("");
    let chat = v.get("chat").and_then(Value::as_str).unwrap_or("");
    if chat.is_empty() || msg.is_empty() || client_id.is_empty() {
        return RawHttpResponse::json(400, &json!({"error": "invalid react payload"}));
    }
    RawHttpResponse::json(
        200,
        &json!({
            "client_id": client_id,
            "msg_id": msg,
            "ts": "2026-11-26T12:01:00Z",
            "status": "sent"
        }),
    )
}

fn handle_delete(req: &RawHttpRequest, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    if let Some(code) = state.lock().unwrap().delete_faults.pop_front() {
        return RawHttpResponse::json(code, &json!({"error": "delete fault"}));
    }
    let Ok(v) = serde_json::from_slice::<Value>(&req.body) else {
        return RawHttpResponse::json(400, &json!({"error": "invalid json"}));
    };
    let chat = v.get("chat").and_then(Value::as_str).unwrap_or("");
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("");
    let mut st = state.lock().unwrap();
    if let Some(list) = st.messages.get_mut(chat) {
        for m in list {
            if m.MsgID == msg {
                m.Revoked = Some(true);
                m.Text = "Message deleted".into();
            }
        }
    }
    RawHttpResponse::json(200, &json!({"ok": true}))
}

fn handle_media(path: &str, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let rest = path.trim_start_matches("/v1/wazig/media/");
    let Some((raw_chat, raw_msg)) = rest.split_once('/') else {
        return RawHttpResponse::json(400, &json!({"error": "missing chat/msg"}));
    };
    let Ok(chat) = percent_decode(raw_chat) else {
        return RawHttpResponse::json(400, &json!({"error": "bad chat encoding"}));
    };
    let Ok(msg) = percent_decode(raw_msg) else {
        return RawHttpResponse::json(400, &json!({"error": "bad msg encoding"}));
    };

    let mut st = state.lock().unwrap();
    let Some(behavior) = st.media.get_mut(&(chat, msg)) else {
        return RawHttpResponse::json(404, &json!({"error": "media not found"}));
    };

    match behavior {
        MediaBehavior::Ready {
            bytes,
            content_type,
        } => RawHttpResponse::bytes(200, content_type, bytes.clone()),
        MediaBehavior::PendingThenReady {
            remaining_202s,
            bytes,
            content_type,
        } => {
            if *remaining_202s > 0 {
                *remaining_202s -= 1;
                RawHttpResponse::json(202, &json!({"status": "pending"}))
            } else {
                RawHttpResponse::bytes(200, content_type, bytes.clone())
            }
        }
        MediaBehavior::Partial206 {
            bytes,
            content_type,
        } => {
            let content_range = if bytes.is_empty() {
                "bytes */0".to_string()
            } else {
                format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len() * 2)
            };
            RawHttpResponse::bytes(206, content_type, bytes.clone())
                .with_header("Content-Range", content_range)
        }
        MediaBehavior::NotFound404 => RawHttpResponse::json(404, &json!({"error": "not found"})),
        MediaBehavior::Gone410 => RawHttpResponse::json(410, &json!({"error": "gone"})),
        MediaBehavior::RateLimit429 => RawHttpResponse::json(429, &json!({"error": "rate limit"})),
        MediaBehavior::ServerError500 => {
            RawHttpResponse::json(500, &json!({"error": "server error"}))
        }
    }
}

fn handle_avatar(path: &str, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let raw_chat = path.trim_start_matches("/v1/wazig/avatar/");
    let Ok(chat) = percent_decode(raw_chat) else {
        return RawHttpResponse::json(400, &json!({"error": "bad chat encoding"}));
    };
    let st = state.lock().unwrap();
    match st.avatars.get(&chat) {
        Some(Ok(bytes)) => RawHttpResponse::bytes(200, "image/png", bytes.clone()),
        Some(Err(code)) => RawHttpResponse::json(*code, &json!({"error": "avatar error"})),
        None => {
            if chat.starts_with("slack:") || chat.starts_with("signal:") {
                RawHttpResponse::json(502, &json!({"error": "avatar unsupported"}))
            } else {
                RawHttpResponse::json(404, &json!({"error": "avatar not found"}))
            }
        }
    }
}

fn handle_workspace_icon(path: &str, state: &Arc<Mutex<MockHubState>>) -> RawHttpResponse {
    let raw_team = path.trim_start_matches("/v1/wazig/workspace-icon/");
    let Ok(team) = percent_decode(raw_team) else {
        return RawHttpResponse::json(400, &json!({"error": "bad team encoding"}));
    };
    if team.is_empty() || team.contains(':') || team == "." || team == ".." {
        return RawHttpResponse::json(400, &json!({"error": "invalid team"}));
    }
    let st = state.lock().unwrap();
    match st.workspace_icons.get(&team) {
        Some(Ok(bytes)) => RawHttpResponse::bytes(200, "image/png", bytes.clone()),
        Some(Err(code)) => RawHttpResponse::json(*code, &json!({"error": "icon error"})),
        None => RawHttpResponse::json(404, &json!({"error": "icon not found"})),
    }
}

fn handle_slack_emoji_index(
    req: &RawHttpRequest,
    state: &Arc<Mutex<MockHubState>>,
) -> RawHttpResponse {
    let team = req.query("account").unwrap_or("");
    let st = state.lock().unwrap();
    match st.slack_emoji_index.get(team) {
        Some(val) => RawHttpResponse::json(200, val),
        None => RawHttpResponse::json(200, &json!({"emoji": {}})),
    }
}

fn handle_slack_emoji_media(
    req: &RawHttpRequest,
    state: &Arc<Mutex<MockHubState>>,
) -> RawHttpResponse {
    let team = req.query("account").unwrap_or("");
    let name = req.query("name").unwrap_or("");
    let st = state.lock().unwrap();
    match st
        .slack_emoji_media
        .get(&(team.to_string(), name.to_string()))
    {
        Some(Ok(bytes)) => RawHttpResponse::bytes(200, "image/png", bytes.clone()),
        Some(Err(code)) => RawHttpResponse::json(*code, &json!({"error": "emoji media error"})),
        None => {
            if name == "default_ok" {
                RawHttpResponse::bytes(200, "image/png", minimal_png_1x1())
            } else {
                RawHttpResponse::json(404, &json!({"error": "emoji not found"}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{LoopbackHttpClient, ParsedSseFrame};
    use postbox_hub::{events_request, media_request};

    fn sse_event(id: u64) -> SseEvent {
        SseEvent {
            id,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:00Z".into(),
            kind: "message".into(),
        }
    }

    #[test]
    fn sse_live_cursor_never_drops_events_pushed_during_handshake() {
        let client = LoopbackHttpClient::default();

        // 20 zero-initial trials
        for trial in 0..20u64 {
            let hub = MockHubServer::start("tok-race-zero");
            let mut sse = client
                .open_sse(&hub.endpoint(), &hub.token(), &events_request(None))
                .expect("open sse");
            let live_id = 100 + trial;
            hub.push_sse_event(sse_event(live_id));
            let frame = sse
                .next_frame(Duration::from_millis(500))
                .expect("read live sse event")
                .expect("non-eof frame");
            match frame {
                ParsedSseFrame::Event(ev) => assert_eq!(ev.id, live_id),
                other => panic!("expected Event({live_id}), got {other:?}"),
            }
        }

        // 20 50-initial-event trials
        for trial in 0..20u64 {
            let hub = MockHubServer::start("tok-race-50");
            for id in 1..=50u64 {
                hub.push_sse_event(sse_event(id));
            }
            let mut sse = client
                .open_sse(&hub.endpoint(), &hub.token(), &events_request(None))
                .expect("open sse with 50 initial events");
            let live_id = 51 + trial;
            hub.push_sse_event(sse_event(live_id));

            let mut received = Vec::with_capacity(51);
            for _ in 0..51 {
                let frame = sse
                    .next_frame(Duration::from_millis(500))
                    .expect("read sse event")
                    .expect("non-eof frame");
                match frame {
                    ParsedSseFrame::Event(ev) => received.push(ev.id),
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
            assert_eq!(received.len(), 51);
            assert_eq!(received[0], 1);
            assert_eq!(received[49], 50);
            assert_eq!(received[50], live_id);
        }
    }

    #[test]
    fn partial_206_content_range_uses_inclusive_end_byte() {
        let hub = MockHubServer::start("tok-206");
        let handle = "personal:1@s.whatsapp.net";
        hub.set_media(
            handle,
            "M-4B",
            MediaBehavior::Partial206 {
                bytes: vec![1, 2, 3, 4],
                content_type: "image/png".into(),
            },
        );
        hub.set_media(
            handle,
            "M-EMPTY",
            MediaBehavior::Partial206 {
                bytes: Vec::new(),
                content_type: "image/png".into(),
            },
        );

        let client = LoopbackHttpClient::default();
        let resp = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(handle, Some("M-4B")).unwrap(),
            )
            .expect("media 206 4B");
        assert_eq!(resp.status, 206);
        assert_eq!(resp.header("content-range"), Some("bytes 0-3/8"));

        let empty = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(handle, Some("M-EMPTY")).unwrap(),
            )
            .expect("media 206 empty");
        assert_eq!(empty.status, 206);
        assert_eq!(empty.header("content-range"), Some("bytes */0"));
    }
}
