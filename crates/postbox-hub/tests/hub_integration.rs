use postbox_core::outbox::{
    OutboxPhase, OutboxRecord, SendOutcome, classify_http_outcome, format_client_id, sha256_bytes,
    verify_attachment_bytes,
};
use postbox_core::{ChatKey, Provider};
use postbox_hub::{
    ArchiveOutcome, AvatarFetchState, Endpoint, HubClient, HubError, MediaFetchState,
    SendFileRequest, SseParser, before_cursor, classify_signal_send_failure,
    classify_telegram_send_failure, is_real_message,
};
use postbox_store::Store;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TEST_TOKEN: &str = "postbox-m2-test-token-0123456789";

#[derive(Default)]
struct MockHubState {
    chats_inbox: Vec<Value>,
    chats_archived: Vec<Value>,
    messages_by_chat: HashMap<String, Vec<Value>>,
    seen_send_client_ids: Vec<String>,
    send_count_by_chat: HashMap<String, usize>,
    send_script: HashMap<String, Vec<(u16, String)>>,
    send_uploads: usize,
    mark_read_calls: Vec<(String, String)>,
    archive_batch_fail_chats: HashSet<String>,
    media_202_remaining: usize,
    media_status_override: Option<u16>,
    sse_last_event_ids_seen: Vec<Option<String>>,
    sse_frames: Vec<Vec<String>>,
    sse_connect_index: usize,
}

async fn start_mock_hub(
    state: Arc<Mutex<MockHubState>>,
) -> (Endpoint, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{}", addr.port()), true).unwrap();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let st = state.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end;
                loop {
                    let n = match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        break;
                    }
                }

                let header_str = String::from_utf8_lossy(&raw[..header_end]).into_owned();
                let mut lines = header_str.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let mut req_parts = req_line.split_whitespace();
                let method = req_parts.next().unwrap_or("");
                let target = req_parts.next().unwrap_or("");

                let mut content_length = 0usize;
                let mut auth_ok = false;
                let mut content_type = String::new();
                let mut last_event_id: Option<String> = None;
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        let kl = k.trim().to_ascii_lowercase();
                        let val = v.trim();
                        match kl.as_str() {
                            "content-length" => {
                                content_length = val.parse().unwrap_or(0);
                            }
                            "authorization" => {
                                auth_ok = val == format!("Bearer {TEST_TOKEN}");
                            }
                            "content-type" => {
                                content_type = val.to_owned();
                            }
                            "last-event-id" => {
                                last_event_id = Some(val.to_owned());
                            }
                            _ => {}
                        }
                    }
                }

                while raw.len() < header_end + content_length {
                    let n = match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&tmp[..n]);
                }
                let body = &raw[header_end..(header_end + content_length).min(raw.len())];

                if !auth_ok {
                    let _ = write_response(
                        &mut stream,
                        401,
                        "application/json",
                        br#"{"error":"unauthorized"}"#,
                    )
                    .await;
                    return;
                }

                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let qparams = parse_query(query);

                if method == "GET" && path == "/v1/wazig/events" {
                    let frames = {
                        let mut g = st.lock().unwrap();
                        g.sse_last_event_ids_seen.push(last_event_id);
                        let idx = g.sse_connect_index;
                        g.sse_connect_index += 1;
                        g.sse_frames.get(idx).cloned().unwrap_or_default()
                    };
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    if stream.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for frame in frames {
                        if stream.write_all(frame.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = stream.flush().await;
                    }
                    return;
                }

                let (status, resp_ct, resp_body): (u16, &str, Vec<u8>) = match (method, path) {
                    ("GET", "/v1/wazig/chats") => {
                        let archived = qparams.get("archived").is_some_and(|v| v == "true");
                        let limit = qparams
                            .get("limit")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(500);
                        let g = st.lock().unwrap();
                        let rows = if archived {
                            &g.chats_archived
                        } else {
                            &g.chats_inbox
                        };
                        let slice: Vec<Value> = rows.iter().take(limit).cloned().collect();
                        (200, "application/json", serde_json::to_vec(&slice).unwrap())
                    }
                    ("GET", "/v1/wazig/messages") => {
                        let chat = qparams.get("chat").cloned().unwrap_or_default();
                        let limit = qparams
                            .get("limit")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(80);
                        let before = qparams.get("before").cloned().unwrap_or_default();
                        let g = st.lock().unwrap();
                        let all = g.messages_by_chat.get(&chat).cloned().unwrap_or_default();
                        let mut filtered: Vec<Value> = if before.is_empty() {
                            all
                        } else if let Some(cur) =
                            postbox_store::MessageCursor::parse_hub_before_cursor(&before)
                        {
                            all.into_iter()
                                .filter(|m| {
                                    let ts = m["Timestamp"].as_str().unwrap_or("");
                                    let id = m["MsgID"].as_str().unwrap_or("");
                                    (ts, id) < (cur.ts.as_str(), cur.msg_id.as_str())
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        filtered.truncate(limit);
                        (
                            200,
                            "application/json",
                            serde_json::to_vec(&filtered).unwrap(),
                        )
                    }
                    ("POST", "/v1/wazig/send") => {
                        if content_type.starts_with("multipart/form-data") {
                            let mut g = st.lock().unwrap();
                            g.send_uploads += 1;
                            let body_str = String::from_utf8_lossy(body);
                            let cid = extract_multipart_field(&body_str, "client_id")
                                .unwrap_or_else(|| "unknown".into());
                            g.seen_send_client_ids.push(cid.clone());
                            let reply = json!({
                                "client_id": cid,
                                "msg_id": format!("srv_file_{}", g.send_uploads),
                                "ts": "2026-11-26T12:00:00Z",
                                "status": "sent"
                            });
                            (200, "application/json", serde_json::to_vec(&reply).unwrap())
                        } else {
                            let val: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                            let chat = val["chat"].as_str().unwrap_or("").to_owned();
                            let cid = val["client_id"].as_str().unwrap_or("").to_owned();
                            let mut g = st.lock().unwrap();
                            g.seen_send_client_ids.push(cid.clone());
                            *g.send_count_by_chat.entry(chat.clone()).or_insert(0) += 1;
                            if let Some(script) = g.send_script.get_mut(&chat)
                                && !script.is_empty()
                            {
                                let (st_code, resp_body) = script.remove(0);
                                (st_code, "application/json", resp_body.into_bytes())
                            } else {
                                let seq = g.seen_send_client_ids.len();
                                let reply = json!({
                                    "client_id": cid,
                                    "msg_id": format!("srv_{seq}"),
                                    "ts": "2026-11-26T12:00:00Z",
                                    "status": "sent"
                                });
                                (200, "application/json", serde_json::to_vec(&reply).unwrap())
                            }
                        }
                    }
                    ("POST", "/v1/wazig/mark-read") => {
                        let val: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                        let chat = val["chat"].as_str().unwrap_or("").to_owned();
                        let mid = val["message_id"].as_str().unwrap_or("").to_owned();
                        st.lock().unwrap().mark_read_calls.push((chat, mid));
                        (200, "application/json", b"{}".to_vec())
                    }
                    ("POST", "/v1/wazig/archive") => {
                        let val: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                        let chat = val["chat"].as_str().unwrap_or("");
                        if chat.starts_with("signal:") || chat.starts_with("telegram:") {
                            (
                                502,
                                "application/json",
                                br#"{"error":"unsupported"}"#.to_vec(),
                            )
                        } else {
                            (200, "application/json", b"{}".to_vec())
                        }
                    }
                    ("POST", "/v1/wazig/archive-batch") => {
                        let val: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                        let g = st.lock().unwrap();
                        let results: Vec<Value> = val["chats"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| {
                                let jid = c["chat"].as_str().unwrap_or("").to_owned();
                                let ok = !g.archive_batch_fail_chats.contains(&jid);
                                json!({ "chat": jid, "ok": ok })
                            })
                            .collect();
                        (
                            200,
                            "application/json",
                            serde_json::to_vec(&json!({ "results": results })).unwrap(),
                        )
                    }
                    ("POST", "/v1/wazig/react") => {
                        let val: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                        let cid = val["client_id"].as_str().unwrap_or("");
                        let reply = json!({
                            "client_id": cid,
                            "msg_id": "react_1",
                            "ts": "2026-11-26T12:00:00Z",
                            "status": "sent"
                        });
                        (200, "application/json", serde_json::to_vec(&reply).unwrap())
                    }
                    ("POST", "/v1/wazig/delete") => (200, "application/json", b"{}".to_vec()),
                    ("GET", p) if p.starts_with("/v1/wazig/media/") => {
                        let mut g = st.lock().unwrap();
                        if let Some(code) = g.media_status_override {
                            (code, "application/octet-stream", b"partial".to_vec())
                        } else if g.media_202_remaining > 0 {
                            g.media_202_remaining -= 1;
                            (202, "application/json", br#"{"status":"pending"}"#.to_vec())
                        } else {
                            (
                                200,
                                "application/octet-stream",
                                b"MEDIA-OK-PAYLOAD".to_vec(),
                            )
                        }
                    }
                    ("GET", p) if p.starts_with("/v1/wazig/avatar/") => {
                        match st.lock().unwrap().media_status_override {
                            Some(202) => {
                                (202, "application/json", br#"{"status":"queued"}"#.to_vec())
                            }
                            Some(code) => (code, "image/png", b"partial".to_vec()),
                            None => (200, "image/png", b"PNG-AVATAR".to_vec()),
                        }
                    }
                    ("GET", p) if p.starts_with("/v1/wazig/workspace-icon/") => {
                        if let Some(code) = st.lock().unwrap().media_status_override {
                            (code, "image/png", b"partial".to_vec())
                        } else {
                            (200, "image/png", b"PNG-ICON".to_vec())
                        }
                    }
                    ("GET", "/v1/wazig/slack/emoji") => (
                        200,
                        "application/json",
                        br#"{"emoji":{"shipit":"https://cdn.example/shipit.png"}}"#.to_vec(),
                    ),
                    ("GET", "/v1/wazig/slack/emoji/media") => {
                        if let Some(code) = st.lock().unwrap().media_status_override {
                            (code, "image/png", b"partial".to_vec())
                        } else {
                            (200, "image/png", b"EMOJI-PNG".to_vec())
                        }
                    }
                    _ => (
                        404,
                        "application/json",
                        br#"{"error":"not_found"}"#.to_vec(),
                    ),
                };

                let _ = write_response(&mut stream, status, resp_ct, &resp_body).await;
            });
        }
    });

    (endpoint, handle)
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn extract_multipart_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("name=\"{field}\"\r\n\r\n");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find("\r\n")?;
    Some(rest[..end].to_owned())
}

#[tokio::test]
async fn real_size_inbox_320_chats_6704_msgs_ingest_and_fts5() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    {
        let mut g = state.lock().unwrap();
        for c in 0..320 {
            let jid = format!("personal:chat_{c}@s.whatsapp.net");
            g.chats_inbox.push(json!({
                "jid": jid,
                "provider": "whatsapp",
                "account": "personal",
                "name": format!("Contact {c}"),
                "last_message_ts": "2026-11-26T12:00:00Z",
                "last_message_preview": "Latest msg",
                "unread": c % 5 == 0,
                "archived": false
            }));
            let count = if c < 304 { 21 } else { 20 }; // 6,704 total messages
            let mut msgs = Vec::with_capacity(count);
            for m in (0..count).rev() {
                msgs.push(json!({
                    "MsgID": format!("m_{c}_{m}"),
                    "ChatJID": jid,
                    "SenderJID": "u1",
                    "SenderName": format!("Sender {c}"),
                    "FromMe": m % 2 == 0,
                    "Timestamp": format!("2026-11-26T10:{:02}:{:02}Z", m / 60, m % 60),
                    "Text": if c == 199 && m == 11 {
                        "postbox_real_corpus_needle_6704".to_owned()
                    } else {
                        format!("Corpus message {m} for chat {c}")
                    }
                }));
            }
            g.messages_by_chat.insert(jid, msgs);
        }
    }

    let (endpoint, server) = start_mock_hub(state).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();

    let chats = client.fetch_archived_and_inbox_chats(500).await.unwrap();
    assert_eq!(chats.len(), 320);

    let mut store = Store::open_in_memory().unwrap();
    let core_chats: Vec<_> = chats.iter().map(|c| c.to_core_chat(false, false)).collect();
    store.upsert_chats_batch(&core_chats, 1_000).unwrap();

    let mut all_core_msgs = Vec::with_capacity(6_704);
    for chat in &chats {
        let fetched = client.fetch_messages(&chat.jid, 80, None).await.unwrap();
        let key = chat.chat_key();
        for m in fetched.iter().filter(|m| is_real_message(m)) {
            all_core_msgs.push(m.to_core_message(&key, chat.is_group));
        }
    }
    assert_eq!(all_core_msgs.len(), 6_704);
    store.upsert_messages_batch(&all_core_msgs).unwrap();

    let hits = store
        .search_messages_fts("postbox_real_corpus_needle_6704", 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].msg_id, "m_199_11");
    server.abort();
}

#[tokio::test]
async fn sse_live_events_last_event_id_resume_and_bad_id_rejection() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    {
        let mut g = state.lock().unwrap();
        g.sse_frames = vec![
            vec![
                ": keepalive\n\n".into(),
                "id: 10\ndata: {\"id\":10,\"kind\":\"msg\",\"chat\":\"c1\"}\n\n".into(),
                // Mismatched id MUST trigger BadEventId and MUST NOT advance last_event_id!
                "id: 99\ndata: {\"id\":100,\"kind\":\"msg\",\"chat\":\"c1\"}\n\n".into(),
            ],
            vec!["id: 11\ndata: {\"id\":11,\"kind\":\"read\",\"chat\":\"c1\"}\n\n".into()],
        ];
    }

    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();
    let mut parser = SseParser::new();

    // First connection receives event 10 then fails on mismatched id 99 != 100.
    let mut stream1 = client.connect_events(parser.last_event_id()).await.unwrap();
    let mut received = Vec::new();
    let mut got_bad_id = false;
    while let Some(chunk) = stream1.next_chunk().await {
        match parser.push_bytes(&chunk.unwrap()) {
            Ok(evs) => received.extend(evs),
            Err(HubError::BadEventId) => {
                got_bad_id = true;
                break;
            }
            Err(other) => panic!("unexpected err: {other}"),
        }
    }
    assert!(got_bad_id);
    assert_eq!(parser.last_event_id(), Some("10"));

    // Second connection resumes with Last-Event-ID: 10 and receives event 11.
    parser.clear_buffer();
    let mut stream2 = client.connect_events(parser.last_event_id()).await.unwrap();
    while let Some(chunk) = stream2.next_chunk().await {
        let evs = parser.push_bytes(&chunk.unwrap()).unwrap();
        received.extend(evs);
    }
    assert_eq!(parser.last_event_id(), Some("11"));
    assert_eq!(
        state.lock().unwrap().sse_last_event_ids_seen,
        vec![None, Some("10".to_owned())]
    );
    server.abort();
}

#[tokio::test]
async fn composite_before_cursor_pagination_and_exhaustion() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    let chat_jid = "personal:tie@s.whatsapp.net";
    {
        let mut g = state.lock().unwrap();
        // Descending order as returned by Hub, with colons inside MsgID and identical timestamps.
        g.messages_by_chat.insert(
            chat_jid.into(),
            vec![
                json!({"MsgID":"personal:M4","ChatJID":chat_jid,"Timestamp":"2026-11-26T10:00:00Z","Text":"4"}),
                json!({"MsgID":"personal:M3","ChatJID":chat_jid,"Timestamp":"2026-11-26T10:00:00Z","Text":"3"}),
                json!({"MsgID":"personal:M2","ChatJID":chat_jid,"Timestamp":"2026-11-26T10:00:00Z","Text":"2"}),
                json!({"MsgID":"personal:M1","ChatJID":chat_jid,"Timestamp":"2026-11-26T09:59:00Z","Text":"1"}),
            ],
        );
    }

    let (endpoint, server) = start_mock_hub(state).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();

    let p1 = client.fetch_messages(chat_jid, 2, None).await.unwrap();
    assert_eq!(
        p1.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["personal:M4", "personal:M3"]
    );
    let cursor1 = before_cursor(&p1[1].msg_id, &p1[1].ts).unwrap();
    let p2 = client
        .fetch_messages(chat_jid, 2, Some(&cursor1))
        .await
        .unwrap();
    assert_eq!(
        p2.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["personal:M2", "personal:M1"]
    );
    let cursor2 = before_cursor(&p2[1].msg_id, &p2[1].ts).unwrap();
    let p3 = client
        .fetch_messages(chat_jid, 2, Some(&cursor2))
        .await
        .unwrap();
    assert!(p3.is_empty());
    server.abort();
}

#[tokio::test]
async fn send_queue_tla_all_six_invariants_and_crash_recovery() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    let chat_jid = "personal:send@s.whatsapp.net";
    {
        let mut g = state.lock().unwrap();
        // 3 transient 500s then 1 final 500 -> exhausts 4 auto-attempts (AutoSendsBounded).
        g.send_script.insert(
            chat_jid.into(),
            vec![
                (500, r#"{"error":"boom1"}"#.into()),
                (500, r#"{"error":"boom2"}"#.into()),
                (500, r#"{"error":"boom3"}"#.into()),
                (500, r#"{"error":"boom4"}"#.into()),
            ],
        );
    }

    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let chat = ChatKey::new(Provider::Whatsapp, "personal", chat_jid);
    let cid = format_client_id([0x42; 16]);
    store
        .enqueue_outbox(&OutboxRecord::new_text(
            cid.clone(),
            chat.clone(),
            "durable message",
            None,
            false,
            1_000,
        ))
        .unwrap();

    // 4 automatic attempts with 3s / 10s / 30s backoff reusing IDENTICAL client_id (NoDouble).
    let mut now = 1_000i64;
    for expected_attempt in 1..=4u8 {
        let claimed = store.claim_next_ready_outbox(now).unwrap().unwrap();
        assert_eq!(claimed.phase, OutboxPhase::Uncertain); // InflightIsUncertain
        assert!(store.claim_next_ready_outbox(now).unwrap().is_none()); // SingleInflight
        let res = client
            .send_text(
                &claimed.chat.id,
                &claimed.text_body,
                None,
                false,
                &claimed.client_id,
            )
            .await;
        assert!(matches!(res, Err(HubError::HttpStatus { status: 500, .. })));
        let classified = classify_http_outcome(Provider::Whatsapp, false, 500, false, "HTTP 500");
        assert_eq!(classified.outcome, SendOutcome::Retryable);
        let next = store
            .mark_outbox_retryable(
                &claimed.client_id,
                claimed.attempt_serial,
                classified.ambiguous,
                "HTTP 500",
                now,
            )
            .unwrap()
            .unwrap();
        assert_eq!(next.attempts, expected_attempt);
        if expected_attempt < 4 {
            assert_eq!(next.phase, OutboxPhase::Ready);
            now = next.not_before_ms;
        } else {
            assert_eq!(next.phase, OutboxPhase::Held); // NoLoss + AutoSendsBounded
            assert!(next.ambiguous);
        }
    }
    // Every automatic attempt reused the exact same client_id.
    assert_eq!(
        state.lock().unwrap().seen_send_client_ids,
        vec![cid.clone(), cid.clone(), cid.clone(), cid.clone()]
    );

    // Explicit UserRetry mints a NEW 32-hex client_id and succeeds!
    let cid2 = format_client_id([0x99; 16]);
    store
        .user_retry_outbox_with_new_client_id(&cid, &cid2, now + 100)
        .unwrap();
    let claimed = store.claim_next_ready_outbox(now + 100).unwrap().unwrap();
    assert_eq!(claimed.client_id, cid2);
    // Simulate crash BEFORE HTTP response arrives -> startup recovery transitions to Held + ambiguous!
    let recovered = store.recover_uncertain_on_startup().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].phase, OutboxPhase::Held);
    assert!(recovered[0].ambiguous);
    assert!(
        store
            .claim_next_ready_outbox(now + 999_999)
            .unwrap()
            .is_none()
    );

    // Signal 429 / 403 and Telegram 429 classifiers verified.
    assert!(
        classify_signal_send_failure(429, r#"{"error":"signal rate limited","retry_after":60}"#)
            .is_some()
    );
    assert!(classify_signal_send_failure(403, r#"{"error":"signal captcha required"}"#).is_some());
    assert!(
        classify_telegram_send_failure(
            429,
            r#"{"error":"telegram rate limited","retry_after":15}"#
        )
        .is_some()
    );
    server.abort();
}

#[tokio::test]
async fn outbox_sha256_attachment_tampering_held_without_upload() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();
    let mut store = Store::open_in_memory().unwrap();

    let tmp_path =
        std::env::temp_dir().join(format!("postbox_tamper_test_{}.bin", std::process::id()));
    let original = b"original-voice-note-bytes";
    let expected_sha = sha256_bytes(original);
    std::fs::write(&tmp_path, b"TAMPERED-VOICE-NOTE-BYTES").unwrap();

    let chat = ChatKey::new(Provider::Whatsapp, "personal", "personal:v@s.whatsapp.net");
    let cid = format_client_id([0x55; 16]);
    let rec = OutboxRecord::new_voice(
        cid.clone(),
        chat,
        None,
        tmp_path.to_string_lossy(),
        expected_sha,
        original.len() as u64,
        3,
        1_000,
    );
    store.enqueue_outbox(&rec).unwrap();

    let claimed = store.claim_next_ready_outbox(1_000).unwrap().unwrap();
    let disk_bytes = std::fs::read(claimed.staged_file_path.as_deref().unwrap()).unwrap();
    assert!(!verify_attachment_bytes(
        &disk_bytes,
        claimed.file_bytes,
        &claimed.file_sha256.unwrap()
    ));
    assert!(
        store
            .mark_outbox_attachment_unavailable(&claimed.client_id)
            .unwrap()
    );

    let held = store.get_outbox(&cid).unwrap().unwrap();
    assert_eq!(held.phase, OutboxPhase::Held);
    assert_eq!(
        held.failure_reason.as_deref(),
        Some("attachment unavailable")
    );
    // Zero upload attempts reached the Hub!
    assert_eq!(state.lock().unwrap().send_uploads, 0);

    // Valid multipart file send DOES succeed.
    let ok_reply = client
        .send_file(SendFileRequest {
            chat: "personal:v@s.whatsapp.net",
            text: "",
            reply_to: "",
            client_id: &format_client_id([0x66; 16]),
            ptt: true,
            filename: "voice.ogg",
            bytes: original,
        })
        .await
        .unwrap();
    assert_eq!(ok_reply.status, "sent");
    assert_eq!(state.lock().unwrap().send_uploads, 1);

    let _ = std::fs::remove_file(&tmp_path);
    server.abort();
}

#[tokio::test]
async fn outbox_failed_chat_never_blocks_other_chats() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    let bad_chat = "personal:bad@s.whatsapp.net";
    let good_chat = "personal:good@s.whatsapp.net";
    state.lock().unwrap().send_script.insert(
        bad_chat.into(),
        vec![(409, r#"{"error":"conflict"}"#.into())],
    );

    let (endpoint, server) = start_mock_hub(state).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();
    let mut store = Store::open_in_memory().unwrap();

    let cid_bad = format_client_id([0x10; 16]);
    let cid_good = format_client_id([0x20; 16]);
    store
        .enqueue_outbox(&OutboxRecord::new_text(
            cid_bad.clone(),
            ChatKey::new(Provider::Whatsapp, "personal", bad_chat),
            "will 409",
            None,
            false,
            100,
        ))
        .unwrap();
    store
        .enqueue_outbox(&OutboxRecord::new_text(
            cid_good.clone(),
            ChatKey::new(Provider::Whatsapp, "personal", good_chat),
            "will succeed",
            None,
            false,
            200,
        ))
        .unwrap();

    // First claim hits 409 -> terminal Held + ambiguous.
    let c1 = store.claim_next_ready_outbox(500).unwrap().unwrap();
    let err1 = client
        .send_text(&c1.chat.id, &c1.text_body, None, false, &c1.client_id)
        .await
        .unwrap_err();
    assert!(matches!(err1, HubError::HttpStatus { status: 409, .. }));
    store
        .mark_outbox_held(&c1.client_id, c1.attempt_serial, true, "conflict")
        .unwrap();

    // Second claim skips held bad_chat and delivers good_chat immediately (ReadyResolves)!
    let c2 = store.claim_next_ready_outbox(500).unwrap().unwrap();
    assert_eq!(c2.client_id, cid_good);
    let reply = client
        .send_text(&c2.chat.id, &c2.text_body, None, false, &c2.client_id)
        .await
        .unwrap();
    assert_eq!(reply.status, "sent");
    assert!(
        store
            .acknowledge_outbox(&c2.client_id, c2.attempt_serial)
            .unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn archive_batch_partial_failure_and_signal_502_local_fallback() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    state
        .lock()
        .unwrap()
        .archive_batch_fail_chats
        .insert("personal:fail@s.whatsapp.net".into());

    let (endpoint, server) = start_mock_hub(state).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();

    let batch = client
        .archive_batch(
            &["personal:ok@s.whatsapp.net", "personal:fail@s.whatsapp.net"],
            true,
        )
        .await
        .unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch[0].ok);
    assert!(!batch[1].ok);

    assert_eq!(
        client.archive("signal:u1", true).await.unwrap(),
        ArchiveOutcome::ManagedLocally
    );
    assert_eq!(
        client.archive("telegram:123", true).await.unwrap(),
        ArchiveOutcome::ManagedLocally
    );
    assert_eq!(
        client
            .archive("personal:ok@s.whatsapp.net", true)
            .await
            .unwrap(),
        ArchiveOutcome::Completed
    );
    server.abort();
}

#[tokio::test]
async fn normalizer_lid_alias_slack_attachments_and_path_stripping() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    {
        let mut g = state.lock().unwrap();
        g.messages_by_chat.insert(
            "personal:49123@s.whatsapp.net".into(),
            vec![json!({
                "MsgID": "wa_1",
                "ChatJID": "personal:777@lid",
                "SenderName": "Alice",
                "Timestamp": "2026-11-26T10:00:00Z",
                "Text": "Aliased lid message",
                "LocalPath": "/secret/path",
                "local_path": "/secret/path2"
            })],
        );
        g.messages_by_chat.insert(
            "slack:T1:C1".into(),
            vec![json!({
                "MsgID": "1700.1",
                "ChatJID": "slack:T1:C1",
                "SenderName": "Bob",
                "Timestamp": "2026-11-26T10:05:00Z",
                "Text": "Gif time",
                "SlackEmojiPaths": {"x": "/tmp/x"},
                "reactions": [{"name": "tea", "count": 1}, {"name": "shipit", "count": 3}],
                "attachments": [{"image_url": "https://media.giphy.com/demo.gif", "service_name": "Giphy"}]
            })],
        );
    }

    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();

    let wa = client
        .fetch_messages("personal:49123@s.whatsapp.net", 80, None)
        .await
        .unwrap();
    assert_eq!(wa[0].chat_jid, "personal:49123@s.whatsapp.net");
    assert!(wa[0].raw.get("LocalPath").is_none());
    assert!(wa[0].raw.get("local_path").is_none());

    let sl = client
        .fetch_messages("slack:T1:C1", 80, None)
        .await
        .unwrap();
    assert_eq!(sl[0].media_type, "image");
    assert_eq!(sl[0].mime_type.as_deref(), Some("image/gif"));
    assert_eq!(sl[0].reaction_emoji, "🍵 1  :shipit: 3");
    assert!(sl[0].raw.get("SlackEmojiPaths").is_none());

    // Also exercise mark-read, react, and delete endpoints over loopback HTTP.
    client.mark_read("signal:u1", "msg-99").await.unwrap();
    assert_eq!(
        state.lock().unwrap().mark_read_calls,
        vec![("signal:u1".to_owned(), "msg-99".to_owned())]
    );
    let r = client
        .react("slack:T1:C1", "1700.1", "👍", "cid-react-1")
        .await
        .unwrap();
    assert_eq!(r.status, "sent");
    client
        .delete_message("slack:T1:C1", "1700.1", "cid-del-1")
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn avatar_fetch_distinguishes_queued_missing_ready_and_errors() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();
    let chat = "personal:1@s.whatsapp.net";

    for (status, expected) in [
        (None, Ok(AvatarFetchState::Ready(b"PNG-AVATAR".to_vec()))),
        (Some(202), Ok(AvatarFetchState::Pending)),
        (Some(404), Ok(AvatarFetchState::Missing)),
        (Some(410), Ok(AvatarFetchState::Missing)),
        (Some(206), Err(HubError::PartialMedia)),
        (
            Some(503),
            Err(HubError::HttpStatus {
                status: 503,
                body: "partial".to_owned(),
            }),
        ),
    ] {
        state.lock().unwrap().media_status_override = status;
        assert_eq!(client.fetch_avatar_state(chat).await, expected);
    }

    state.lock().unwrap().media_status_override = Some(202);
    assert_eq!(client.fetch_avatar(chat).await.unwrap(), None);
    state.lock().unwrap().media_status_override = None;
    assert_eq!(
        client.fetch_avatar_state(chat).await.unwrap(),
        AvatarFetchState::Ready(b"PNG-AVATAR".to_vec())
    );
    server.abort();
}

#[tokio::test]
async fn media_202_poll_206_reject_and_410_unavailable() {
    let state = Arc::new(Mutex::new(MockHubState::default()));
    state.lock().unwrap().media_202_remaining = 2;

    let (endpoint, server) = start_mock_hub(state.clone()).await;
    let client = HubClient::new(endpoint, TEST_TOKEN).unwrap();

    // Polls 202 -> 202 -> 200 Ready.
    let media = client
        .fetch_media_polling(
            "personal:1@s.whatsapp.net",
            "m1",
            Duration::from_millis(10),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(media, MediaFetchState::Ready(b"MEDIA-OK-PAYLOAD".to_vec()));

    // 206 Partial Content MUST be rejected uniformly across media, avatar, icon, and emoji!
    state.lock().unwrap().media_status_override = Some(206);
    assert_eq!(
        client
            .fetch_media_once("personal:1@s.whatsapp.net", "m1")
            .await,
        Err(HubError::PartialMedia)
    );
    assert_eq!(
        client.fetch_avatar("personal:1@s.whatsapp.net").await,
        Err(HubError::PartialMedia)
    );
    assert_eq!(
        client.fetch_workspace_icon("T1").await,
        Err(HubError::PartialMedia)
    );
    assert_eq!(
        client.fetch_slack_emoji_media("T1", "shipit").await,
        Err(HubError::PartialMedia)
    );

    // 410 Gone returns Gone (permanently unavailable) while 404 returns Unavailable.
    state.lock().unwrap().media_status_override = Some(410);
    assert_eq!(
        client
            .fetch_media_once("personal:1@s.whatsapp.net", "m1")
            .await
            .unwrap(),
        MediaFetchState::Gone
    );
    state.lock().unwrap().media_status_override = Some(404);
    assert_eq!(
        client
            .fetch_media_once("personal:1@s.whatsapp.net", "m1")
            .await
            .unwrap(),
        MediaFetchState::Unavailable
    );

    // Clear override for normal 200 avatar, workspace icon, Slack emoji index & media.
    state.lock().unwrap().media_status_override = None;
    assert_eq!(
        client
            .fetch_avatar("personal:1@s.whatsapp.net")
            .await
            .unwrap()
            .as_deref(),
        Some(b"PNG-AVATAR".as_slice())
    );
    assert_eq!(
        client.fetch_workspace_icon("T1").await.unwrap().as_deref(),
        Some(b"PNG-ICON".as_slice())
    );
    let idx = client.fetch_slack_emoji_index("T1").await.unwrap();
    assert!(idx["emoji"]["shipit"].is_string());
    assert_eq!(
        client
            .fetch_slack_emoji_media("T1", "shipit")
            .await
            .unwrap()
            .as_deref(),
        Some(b"EMOJI-PNG".as_slice())
    );
    server.abort();
}
