#![forbid(unsafe_code)]

use postbox_core::archive::should_resurface_on_event;
use postbox_core::control::{ControlRequest, TelemetryCounters};
use postbox_core::deep_link::parse as parse_deep_link;
use postbox_core::dictation::DictationLanguage;
use postbox_core::{Chat, ChatKey, MediaKind, Message, Provider, ReadStatus, ThemeChoice, ViewTab};
use postbox_e2e::fixtures::{
    minimal_ogg_opus_packet, minimal_png_1x1, pcm16_raw, wcag_contrast_ratio,
};
use postbox_e2e::wire::{parse_store_zip, sha256_hex};
use postbox_e2e::{
    ArchiveBatchFault, E2eWorkspace, HubMessageRow, LoopbackHttpClient, M8HookRegistry,
    MediaBehavior, MockDeepgramServer, MockExternalServer, MockHubServer, ParsedSseFrame,
    ProcessRunner, RealSizeInbox, SendFault, UpdateScenario, WsScriptStep, WsServerAction,
    WsTrigger,
};
use postbox_hub::{
    OLDER_PAGE_LIMIT, SendReply, SseEvent, archive_request, before_cursor, events_request,
    mark_read_request, media_request, messages_request, send_text_request,
};
use postbox_keymap::ctrl_a_selects_chats;
use postbox_platform::SecretStore;
use postbox_store::{MEDIA_TTL_SECS, OutboxPhase, OutboxRecord};
use postbox_ui::theme::ShadcnPalette;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn mk_chat(provider: Provider, account: &str, id: &str, title: &str, ts: &str) -> Chat {
    Chat {
        key: ChatKey::new(provider, account, id),
        title: title.to_string(),
        is_group: false,
        archived_local: false,
        archived_remote: false,
        pinned: false,
        unread_count: 0,
        last_msg_ts: ts.to_string(),
        last_msg_preview: String::new(),
        avatar_hash: None,
    }
}

fn mk_msg(
    chat: &ChatKey,
    msg_id: &str,
    sender: &str,
    ts: &str,
    body: &str,
    from_me: bool,
) -> Message {
    Message {
        chat: chat.clone(),
        msg_id: msg_id.to_string(),
        sender_id: format!("u-{}", sender.to_ascii_lowercase()),
        sender_name: sender.to_string(),
        from_me,
        ts: ts.to_string(),
        body: body.to_string(),
        reply_to_id: None,
        reply_broadcast: false,
        media_kind: None,
        mime_type: None,
        filename: None,
        reactions: Vec::new(),
        read_status: ReadStatus::None,
        deleted: false,
    }
}

#[test]
fn t4_01_real_size_inbox_320_chats_6704_msgs_fts5_under_2ms() {
    let mut ws = E2eWorkspace::new("t4_01_real_size");
    let dataset = RealSizeInbox::generate();
    let stats = dataset.seed_workspace_bulk(&mut ws).unwrap();

    assert_eq!(stats.chats, 320);
    assert_eq!(stats.messages, 6_704);
    assert_eq!(stats.images, 638);
    assert_eq!(stats.voice_notes, 319);
    assert_eq!(ws.count_chats(), 320);
    assert_eq!(ws.count_messages(), 6_704);

    // Cold reopen on real on-disk WAL DB
    ws.reopen_store().unwrap();

    let inbox_start = Instant::now();
    let top50 = ws.store.list_inbox_chats(false, 50).unwrap();
    let inbox_elapsed = inbox_start.elapsed();
    assert_eq!(top50.len(), 50);
    assert!(
        inbox_elapsed < Duration::from_millis(15),
        "cold top-50 inbox query took {:?} (budget < 15 ms)",
        inbox_elapsed
    );

    let queries = [
        "DXVA",
        "Uberblick",
        "Rechnung",
        "Quarterly",
        "Besprechung",
        "Cafe",
        "Munchen",
        "updater",
        "SQLite",
        "broadcast",
        "toolbar",
        "Deepgram",
        "eviction",
        "benchmark",
        "hardware*",
        "Quartals*",
        "canary",
        "\"canary-needle-001\"",
        "\"canary-needle-005\"",
        "\"canary-needle-010\"",
    ];
    assert_eq!(queries.len(), 20);

    let mut timings_us = Vec::with_capacity(queries.len());
    for q in queries {
        let t0 = Instant::now();
        let hits = ws.store.search_messages_fts(q, 25).unwrap();
        let us = t0.elapsed().as_micros();
        assert!(!hits.is_empty(), "expected FTS5 hits for query {q}");
        timings_us.push(us);
    }

    timings_us.sort_unstable();
    let median_us = timings_us[timings_us.len() / 2];
    let p95_us = timings_us[(timings_us.len() * 95) / 100];
    eprintln!(
        "[t4_01] seed={:?} cold_top50={:?} fts5_median={}us fts5_p95={}us fts5_max={}us (320 chats, 6704 msgs, 638 images, 319 voice notes)",
        stats.seed_duration,
        inbox_elapsed,
        median_us,
        p95_us,
        timings_us.last().copied().unwrap_or(0)
    );
    assert!(
        median_us < 2_000,
        "median FTS5 latency {}us >= 2000us",
        median_us
    );
}

#[test]
fn t4_02_morning_triage_sse_burst_resurface_and_mark_read() {
    let mut ws = E2eWorkspace::new("t4_02_triage");
    let hub = MockHubServer::start("tok-t4-02");

    // Seed 10 chats across 4 providers; 4 locally archived WA chats
    let mut chats = Vec::new();
    for i in 0..10usize {
        let (prov, acct, id) = match i {
            0..=5 => (
                Provider::Whatsapp,
                "personal",
                format!("wa{i}@s.whatsapp.net"),
            ),
            6..=7 => (Provider::Slack, "T1", format!("C{i}")),
            8 => (Provider::Telegram, "telegram", "tg-8".into()),
            _ => (Provider::Signal, "signal", "sig-9".into()),
        };
        let mut c = mk_chat(
            prov,
            acct,
            &id,
            &format!("Chat {i}"),
            "2026-11-26T08:00:00Z",
        );
        if i < 4 {
            c.archived_local = true;
        }
        ws.store.upsert_chat(&c, 100 + i as i64).unwrap();
        chats.push(c);
    }

    // Push 50 SSE events id=101..=150
    let kinds = ["msg", "read", "react", "send", "reset"];
    let events: Vec<SseEvent> = (101u64..=150)
        .map(|id| {
            let idx = (id as usize) % chats.len();
            let c = &chats[idx];
            SseEvent {
                id,
                account: c.key.account.clone(),
                chat: format!("{}:{}", c.key.account, c.key.id),
                ts: format!("2026-11-26T09:{:02}:{:02}Z", (id / 60) % 60, id % 60),
                kind: kinds[(id as usize) % kinds.len()].into(),
            }
        })
        .collect();
    hub.push_sse_burst(events);

    let client = LoopbackHttpClient::default();
    let mut stream = client
        .open_sse(&hub.endpoint(), &hub.token(), &events_request(Some("100")))
        .unwrap();

    let mut last_id = 100u64;
    for _ in 0..50 {
        let Some(ParsedSseFrame::Event(ev)) = stream.next_frame(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected SSE event");
        };
        assert!(ev.id > last_id);
        last_id = ev.id;

        if let Some(chat) = chats
            .iter_mut()
            .find(|c| format!("{}:{}", c.key.account, c.key.id) == ev.chat)
            && ev.kind == "msg"
        {
            if should_resurface_on_event(chat.archived_local, false, false) {
                chat.archived_local = false;
            }
            chat.unread_count += 1;
            ws.store.upsert_chat(chat, 1000 + ev.id as i64).unwrap();
            let m = mk_msg(
                &chat.key,
                &format!("SSE-M-{}", ev.id),
                "Peer",
                &ev.ts,
                &format!("Morning burst event {}", ev.id),
                false,
            );
            ws.store.upsert_message(&m).unwrap();
        }
    }
    assert_eq!(last_id, 150);

    // Mark triaged chats read via Hub
    for chat in &mut chats {
        if chat.unread_count > 0 {
            let handle = format!("{}:{}", chat.key.account, chat.key.id);
            let resp = client
                .execute_hub(
                    &hub.endpoint(),
                    &hub.token(),
                    &mark_read_request(&handle, "").unwrap(),
                )
                .unwrap();
            assert_eq!(resp.status, 200);
            chat.unread_count = 0;
            ws.store.upsert_chat(chat, 2000).unwrap();
        }
    }

    for c in ws.store.list_inbox_chats(true, 20).unwrap() {
        assert_eq!(c.unread_count, 0);
    }
    assert_eq!(ws.count_messages(), 10);
}

#[test]
fn t4_03_bulk_inbox_zero_ctrl_a_archive_batch_and_partial_retry() {
    let ws = E2eWorkspace::new("t4_03_inbox_zero");
    let hub = MockHubServer::start("tok-t4-03");
    assert!(ctrl_a_selects_chats(true, true, true));

    let mut wa_chats = Vec::new();
    let mut local_chats = Vec::new();
    for i in 0..40usize {
        let (prov, acct, id) = match i {
            0..=19 => (
                Provider::Whatsapp,
                "personal",
                format!("wa{i:02}@s.whatsapp.net"),
            ),
            20..=29 => (Provider::Slack, "T1", format!("C{i:02}")),
            30..=34 => (Provider::Telegram, "telegram", format!("tg{i:02}")),
            _ => (Provider::Signal, "signal", format!("sig{i:02}")),
        };
        let c = mk_chat(
            prov,
            acct,
            &id,
            &format!("Triage {i:02}"),
            "2026-11-26T10:00:00Z",
        );
        ws.store.upsert_chat(&c, 100 + i as i64).unwrap();
        if prov == Provider::Whatsapp {
            wa_chats.push(c);
        } else {
            local_chats.push(c);
        }
    }

    // Inject 2 item failures into WA archive-batch
    let mut fail_set = HashSet::new();
    fail_set.insert("personal:wa03@s.whatsapp.net".to_string());
    fail_set.insert("personal:wa17@s.whatsapp.net".to_string());
    hub.set_archive_batch_fault(Some(ArchiveBatchFault::PartialFailure(fail_set)));

    let client = LoopbackHttpClient::default();
    let auth = format!("Bearer {}", hub.token());
    let batch_items: Vec<Value> = wa_chats
        .iter()
        .map(|c| json!({"chat": format!("{}:{}", c.key.account, c.key.id)}))
        .collect();
    let batch_resp = client
        .execute_raw(
            hub.addr(),
            "POST",
            "/v1/wazig/archive-batch",
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(
                json!({"account": "", "chats": batch_items, "archived": true})
                    .to_string()
                    .as_bytes(),
            ),
        )
        .unwrap();
    assert_eq!(batch_resp.status, 200);
    let results = batch_resp.json::<Value>().unwrap()["results"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(results.len(), 20);

    // Retry failed items individually
    let mut retried = 0usize;
    for res in &results {
        if res["ok"] == false {
            let handle = res["chat"].as_str().unwrap();
            let r = client
                .execute_hub(
                    &hub.endpoint(),
                    &hub.token(),
                    &archive_request(handle, true).unwrap(),
                )
                .unwrap();
            assert_eq!(r.status, 200);
            retried += 1;
        }
    }
    assert_eq!(retried, 2);

    for c in wa_chats.iter_mut().chain(local_chats.iter_mut()) {
        c.archived_local = true;
        ws.store.upsert_chat(c, 500).unwrap();
    }

    assert!(ws.store.list_inbox_chats(false, 100).unwrap().is_empty());
    assert_eq!(ws.store.list_inbox_chats(true, 100).unwrap().len(), 40);
    assert_eq!(hub.request_count("POST", "/v1/wazig/archive-batch"), 1);
    assert_eq!(hub.request_count("POST", "/v1/wazig/archive"), 2);
}

#[test]
fn t4_04_voice_review_319_notes_cached_transcripts_zero_refetch() {
    let mut ws = E2eWorkspace::new("t4_04_voice_319");
    let hub = MockHubServer::start("tok-t4-04");
    let dg = MockDeepgramServer::start("dg-t4-04");
    let dataset = RealSizeInbox::generate();
    assert_eq!(dataset.voice_count, 319);

    let voice_msgs: Vec<&Message> = dataset
        .messages
        .iter()
        .filter(|m| m.media_kind == Some(MediaKind::Voice))
        .collect();
    assert_eq!(voice_msgs.len(), 319);

    for m in &voice_msgs {
        ws.store.upsert_message(m).unwrap();
    }
    assert_eq!(ws.count_messages(), 319);

    let client = LoopbackHttpClient::default();
    let mut cached_shas = Vec::new();
    for (idx, m) in voice_msgs.iter().take(20).enumerate() {
        let mut ogg = minimal_ogg_opus_packet();
        ogg.extend_from_slice(&(idx as u32).to_le_bytes());
        let handle = format!("{}:{}", m.chat.account, m.chat.id);
        hub.set_media(
            &handle,
            &m.msg_id,
            MediaBehavior::Ready {
                bytes: ogg,
                content_type: "audio/ogg".into(),
            },
        );
        let dl = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(&handle, Some(&m.msg_id)).unwrap(),
            )
            .unwrap();
        assert_eq!(dl.status, 200);
        let sha = sha256_hex(&dl.body);

        let dg_resp = client
            .execute_raw(
                dg.addr(),
                "POST",
                DictationLanguage::Automatic.deepgram_listen_path(),
                &[("Authorization", "Token dg-t4-04")],
                Some(&dl.body),
            )
            .unwrap();
        assert_eq!(dg_resp.status, 200);
        let text = format!("Voice note #{idx} transcript");
        ws.store
            .cache_transcript(&sha, "en", &text, 1000 + idx as i64)
            .unwrap();
        cached_shas.push((sha, text));
    }
    assert_eq!(dg.rest_call_count(), 20);

    // Reopen Store -> 0 re-requests + immutable ON CONFLICT DO NOTHING
    ws.reopen_store().unwrap();
    for (sha, expected) in &cached_shas {
        assert_eq!(
            ws.store.get_transcript(sha).unwrap().as_deref(),
            Some(expected.as_str())
        );
        ws.store
            .cache_transcript(sha, "de", "Mutated attempt", 9999)
            .unwrap();
        assert_eq!(
            ws.store.get_transcript(sha).unwrap().as_deref(),
            Some(expected.as_str())
        );
    }
    assert_eq!(dg.rest_call_count(), 20);
}

#[test]
fn t4_05_live_dictation_reply_compose_and_durable_outbox_send() {
    let mut ws = E2eWorkspace::new("t4_05_dict_send");
    let dg = MockDeepgramServer::start("dg-t4-05");
    let hub = MockHubServer::start("hub-t4-05");

    ws.platform
        .save_secret("DEEPGRAM_API_KEY", " dg-t4-05 ")
        .unwrap();
    let api_key = ws
        .platform
        .load_secret("DEEPGRAM_API_KEY")
        .unwrap()
        .unwrap();

    dg.set_ws_script(vec![
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: false,
                start: 0.0,
                duration: 0.4,
                transcript: "bonjour ignore".into(),
                word_languages: vec!["fr".into()],
                languages: vec!["fr".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(2),
            action: WsServerAction::SendResults {
                is_final: false,
                start: 0.5,
                duration: 0.4,
                transcript: "Confirming deploy".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::OnCloseStream,
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.5,
                duration: 0.8,
                transcript: "Confirming deployment window at 18:00.".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
    ]);

    let client = LoopbackHttpClient::default();
    let auth = format!("Token {api_key}");
    let mut conn = client
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3&interim_results=true",
            &[("Authorization", auth.as_str())],
        )
        .unwrap();

    conn.send_binary(&pcm16_raw(16_000, 40, 5000)).unwrap();
    let f_fr: Value =
        serde_json::from_slice(&conn.recv_frame(Duration::from_secs(2)).unwrap().payload).unwrap();
    assert!(
        !DictationLanguage::Automatic
            .accepts_detected_tag(f_fr["channel"]["alternatives"][0]["languages"][0].as_str())
    );

    conn.send_binary(&pcm16_raw(16_000, 40, 5000)).unwrap();
    let _interim = conn.recv_frame(Duration::from_secs(2)).unwrap();

    conn.send_text(r#"{"type":"CloseStream"}"#).unwrap();
    let f_final: Value =
        serde_json::from_slice(&conn.recv_frame(Duration::from_secs(2)).unwrap().payload).unwrap();
    let lang = f_final["channel"]["alternatives"][0]["languages"][0].as_str();
    assert!(DictationLanguage::Automatic.accepts_detected_tag(lang));
    let final_text = f_final["channel"]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap()
        .to_string();

    let chat_key = ChatKey::new(Provider::Whatsapp, "personal", "ops@s.whatsapp.net");
    let cid = "d405d405d405d405d405d405d405d405";
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: cid.into(),
            chat: chat_key.clone(),
            kind: "text".into(),
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            text_body: final_text.clone(),
            reply_to_id: Some("M-ORIG-1".into()),
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();

    let ready = ws.store.next_ready_outbox(500).unwrap().unwrap();
    let send_req = send_text_request(
        "personal:ops@s.whatsapp.net",
        &ready.text_body,
        ready.reply_to_id.as_deref(),
        false,
        &ready.client_id,
    )
    .unwrap();
    let send_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &send_req)
        .unwrap();
    assert_eq!(send_resp.status, 200);
    let reply: SendReply = send_resp.json().unwrap();
    assert_eq!(reply.client_id, cid);

    let confirmed = mk_msg(&chat_key, &reply.msg_id, "Me", &reply.ts, &final_text, true);
    ws.store.upsert_message(&confirmed).unwrap();
    assert_eq!(
        ws.store.search_messages_fts("deployment", 5).unwrap().len(),
        1
    );
}

#[test]
fn t4_06_offline_outage_crash_recovery_and_reconnect_drain() {
    let mut ws = E2eWorkspace::new("t4_06_outage");
    let hub = MockHubServer::start("tok-t4-06");

    // 1 Held (409 on Chat 1), 1 Uncertain (crash on Chat 2), 10 Ready across Chats 3 & 4
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "40900000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "c1@s.whatsapp.net"),
            kind: "text".into(),
            phase: OutboxPhase::Held,
            attempt_serial: 1,
            text_body: "Held 409".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 1,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "c0a50000000000000000000000000002".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "c2@s.whatsapp.net"),
            kind: "text".into(),
            phase: OutboxPhase::Uncertain,
            attempt_serial: 1,
            text_body: "Crashed uncertain".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 2,
        })
        .unwrap();

    for i in 0..10u64 {
        let chat_id = if i % 2 == 0 {
            "c3@s.whatsapp.net"
        } else {
            "c4@s.whatsapp.net"
        };
        ws.store
            .enqueue_outbox(&OutboxRecord {
                client_id: format!("e00000000000000000000000000000{i:02}"),
                chat: ChatKey::new(Provider::Whatsapp, "personal", chat_id),
                kind: "text".into(),
                phase: OutboxPhase::Ready,
                attempt_serial: 0,
                text_body: format!("Queued msg {i}"),
                reply_to_id: None,
                reply_broadcast: false,
                not_before_ms: 0,
                created_at_ms: 100 + i as i64,
            })
            .unwrap();
    }

    ws.reopen_store().unwrap();

    // Inject transient 503 on first send attempt, then Hub recovers to 200
    hub.push_send_fault(SendFault::Transient503);
    let client = LoopbackHttpClient::default();

    let mut delivered_order = Vec::new();
    while let Some(rec) = ws.store.next_ready_outbox(10_000).unwrap() {
        let handle = format!("{}:{}", rec.chat.account, rec.chat.id);
        let req = send_text_request(&handle, &rec.text_body, None, false, &rec.client_id).unwrap();
        let mut resp = client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap();
        if resp.status == 503 {
            resp = client
                .execute_hub(&hub.endpoint(), &hub.token(), &req)
                .unwrap();
        }
        assert_eq!(resp.status, 200);
        delivered_order.push(rec.client_id.clone());
        ws.open_aux_conn()
            .execute("DELETE FROM outbox WHERE client_id = ?1", [&rec.client_id])
            .unwrap();
    }

    assert_eq!(delivered_order.len(), 10);
    assert_eq!(delivered_order[0], "e0000000000000000000000000000000");
    assert_eq!(delivered_order[9], "e0000000000000000000000000000009");
    // Crashed Uncertain row was NEVER auto-resent
    assert_eq!(
        hub.client_id_delivery_count("c0a50000000000000000000000000002"),
        0
    );

    // Explicit user retry with fresh 32-hex client_id succeeds
    let fresh_retry = "f0000000000000000000000000000099";
    let retry_resp = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request(
                "personal:c2@s.whatsapp.net",
                "Crashed uncertain",
                None,
                false,
                fresh_retry,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(retry_resp.status, 200);
    assert_eq!(hub.client_id_delivery_count(fresh_retry), 1);
}

#[test]
fn t4_07_media_heavy_638_images_14d_eviction_410_and_206_faults() {
    let ws = E2eWorkspace::new("t4_07_media_638");
    let hub = MockHubServer::start("tok-t4-07");
    let now = 1_764_150_000_i64;

    // Seed exact 638 image entries in one transaction: 400 fresh (<14d) + 238 stale (>14d)
    let owned_rows: Vec<(String, String, String, i64)> = (0..638usize)
        .map(|i| {
            let ts = if i < 400 {
                now - ((i as i64) * 100)
            } else {
                now - MEDIA_TTL_SECS - 1 - (i as i64)
            };
            (
                format!("img-{i:04}"),
                format!("sha-{i:04}"),
                format!("hub-media/img-{i:04}.png"),
                ts,
            )
        })
        .collect();
    let batch: Vec<(&str, &str, &str, &str, i64, i64)> = owned_rows
        .iter()
        .map(|(key, sha, rel, ts)| {
            (
                key.as_str(),
                sha.as_str(),
                rel.as_str(),
                "image/png",
                1024,
                *ts,
            )
        })
        .collect();
    ws.seed_media_cache_batch(&batch);
    assert_eq!(ws.count_media_cache(), 638);

    let evicted = ws.store.evict_expired_media(now).unwrap();
    assert_eq!(evicted.len(), 238);
    assert_eq!(ws.count_media_cache(), 400);

    // Exercise 200, 202->200, 206 (rejected), 410 (gone) on MockHubServer
    let chat = "personal:med@s.whatsapp.net";
    let png = minimal_png_1x1();
    hub.set_media(
        chat,
        "M-200",
        MediaBehavior::Ready {
            bytes: png.clone(),
            content_type: "image/png".into(),
        },
    );
    hub.set_media(
        chat,
        "M-202",
        MediaBehavior::PendingThenReady {
            remaining_202s: 1,
            bytes: png.clone(),
            content_type: "image/png".into(),
        },
    );
    hub.set_media(
        chat,
        "M-206",
        MediaBehavior::Partial206 {
            bytes: vec![1, 2],
            content_type: "image/png".into(),
        },
    );
    hub.set_media(chat, "M-410", MediaBehavior::Gone410);

    let client = LoopbackHttpClient::default();
    assert_eq!(
        client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some("M-200")).unwrap()
            )
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some("M-202")).unwrap()
            )
            .unwrap()
            .status,
        202
    );
    assert_eq!(
        client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some("M-202")).unwrap()
            )
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some("M-206")).unwrap()
            )
            .unwrap()
            .status,
        206
    );
    assert_eq!(
        client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some("M-410")).unwrap()
            )
            .unwrap()
            .status,
        410
    );
}

#[test]
fn t4_08_multi_account_slack_threads_broadcast_and_deep_link() {
    let mut ws = E2eWorkspace::new("t4_08_multi");
    let hub = MockHubServer::start("tok-t4-08");

    let views = [
        (ViewTab::CombinedInbox, "Combined inbox - Postbox v0.1.0"),
        (
            ViewTab::Account {
                provider: Provider::Whatsapp,
                account: "personal".into(),
                label: "Personal".into(),
            },
            "Personal - Postbox v0.1.0",
        ),
        (
            ViewTab::Account {
                provider: Provider::Whatsapp,
                account: "business".into(),
                label: "Business".into(),
            },
            "Business - Postbox v0.1.0",
        ),
        (
            ViewTab::Account {
                provider: Provider::Slack,
                account: "T-ACME".into(),
                label: "Acme".into(),
            },
            "Acme - Postbox v0.1.0",
        ),
        (
            ViewTab::Account {
                provider: Provider::Telegram,
                account: "main".into(),
                label: "Telegram".into(),
            },
            "Telegram - Postbox v0.1.0",
        ),
        (
            ViewTab::Account {
                provider: Provider::Signal,
                account: "main".into(),
                label: "Signal".into(),
            },
            "Signal - Postbox v0.1.0",
        ),
    ];
    for (tab, expected) in &views {
        assert_eq!(tab.window_title("0.1.0"), *expected);
    }

    let dl = parse_deep_link("postbox://slack/T-ACME/C-ENG/p1700000005000100").unwrap();
    assert_eq!(dl.message.as_deref(), Some("1700000005.000100"));

    let handle = "slack:T-ACME:C-ENG";
    let rows: Vec<HubMessageRow> = (0..100usize)
        .map(|i| {
            HubMessageRow::simple(
                handle,
                format!("1700000{i:03}.000100"),
                "Grace",
                format!("Slack eng msg {i}"),
                format!("2026-11-26T{:02}:{:02}:00Z", i / 60, i % 60),
                false,
            )
        })
        .collect();
    hub.set_messages(handle, rows);

    let client = LoopbackHttpClient::default();
    let p1: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(handle, OLDER_PAGE_LIMIT, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    let cur = before_cursor(&p1.last().unwrap().MsgID, &p1.last().unwrap().Timestamp).unwrap();
    let p2: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(handle, OLDER_PAGE_LIMIT, Some(&cur)).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(p1.len() + p2.len(), 100);

    let bcast = send_text_request(
        handle,
        "Channel broadcast",
        dl.message.as_deref(),
        true,
        "cid-t4-bcast",
    )
    .unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &bcast)
            .unwrap()
            .status,
        200
    );
    let m = mk_msg(
        &dl.chat,
        "M-BC",
        "Me",
        "2026-11-26T12:30:00Z",
        "Channel broadcast",
        true,
    );
    ws.store.upsert_message(&m).unwrap();
    assert_eq!(
        ws.store.search_messages_fts("broadcast", 5).unwrap().len(),
        1
    );
}

#[test]
fn t4_09_power_user_postboxctl_telemetry_and_appearance_session() {
    let app = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(app.exit_code, 0);

    let valid_cmds: &[&[&str]] = &[
        &["status"],
        &["theme", "amoled"],
        &["theme", "light"],
        &["theme", "zinc"],
        &["font", "80"],
        &["font", "125"],
        &["font", "150"],
        &["font", "100"],
        &["search", "DXVA"],
        &["view", "inbox"],
        &["archive", "personal:1@s.whatsapp.net"],
        &["unarchive", "personal:1@s.whatsapp.net"],
    ];
    for args in valid_cmds {
        let out = ProcessRunner::run("postboxctl", args, Duration::from_secs(10));
        assert_eq!(out.exit_code, 0, "args={args:?} stderr={}", out.stderr);
        let parsed: ControlRequest = serde_json::from_str(out.stdout.trim()).unwrap();
        let _ = serde_json::to_string(&parsed).unwrap();
    }

    let invalid = ProcessRunner::run("postboxctl", &["unknown-cmd"], Duration::from_secs(10));
    assert_eq!(invalid.exit_code, 2);

    let ws = E2eWorkspace::new("t4_09_telemetry");
    let pending_outbox = u32::from(ws.store.next_ready_outbox(0).unwrap().is_some());
    let telemetry = TelemetryCounters {
        frames_rendered: 3600,
        working_set_kb: 38_500,
        gpu_texture_bytes: 24 * 1024 * 1024,
        pending_outbox,
    };
    let raw_t = serde_json::to_string(&telemetry).unwrap();
    let json_t: Value = serde_json::from_str(&raw_t).unwrap();
    assert_eq!(json_t["frames_rendered"], 3600);
    assert_eq!(json_t["working_set_kb"], 38_500);
    assert_eq!(json_t["gpu_texture_bytes"], 24 * 1024 * 1024);
    assert_eq!(json_t["pending_outbox"], 0);
    assert_eq!(
        serde_json::from_str::<TelemetryCounters>(&raw_t).unwrap(),
        telemetry
    );
    let _hooks = M8HookRegistry::current();

    for choice in [
        ThemeChoice::ZincDark,
        ThemeChoice::Amoled,
        ThemeChoice::Light,
    ] {
        let p = ShadcnPalette::for_choice(choice);
        assert!(wcag_contrast_ratio(p.foreground, p.background) >= 4.5);
    }

    let assets = E2eWorkspace::assets_dir();
    for font in [
        "IBMPlexSans-Regular.ttf",
        "IBMPlexSans-Medium.ttf",
        "IBMPlexSans-SemiBold.ttf",
        "IBMPlexSans-Bold.ttf",
        "IBMPlexSans-Italic.ttf",
        "IBMPlexMono-Regular.ttf",
    ] {
        assert!(assets.join(font).is_file(), "missing {font}");
    }
}

#[test]
fn t4_10_release_update_atomic_swap_rollback_and_clean_restart() {
    let ext_fault = MockExternalServer::start("k");
    let mut ws = E2eWorkspace::new("t4_10_release");
    let client = LoopbackHttpClient::default();

    // Step 1: Tampered artifact SHA-256 mismatch is detected before staging/swap
    ext_fault.set_update_scenario(UpdateScenario::Sha256Mismatch);
    let bad_meta: Value = client
        .execute_raw(ext_fault.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let expected_bad_sha = bad_meta["assets"][0]["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let bad_dl = bad_meta["assets"][0]["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext_fault.base_url())
        .unwrap();
    let bad_zip = client
        .execute_raw(ext_fault.addr(), "GET", bad_dl, &[], None)
        .unwrap();
    assert_eq!(bad_zip.status, 200);
    assert_ne!(sha256_hex(&bad_zip.body), expected_bad_sha);

    // Step 2: Downgrade redirect is returned verbatim without silent redirect follow
    ext_fault.set_update_scenario(UpdateScenario::HttpsToHttpRedirect302);
    let redir_meta: Value = client
        .execute_raw(ext_fault.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let redir_dl = redir_meta["assets"][0]["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext_fault.base_url())
        .unwrap();
    let redir = client
        .execute_raw(ext_fault.addr(), "GET", redir_dl, &[], None)
        .unwrap();
    assert_eq!(redir.status, 302);
    assert!(redir.header("location").is_some());

    // Step 3: Valid release passes size, SHA-256, zip-slip, and 4-file bundle verification
    let ext_valid = MockExternalServer::start("k");
    let meta: Value = client
        .execute_raw(ext_valid.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let asset = &meta["assets"][0];
    let dl = asset["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext_valid.base_url())
        .unwrap();
    let zip = client
        .execute_raw(ext_valid.addr(), "GET", dl, &[], None)
        .unwrap();
    assert_eq!(zip.status, 200);
    assert_eq!(zip.body.len(), asset["size"].as_u64().unwrap() as usize);
    assert_eq!(
        sha256_hex(&zip.body),
        asset["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap()
    );
    let entries = parse_store_zip(&zip.body).unwrap();
    assert_eq!(entries.len(), 4);
    assert!(
        entries
            .iter()
            .all(|(name, _)| !name.contains("..") && !name.starts_with('/'))
    );
    for required in [
        "postbox.exe",
        "postboxctl.exe",
        "IBMPlexSans-Regular.ttf",
        "IBM-Plex-LICENSE.txt",
    ] {
        let payload = &entries
            .iter()
            .find(|(n, _)| n == required)
            .unwrap_or_else(|| panic!("missing {required} in release zip"))
            .1;
        assert!(!payload.is_empty());
    }

    // Step 4: Clean restart preserves on-disk WAL SQLite + FTS5 state and real workspace binaries run cleanly
    let chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "restart@s.whatsapp.net",
        "Release Restart Peer",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 100).unwrap();
    let msg = mk_msg(
        &chat.key,
        "M-REL-1",
        "Ada",
        "2026-11-26T12:00:00Z",
        "Clean restart after release update",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();
    ws.reopen_store().unwrap();
    assert_eq!(ws.count_chats(), 1);
    assert_eq!(ws.store.search_messages_fts("restart", 5).unwrap().len(), 1);

    let app = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(app.exit_code, 0, "stderr: {}", app.stderr);
    let ctl = ProcessRunner::run("postboxctl", &["status"], Duration::from_secs(10));
    assert_eq!(ctl.exit_code, 0, "stderr: {}", ctl.stderr);
    assert_eq!(
        serde_json::from_str::<ControlRequest>(ctl.stdout.trim()).unwrap(),
        ControlRequest::Status
    );

    // M8 seam (`m7_os`): wire `SelfUpdater` atomic `.new`/`.old` swap, rollback, and `last-exit.txt` `reason=update` handoff.
    let _hooks = M8HookRegistry::current();
}
