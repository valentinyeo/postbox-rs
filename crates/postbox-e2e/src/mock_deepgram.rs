#![forbid(unsafe_code)]

use crate::wire::{
    DEFAULT_MAX_BODY_BYTES, LoopbackServer, RawHttpRequest, RawHttpResponse, WsFrame, WsOpcode,
    read_http_request, sha256_hex, try_decode_ws_frame, write_ws_frame, ws_accept_key,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RecordedDeepgramRest {
    pub path: String,
    pub query_params: Vec<(String, String)>,
    pub authorization: Option<String>,
    pub content_type: Option<String>,
    pub audio_len: usize,
    pub audio_sha256: String,
}

#[derive(Debug, Clone)]
pub struct RecordedDeepgramWs {
    pub path: String,
    pub query_params: Vec<(String, String)>,
    pub authorization: Option<String>,
    pub binary_frame_count: usize,
    pub pcm_bytes_received: usize,
    pub close_stream_received: bool,
    pub client_close_code: Option<u16>,
}

#[derive(Debug, Clone)]
pub enum DeepgramRestReply {
    Transcript {
        text: String,
        detected_language: Option<String>,
        confidence: f64,
    },
    NoSpeech,
    BadAudio400,
    Unauthorized401,
    PaymentRequired402,
    RateLimited429,
    Unavailable503,
}

#[derive(Debug, Clone)]
pub enum DeepgramRestMode {
    ScriptedQueue(VecDeque<DeepgramRestReply>),
    SmartLanguageRouter {
        en_text: String,
        en_conf: f64,
        de_text: String,
        de_conf: f64,
        auto_detected_lang: String,
        auto_text: String,
        multi_text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsTrigger {
    OnConnect,
    AfterBinaryFrames(usize),
    AfterPcmBytes(usize),
    OnCloseStream,
}

#[derive(Debug, Clone)]
pub enum WsServerAction {
    SendResults {
        is_final: bool,
        start: f64,
        duration: f64,
        transcript: String,
        word_languages: Vec<String>,
        languages: Vec<String>,
    },
    SendError {
        message: String,
    },
    SendMetadata,
    SendClose(u16),
    AbruptDrop,
    DelayMs(u64),
}

#[derive(Debug, Clone)]
pub struct WsScriptStep {
    pub trigger: WsTrigger,
    pub action: WsServerAction,
}

struct MockDeepgramState {
    expected_token: String,
    fail_once_401_then_token: Option<String>,
    rest_mode: DeepgramRestMode,
    ws_handshake_fault: Option<u16>,
    ws_script: Vec<WsScriptStep>,
    recorded_rest: Vec<RecordedDeepgramRest>,
    recorded_ws: Vec<RecordedDeepgramWs>,
}

impl MockDeepgramState {
    fn new(token: String) -> Self {
        Self {
            expected_token: token,
            fail_once_401_then_token: None,
            rest_mode: DeepgramRestMode::SmartLanguageRouter {
                en_text: "Hello from Postbox voice note.".into(),
                en_conf: 0.96,
                de_text: "Hallo aus der Postbox Sprachnachricht.".into(),
                de_conf: 0.95,
                auto_detected_lang: "en".into(),
                auto_text: "Auto detected English transcript.".into(),
                multi_text: "Multilingual fallback transcript.".into(),
            },
            ws_handshake_fault: None,
            ws_script: Vec::new(),
            recorded_rest: Vec::new(),
            recorded_ws: Vec::new(),
        }
    }
}

pub struct MockDeepgramServer {
    server: LoopbackServer,
    state: Arc<Mutex<MockDeepgramState>>,
}

impl MockDeepgramServer {
    pub fn start(token: impl Into<String>) -> Self {
        let state = Arc::new(Mutex::new(MockDeepgramState::new(token.into())));
        let handler_state = Arc::clone(&state);
        let server =
            LoopbackServer::start("mock-deepgram", Arc::new(|| {}), move |stream, stop| {
                handle_deepgram_connection(stream, stop, &handler_state);
            })
            .expect("bind MockDeepgramServer on 127.0.0.1:0");

        Self { server, state }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    pub fn set_expected_token(&self, token: impl Into<String>) {
        self.state.lock().unwrap().expected_token = token.into();
    }

    pub fn fail_once_with_401_then_accept(&self, new_token: impl Into<String>) {
        self.state.lock().unwrap().fail_once_401_then_token = Some(new_token.into());
    }

    pub fn set_rest_mode(&self, mode: DeepgramRestMode) {
        self.state.lock().unwrap().rest_mode = mode;
    }

    pub fn push_rest_reply(&self, reply: DeepgramRestReply) {
        let mut st = self.state.lock().unwrap();
        match &mut st.rest_mode {
            DeepgramRestMode::ScriptedQueue(q) => q.push_back(reply),
            _ => {
                let mut q = VecDeque::new();
                q.push_back(reply);
                st.rest_mode = DeepgramRestMode::ScriptedQueue(q);
            }
        }
    }

    pub fn set_ws_handshake_fault(&self, status: Option<u16>) {
        self.state.lock().unwrap().ws_handshake_fault = status;
    }

    pub fn set_ws_script(&self, steps: Vec<WsScriptStep>) {
        self.state.lock().unwrap().ws_script = steps;
    }

    pub fn recorded_rest(&self) -> Vec<RecordedDeepgramRest> {
        self.state.lock().unwrap().recorded_rest.clone()
    }

    pub fn rest_call_count(&self) -> usize {
        self.state.lock().unwrap().recorded_rest.len()
    }

    pub fn recorded_ws(&self) -> Vec<RecordedDeepgramWs> {
        self.state.lock().unwrap().recorded_ws.clone()
    }
}

fn check_token_auth(req: &RawHttpRequest, state: &Arc<Mutex<MockDeepgramState>>) -> bool {
    let mut st = state.lock().unwrap();
    if let Some(next_token) = st.fail_once_401_then_token.take() {
        st.expected_token = next_token;
        return false;
    }
    let expected = format!("Token {}", st.expected_token);
    req.header("authorization") == Some(expected.as_str())
}

fn handle_deepgram_connection(
    mut stream: TcpStream,
    shutdown: Arc<AtomicBool>,
    state: &Arc<Mutex<MockDeepgramState>>,
) {
    let Ok(req) = read_http_request(&mut stream, DEFAULT_MAX_BODY_BYTES) else {
        return;
    };

    let is_ws = req
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_ws {
        handle_deepgram_ws(stream, &req, shutdown, state);
    } else {
        handle_deepgram_rest(stream, &req, state);
    }
}

fn handle_deepgram_rest(
    mut stream: TcpStream,
    req: &RawHttpRequest,
    state: &Arc<Mutex<MockDeepgramState>>,
) {
    {
        let mut st = state.lock().unwrap();
        st.recorded_rest.push(RecordedDeepgramRest {
            path: req.path.clone(),
            query_params: req.query_params.clone(),
            authorization: req.header("authorization").map(ToOwned::to_owned),
            content_type: req.header("content-type").map(ToOwned::to_owned),
            audio_len: req.body.len(),
            audio_sha256: sha256_hex(&req.body),
        });
    }

    if !check_token_auth(req, state) {
        let _ =
            RawHttpResponse::json(401, &json!({"err_code": "INVALID_AUTH"})).write_to(&mut stream);
        return;
    }

    let reply = {
        let mut st = state.lock().unwrap();
        match &mut st.rest_mode {
            DeepgramRestMode::ScriptedQueue(q) => {
                q.pop_front().unwrap_or(DeepgramRestReply::NoSpeech)
            }
            DeepgramRestMode::SmartLanguageRouter {
                en_text,
                en_conf,
                de_text,
                de_conf,
                auto_detected_lang,
                auto_text,
                multi_text,
            } => match req.query("language") {
                Some("en") => DeepgramRestReply::Transcript {
                    text: en_text.clone(),
                    detected_language: Some("en".into()),
                    confidence: *en_conf,
                },
                Some("de") => DeepgramRestReply::Transcript {
                    text: de_text.clone(),
                    detected_language: Some("de".into()),
                    confidence: *de_conf,
                },
                Some("multi") => DeepgramRestReply::Transcript {
                    text: multi_text.clone(),
                    detected_language: Some("de".into()),
                    confidence: 0.90,
                },
                _ => DeepgramRestReply::Transcript {
                    text: auto_text.clone(),
                    detected_language: Some(auto_detected_lang.clone()),
                    confidence: 0.94,
                },
            },
        }
    };

    let resp = match reply {
        DeepgramRestReply::Transcript {
            text,
            detected_language,
            confidence,
        } => {
            let mut channel = json!({
                "alternatives": [{
                    "transcript": text,
                    "confidence": confidence
                }]
            });
            if let Some(lang) = detected_language {
                channel["detected_language"] = Value::String(lang);
            }
            RawHttpResponse::json(200, &json!({"results": {"channels": [channel]}}))
        }
        DeepgramRestReply::NoSpeech => RawHttpResponse::json(
            200,
            &json!({"results": {"channels": [{"alternatives": [{"transcript": "", "confidence": 0.0}]}]}}),
        ),
        DeepgramRestReply::BadAudio400 => {
            RawHttpResponse::json(400, &json!({"err_code": "BAD_REQUEST"}))
        }
        DeepgramRestReply::Unauthorized401 => {
            RawHttpResponse::json(401, &json!({"err_code": "INVALID_AUTH"}))
        }
        DeepgramRestReply::PaymentRequired402 => {
            RawHttpResponse::json(402, &json!({"err_code": "INSUFFICIENT_FUNDS"}))
        }
        DeepgramRestReply::RateLimited429 => {
            RawHttpResponse::json(429, &json!({"err_code": "RATE_LIMIT"}))
        }
        DeepgramRestReply::Unavailable503 => {
            RawHttpResponse::json(503, &json!({"err_code": "UNAVAILABLE"}))
        }
    };

    let _ = resp.write_to(&mut stream);
}

fn handle_deepgram_ws(
    mut stream: TcpStream,
    req: &RawHttpRequest,
    shutdown: Arc<AtomicBool>,
    state: &Arc<Mutex<MockDeepgramState>>,
) {
    let handshake_fault = state.lock().unwrap().ws_handshake_fault;
    if let Some(code) = handshake_fault {
        let _ = RawHttpResponse::json(code, &json!({"error": "ws handshake fault"}))
            .write_to(&mut stream);
        return;
    }

    if !check_token_auth(req, state) {
        let _ = RawHttpResponse::json(401, &json!({"error": "unauthorized"})).write_to(&mut stream);
        return;
    }

    let Some(ws_key) = req.header("sec-websocket-key") else {
        let _ = RawHttpResponse::text(400, "missing Sec-WebSocket-Key").write_to(&mut stream);
        return;
    };
    if req.header("sec-websocket-version") != Some("13") {
        let _ = RawHttpResponse::text(400, "bad Sec-WebSocket-Version").write_to(&mut stream);
        return;
    }

    let accept = ws_accept_key(ws_key);
    let upgrade_resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(upgrade_resp.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }

    let script = state.lock().unwrap().ws_script.clone();
    let mut fired = vec![false; script.len()];
    let mut rec = RecordedDeepgramWs {
        path: req.path.clone(),
        query_params: req.query_params.clone(),
        authorization: req.header("authorization").map(ToOwned::to_owned),
        binary_frame_count: 0,
        pcm_bytes_received: 0,
        close_stream_received: false,
        client_close_code: None,
    };

    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));

    if run_ws_triggers(&mut stream, &script, &mut fired, &rec, true) {
        state.lock().unwrap().recorded_ws.push(rec);
        return;
    }

    let mut read_buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        match try_decode_ws_frame(&mut read_buf, true) {
            Ok(Some(frame)) => {
                match frame.opcode {
                    WsOpcode::Binary => {
                        rec.binary_frame_count += 1;
                        rec.pcm_bytes_received += frame.payload.len();
                        if run_ws_triggers(&mut stream, &script, &mut fired, &rec, false) {
                            break;
                        }
                    }
                    WsOpcode::Text => {
                        if let Ok(v) = serde_json::from_slice::<Value>(&frame.payload)
                            && v.get("type").and_then(Value::as_str) == Some("CloseStream")
                        {
                            rec.close_stream_received = true;
                            let closed =
                                run_ws_triggers(&mut stream, &script, &mut fired, &rec, false);
                            if !closed {
                                let close_frame = WsFrame {
                                    fin: true,
                                    opcode: WsOpcode::Close,
                                    masked: false,
                                    payload: 1000u16.to_be_bytes().to_vec(),
                                };
                                let _ = write_ws_frame(&mut stream, &close_frame, None);
                            }
                            break;
                        }
                    }
                    WsOpcode::Close => {
                        if frame.payload.len() >= 2 {
                            rec.client_close_code =
                                Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]));
                        }
                        let reply = WsFrame {
                            fin: true,
                            opcode: WsOpcode::Close,
                            masked: false,
                            payload: frame.payload,
                        };
                        let _ = write_ws_frame(&mut stream, &reply, None);
                        break;
                    }
                    WsOpcode::Ping => {
                        let pong = WsFrame {
                            fin: true,
                            opcode: WsOpcode::Pong,
                            masked: false,
                            payload: frame.payload,
                        };
                        let _ = write_ws_frame(&mut stream, &pong, None);
                    }
                    WsOpcode::Pong | WsOpcode::Continuation => {}
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => break,
        }

        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => read_buf.extend_from_slice(&tmp[..n]),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => break,
        }
    }

    state.lock().unwrap().recorded_ws.push(rec);
}

fn run_ws_triggers(
    stream: &mut TcpStream,
    script: &[WsScriptStep],
    fired: &mut [bool],
    rec: &RecordedDeepgramWs,
    on_connect: bool,
) -> bool {
    for (idx, step) in script.iter().enumerate() {
        if fired[idx] {
            continue;
        }
        let should_fire = match step.trigger {
            WsTrigger::OnConnect => on_connect,
            WsTrigger::AfterBinaryFrames(n) => rec.binary_frame_count >= n,
            WsTrigger::AfterPcmBytes(b) => rec.pcm_bytes_received >= b,
            WsTrigger::OnCloseStream => rec.close_stream_received,
        };
        if !should_fire {
            continue;
        }
        fired[idx] = true;
        match &step.action {
            WsServerAction::SendResults {
                is_final,
                start,
                duration,
                transcript,
                word_languages,
                languages,
            } => {
                let words: Vec<Value> = transcript
                    .split_whitespace()
                    .enumerate()
                    .map(|(i, w)| {
                        let lang = word_languages
                            .get(i)
                            .or_else(|| word_languages.last())
                            .cloned()
                            .unwrap_or_else(|| "en".into());
                        json!({
                            "word": w,
                            "start": start + (i as f64) * 0.1,
                            "end": start + (i as f64 + 1.0) * 0.1,
                            "language": lang
                        })
                    })
                    .collect();
                let payload = json!({
                    "type": "Results",
                    "is_final": is_final,
                    "start": start,
                    "duration": duration,
                    "channel": {
                        "alternatives": [{
                            "transcript": transcript,
                            "confidence": 0.96,
                            "words": words,
                            "languages": languages
                        }]
                    }
                });
                let frame = WsFrame {
                    fin: true,
                    opcode: WsOpcode::Text,
                    masked: false,
                    payload: serde_json::to_vec(&payload).unwrap(),
                };
                if write_ws_frame(stream, &frame, None).is_err() {
                    return true;
                }
            }
            WsServerAction::SendError { message } => {
                let payload = json!({
                    "type": "Error",
                    "description": message,
                    "message": message
                });
                let frame = WsFrame {
                    fin: true,
                    opcode: WsOpcode::Text,
                    masked: false,
                    payload: serde_json::to_vec(&payload).unwrap(),
                };
                if write_ws_frame(stream, &frame, None).is_err() {
                    return true;
                }
            }
            WsServerAction::SendMetadata => {
                let payload = json!({"type": "Metadata", "request_id": "mock-dg-1"});
                let frame = WsFrame {
                    fin: true,
                    opcode: WsOpcode::Text,
                    masked: false,
                    payload: serde_json::to_vec(&payload).unwrap(),
                };
                if write_ws_frame(stream, &frame, None).is_err() {
                    return true;
                }
            }
            WsServerAction::SendClose(code) => {
                let frame = WsFrame {
                    fin: true,
                    opcode: WsOpcode::Close,
                    masked: false,
                    payload: code.to_be_bytes().to_vec(),
                };
                let _ = write_ws_frame(stream, &frame, None);
                return true;
            }
            WsServerAction::AbruptDrop => {
                return true;
            }
            WsServerAction::DelayMs(ms) => {
                thread::sleep(Duration::from_millis(*ms));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::LoopbackHttpClient;
    use crate::wire::{base64_encode, read_ws_frame};
    use std::time::Instant;

    #[test]
    fn ws_server_survives_partial_frame_across_50ms_timeout_and_shuts_down_promptly() {
        let server = MockDeepgramServer::start("dg-ws-frag");
        server.set_ws_script(vec![WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.0,
                duration: 0.5,
                transcript: "fragmented ok".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        }]);

        // Perform raw RFC 6455 handshake so we can trickle bytes across the 50 ms server timeout
        let mut raw = TcpStream::connect(server.addr()).expect("connect mock deepgram");
        raw.set_nodelay(true).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let key = base64_encode(b"frag-ws-test-key");
        write!(
            raw,
            "GET /v1/listen?model=nova-3 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nAuthorization: Token dg-ws-frag\r\n\r\n",
            server.port()
        )
        .unwrap();
        raw.flush().unwrap();

        // Read 101 response headers
        let mut resp_buf = Vec::new();
        let mut b = [0u8; 1];
        while !resp_buf.ends_with(b"\r\n\r\n") {
            raw.read_exact(&mut b).unwrap();
            resp_buf.push(b[0]);
        }
        assert!(String::from_utf8_lossy(&resp_buf).starts_with("HTTP/1.1 101"));

        // Build a 100-byte masked binary WS frame and send in 3 pieces across 80 ms pauses
        let frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Binary,
            masked: true,
            payload: vec![0x5Au8; 100],
        };
        let mut encoded = Vec::new();
        write_ws_frame(&mut encoded, &frame, Some([0x11, 0x22, 0x33, 0x44])).unwrap();
        assert_eq!(encoded.len(), 2 + 4 + 100);

        raw.write_all(&encoded[..2]).unwrap();
        raw.flush().unwrap();
        thread::sleep(Duration::from_millis(80));
        raw.write_all(&encoded[2..6]).unwrap();
        raw.flush().unwrap();
        thread::sleep(Duration::from_millis(80));
        raw.write_all(&encoded[6..]).unwrap();
        raw.flush().unwrap();

        let reply = read_ws_frame(&mut raw, false).expect("receive Results text frame");
        assert_eq!(reply.opcode, WsOpcode::Text);
        let v: Value = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(v["type"], "Results");
        assert_eq!(
            v["channel"]["alternatives"][0]["transcript"],
            "fragmented ok"
        );

        // Close first connection cleanly so server records it
        let close_frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Close,
            masked: true,
            payload: 1000u16.to_be_bytes().to_vec(),
        };
        write_ws_frame(&mut raw, &close_frame, Some([1, 2, 3, 4])).unwrap();
        let _ = read_ws_frame(&mut raw, false);
        drop(raw);

        // Wait briefly for recording to land
        let deadline = Instant::now() + Duration::from_secs(1);
        while server.recorded_ws().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let recorded = server.recorded_ws();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary_frame_count, 1);
        assert_eq!(recorded[0].pcm_bytes_received, 100);

        // Open a second idle WS connection and verify drop(server) completes promptly (< 300 ms)
        let client = LoopbackHttpClient::default();
        let _idle_ws = client
            .connect_ws(
                server.addr(),
                "/v1/listen?model=nova-3",
                &[("Authorization", "Token dg-ws-frag")],
            )
            .expect("open idle ws");
        let drop_start = Instant::now();
        drop(server);
        assert!(
            drop_start.elapsed() < Duration::from_millis(300),
            "MockDeepgramServer::drop took {:?}",
            drop_start.elapsed()
        );
    }
}
