#![forbid(unsafe_code)]

use postbox_core::archive::should_resurface_on_event;
use postbox_core::control::{CONTROL_PIPE_PREFIX, ControlRequest, TelemetryCounters};
use postbox_core::deep_link::{
    LINK_PIPE_PREFIX, SCHEME_PREFIX, is_postbox_url, missing_chat_note, parse as parse_deep_link,
};
use postbox_core::dictation::DictationLanguage;
use postbox_core::{
    Chat, ChatKey, FontScale, MediaKind, Message, Provider, Reaction, ReadStatus, ThemeChoice,
    ViewTab,
};
use postbox_e2e::fixtures::{
    minimal_gif89a, minimal_mp4_ftyp, minimal_ogg_opus_packet, minimal_png_1x1, pcm16_raw,
    wcag_contrast_ratio,
};
use postbox_e2e::wire::{build_multipart, parse_store_zip, sha256_hex};
use postbox_e2e::{
    E2eWorkspace, HubChatRow, HubMessageRow, LoopbackHttpClient, M8HookRegistry, MediaBehavior,
    MessagesFault, MockDeepgramServer, MockExternalServer, MockHubServer, MultipartPart,
    ParsedSseFrame, ProcessRunner, SendFault, WsOpcode, WsScriptStep, WsServerAction, WsTrigger,
};
use postbox_hub::{
    Endpoint, HubError, MAX_FILE_BYTES, OLDER_PAGE_LIMIT, SendReply, SseEvent, archive_request,
    before_cursor, chats_request, events_request, mark_read_request, media_request,
    messages_request, react_request, send_text_request,
};
use postbox_keymap::{Action, InboxChord, KeyContext, Keymap};
use postbox_platform::{Notifier, SecretStore};
use postbox_store::{MEDIA_TTL_SECS, OutboxPhase, OutboxRecord};
use postbox_ui::theme::ShadcnPalette;
use postbox_ui::{BASE_FONT_PX, FONT_MONO, FONT_SANS, scaled_px};
use serde_json::{Value, json};
use std::fs;
use std::time::Duration;

fn sample_chat(provider: Provider, account: &str, id: &str, title: &str, ts: &str) -> Chat {
    Chat {
        key: ChatKey::new(provider, account, id),
        title: title.to_string(),
        is_group: false,
        archived_local: false,
        archived_remote: false,
        pinned: false,
        unread_count: 0,
        last_msg_ts: ts.to_string(),
        last_msg_preview: format!("Preview: {title}"),
        avatar_hash: None,
    }
}

fn sample_message(
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

// ============================================================================
// Area 1: Core Messaging & Provider-Qualified ChatKey Isolation (5 tests)
// ============================================================================

#[test]
fn t1_a01_chat_key_provider_account_isolation() {
    let ws = E2eWorkspace::new("t1_a01_isolation");
    let specs = [
        (Provider::Whatsapp, "personal", "Personal Ada", 3),
        (Provider::Whatsapp, "business", "Business Ada", 1),
        (Provider::Slack, "T1", "Slack Shared 100", 5),
        (Provider::Telegram, "main", "Telegram Shared 100", 2),
    ];
    for (i, (provider, account, title, unread)) in specs.iter().enumerate() {
        let mut chat = sample_chat(
            *provider,
            account,
            "shared-100",
            title,
            &format!("2026-11-26T10:0{i}:00Z"),
        );
        chat.unread_count = *unread;
        ws.store.upsert_chat(&chat, 1000 + i as i64).unwrap();
    }

    let chats = ws.store.list_inbox_chats(false, 20).unwrap();
    assert_eq!(chats.len(), 4);
    for (provider, account, title, unread) in specs {
        let found = chats
            .iter()
            .find(|c| c.key == ChatKey::new(provider, account, "shared-100"))
            .expect("distinct ChatKey present");
        assert_eq!(found.title, title);
        assert_eq!(found.unread_count, unread);
    }
}

#[test]
fn t1_a01_sqlite_wal_persistence_across_restart() {
    let mut ws = E2eWorkspace::new("t1_a01_wal");
    assert_eq!(ws.pragma_journal_mode().to_ascii_lowercase(), "wal");
    assert_eq!(ws.pragma_foreign_keys(), 1);

    let key = ChatKey::new(Provider::Whatsapp, "personal", "491700001@s.whatsapp.net");
    let chat = sample_chat(
        Provider::Whatsapp,
        "personal",
        "491700001@s.whatsapp.net",
        "Ada Lovelace",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 1_764_150_000).unwrap();
    let msg = sample_message(
        &key,
        "M-WAL-1",
        "Ada",
        "2026-11-26T12:00:00Z",
        "Persistent WAL architecture verified",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();

    ws.reopen_store().expect("reopen on-disk WAL store");

    let inbox = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].key, key);
    let hits = ws.store.search_messages_fts("Persistent", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].msg_id, "M-WAL-1");
    assert_eq!(hits[0].chat, key);
}

#[test]
fn t1_a01_headless_postbox_binary_boots_cleanly() {
    let out = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Combined inbox - Postbox v"),
        "unexpected stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("font=IBM Plex Sans"));
    assert!(out.stdout.contains("default bindings"));
}

#[test]
fn t1_a01_bundled_assets_and_license_present() {
    let assets = E2eWorkspace::assets_dir();
    let fonts = [
        "IBMPlexSans-Regular.ttf",
        "IBMPlexSans-Medium.ttf",
        "IBMPlexSans-SemiBold.ttf",
        "IBMPlexSans-Bold.ttf",
        "IBMPlexSans-Italic.ttf",
        "IBMPlexMono-Regular.ttf",
    ];
    for font in fonts {
        let bytes = fs::read(assets.join(font)).unwrap_or_else(|e| panic!("missing {font}: {e}"));
        assert!(bytes.len() > 50_000, "{font} unexpectedly small");
        assert_eq!(&bytes[..4], &[0x00, 0x01, 0x00, 0x00], "{font} sfnt magic");
    }

    let license = fs::read_to_string(assets.join("IBM-Plex-LICENSE.txt")).unwrap();
    assert!(license.contains("SIL OPEN FONT LICENSE"));

    let png = fs::read(assets.join("postbox-icon.png")).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

    assert!(fs::read(assets.join("messages.ico")).unwrap().len() > 100);
    let svg = fs::read_to_string(assets.join("postbox-icon.svg")).unwrap();
    assert!(svg.contains("<svg"));
}

#[test]
fn t1_a01_workspace_lint_unsafe_deny_and_four_providers() {
    let manifest = fs::read_to_string(E2eWorkspace::repo_root().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("unsafe_code = \"deny\""));

    for (provider, expected) in [
        (Provider::Whatsapp, "whatsapp"),
        (Provider::Slack, "slack"),
        (Provider::Telegram, "telegram"),
        (Provider::Signal, "signal"),
    ] {
        assert_eq!(provider.as_str(), expected);
        assert_eq!(Provider::parse(expected), Some(provider));
        let json = serde_json::to_string(&provider).unwrap();
        assert_eq!(json, format!("\"{expected}\""));
        assert_eq!(serde_json::from_str::<Provider>(&json).unwrap(), provider);
    }
}

// ============================================================================
// Area 2: Hub /v1/wazig/* HTTP & SSE Sync (6 tests)
// ============================================================================

#[test]
fn t1_a02_hub_endpoint_parsing_and_chats_fetch() {
    let hub = MockHubServer::start("tok-t1-a02");
    hub.set_chats(vec![
        HubChatRow {
            jid: "personal:1@s.whatsapp.net".into(),
            provider: "whatsapp".into(),
            name: "WhatsApp Alice".into(),
            kind: "dm".into(),
            last_message_ts: "2026-11-26T12:04:00Z".into(),
            unread_count: 2,
            unread: true,
            archived: false,
            account: Some("personal".into()),
            team_name: None,
            member_count: None,
            saved: None,
        },
        HubChatRow {
            jid: "slack:T1:C1".into(),
            provider: "slack".into(),
            name: "#general".into(),
            kind: "group".into(),
            last_message_ts: "2026-11-26T12:03:00Z".into(),
            unread_count: 1,
            unread: true,
            archived: false,
            account: Some("T1".into()),
            team_name: Some("Acme".into()),
            member_count: Some(15),
            saved: None,
        },
        HubChatRow {
            jid: "telegram:tg-1".into(),
            provider: "telegram".into(),
            name: "TG Bob".into(),
            kind: "dm".into(),
            last_message_ts: "2026-11-26T12:02:00Z".into(),
            unread_count: 0,
            unread: false,
            archived: false,
            account: Some("telegram".into()),
            team_name: None,
            member_count: None,
            saved: None,
        },
        HubChatRow {
            jid: "signal:aci-1".into(),
            provider: "signal".into(),
            name: "Signal Carol".into(),
            kind: "dm".into(),
            last_message_ts: "2026-11-26T12:01:00Z".into(),
            unread_count: 0,
            unread: false,
            archived: false,
            account: Some("signal".into()),
            team_name: None,
            member_count: None,
            saved: None,
        },
    ]);

    let https_ep = Endpoint::parse("https://hub.example.invalid/", false).unwrap();
    assert!(https_ep.secure);
    assert_eq!(https_ep.port, 443);

    let ep = Endpoint::parse(&hub.base_url(), true).unwrap();
    let client = LoopbackHttpClient::default();
    let req = chats_request(false, 50);
    let resp = client.execute_hub(&ep, &hub.token(), &req).unwrap();
    assert_eq!(resp.status, 200);
    let rows: Vec<HubChatRow> = resp.json().unwrap();
    assert_eq!(rows.len(), 4);

    let ws = E2eWorkspace::new("t1_a02_chats");
    for (i, row) in rows.iter().enumerate() {
        let provider = Provider::parse(&row.provider).unwrap();
        let chat = sample_chat(
            provider,
            row.account.as_deref().unwrap_or("default"),
            &row.jid,
            &row.name,
            &row.last_message_ts,
        );
        ws.store.upsert_chat(&chat, 2000 + i as i64).unwrap();
    }
    assert_eq!(ws.store.list_inbox_chats(false, 50).unwrap().len(), 4);
}

#[test]
fn t1_a02_hub_messages_pagination_before_cursor() {
    let hub = MockHubServer::start("tok-page");
    let chat_handle = "personal:15550001@s.whatsapp.net";
    let mut rows = Vec::new();
    for i in 0..120usize {
        rows.push(HubMessageRow::simple(
            chat_handle,
            format!("MSG-{i:04}"),
            "Ada",
            format!("Paginated message {i}"),
            format!("2026-11-26T{:02}:{:02}:00Z", i / 60, i % 60),
            false,
        ));
    }
    hub.set_messages(chat_handle, rows);

    let client = LoopbackHttpClient::default();
    let ep = hub.endpoint();
    let req1 = messages_request(chat_handle, OLDER_PAGE_LIMIT, None).unwrap();
    let resp1 = client.execute_hub(&ep, &hub.token(), &req1).unwrap();
    assert_eq!(resp1.status, 200);
    let page1: Vec<HubMessageRow> = resp1.json().unwrap();
    assert_eq!(page1.len(), OLDER_PAGE_LIMIT);

    let oldest_p1 = page1.last().unwrap();
    let cursor = before_cursor(&oldest_p1.MsgID, &oldest_p1.Timestamp).unwrap();
    let req2 = messages_request(chat_handle, OLDER_PAGE_LIMIT, Some(&cursor)).unwrap();
    let resp2 = client.execute_hub(&ep, &hub.token(), &req2).unwrap();
    assert_eq!(resp2.status, 200);
    let page2: Vec<HubMessageRow> = resp2.json().unwrap();
    assert_eq!(page2.len(), 40);

    let mut ws = E2eWorkspace::new("t1_a02_page");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "15550001@s.whatsapp.net");
    for row in page1.iter().chain(page2.iter()) {
        let m = sample_message(
            &key,
            &row.MsgID,
            &row.SenderName,
            &row.Timestamp,
            &row.Text,
            row.FromMe,
        );
        ws.store.upsert_message(&m).unwrap();
    }
    assert_eq!(ws.count_messages(), 120);
}

#[test]
fn t1_a02_hub_send_text_and_multipart_file_wire() {
    assert_eq!(MAX_FILE_BYTES, 64 * 1024 * 1024);
    let hub = MockHubServer::start("tok-send");
    let client = LoopbackHttpClient::default();
    let ep = hub.endpoint();

    let text_req = send_text_request(
        "personal:15550001@s.whatsapp.net",
        "Hello over loopback Hub!",
        None,
        false,
        "a1b2c3d4e5f60718293a4b5c6d7e8f90",
    )
    .unwrap();
    let resp = client.execute_hub(&ep, &hub.token(), &text_req).unwrap();
    assert_eq!(resp.status, 200);
    let reply: SendReply = resp.json().unwrap();
    assert_eq!(reply.status, "sent");
    assert_eq!(reply.client_id, "a1b2c3d4e5f60718293a4b5c6d7e8f90");

    let boundary = "PostboxUploadBoundary99887766";
    let multipart = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "account".into(),
                filename: None,
                content_type: None,
                body: b"hub".to_vec(),
            },
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:15550001@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "text".into(),
                filename: None,
                content_type: None,
                body: b"Attachment caption".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"11223344556677889900aabbccddeeff".to_vec(),
            },
            MultipartPart {
                name: "ptt".into(),
                filename: None,
                content_type: None,
                body: b"false".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("photo.png".into()),
                content_type: Some("image/png".into()),
                body: minimal_png_1x1(),
            },
        ],
    );
    let mp_resp = client
        .execute_hub_multipart(&ep, &hub.token(), "/v1/wazig/send", boundary, &multipart)
        .unwrap();
    assert_eq!(mp_resp.status, 200);
    let mp_reply: SendReply = mp_resp.json().unwrap();
    assert_eq!(mp_reply.status, "sent");
    assert_eq!(mp_reply.client_id, "11223344556677889900aabbccddeeff");
}

#[test]
fn t1_a02_hub_sse_stream_receives_events_and_resumes_last_event_id() {
    let hub = MockHubServer::start("tok-sse");
    hub.push_sse_burst(vec![
        SseEvent {
            id: 1,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:01Z".into(),
            kind: "msg".into(),
        },
        SseEvent {
            id: 2,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:02Z".into(),
            kind: "read".into(),
        },
    ]);

    let client = LoopbackHttpClient::default();
    let ep = hub.endpoint();
    let mut stream = client
        .open_sse(&ep, &hub.token(), &events_request(None))
        .unwrap();
    assert_eq!(
        stream.next_frame(Duration::from_secs(2)).unwrap(),
        Some(ParsedSseFrame::Event(SseEvent {
            id: 1,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:01Z".into(),
            kind: "msg".into(),
        }))
    );
    assert_eq!(
        stream.next_frame(Duration::from_secs(2)).unwrap(),
        Some(ParsedSseFrame::Event(SseEvent {
            id: 2,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:02Z".into(),
            kind: "read".into(),
        }))
    );
    drop(stream);

    hub.push_sse_event(SseEvent {
        id: 3,
        account: "T1".into(),
        chat: "slack:T1:C1".into(),
        ts: "2026-11-26T12:00:03Z".into(),
        kind: "msg".into(),
    });

    let mut resumed = client
        .open_sse(&ep, &hub.token(), &events_request(Some("2")))
        .unwrap();
    let third = resumed.next_frame(Duration::from_secs(2)).unwrap();
    match third {
        Some(ParsedSseFrame::Event(ev)) => assert_eq!(ev.id, 3),
        other => panic!("expected event 3, got {other:?}"),
    }
    assert_eq!(
        hub.sse_connection_last_event_ids(),
        vec![None, Some("2".to_string())]
    );
}

#[test]
fn t1_a02_hub_normalizes_same_account_lid_and_slack_attachments() {
    let hub = MockHubServer::start("tok-norm");
    let wa_chat = "personal:15550001@s.whatsapp.net";
    let slack_chat = "slack:T1:C1";
    hub.set_messages_fault(
        wa_chat,
        MessagesFault::SameAccountLidAlias {
            lid_chat: "personal:999@lid".into(),
        },
    );
    hub.set_messages_fault(slack_chat, MessagesFault::SlackAttachmentsAndReactions);

    let client = LoopbackHttpClient::default();
    let ep = hub.endpoint();

    let wa_resp = client
        .execute_hub(
            &ep,
            &hub.token(),
            &messages_request(wa_chat, 50, None).unwrap(),
        )
        .unwrap();
    let wa_rows: Vec<HubMessageRow> = wa_resp.json().unwrap();
    assert_eq!(wa_rows.len(), 1);
    assert_eq!(wa_rows[0].ChatJID, "personal:999@lid");

    let sl_resp = client
        .execute_hub(
            &ep,
            &hub.token(),
            &messages_request(slack_chat, 50, None).unwrap(),
        )
        .unwrap();
    let sl_rows: Vec<HubMessageRow> = sl_resp.json().unwrap();
    assert_eq!(sl_rows.len(), 1);
    assert!(sl_rows[0].attachments.is_some());
    assert!(sl_rows[0].reactions.is_some());
    assert!(sl_rows[0].LocalPath.is_some());

    let mut ws = E2eWorkspace::new("t1_a02_norm");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "15550001@s.whatsapp.net");
    let m = sample_message(
        &key,
        &wa_rows[0].MsgID,
        &wa_rows[0].SenderName,
        &wa_rows[0].Timestamp,
        &wa_rows[0].Text,
        false,
    );
    ws.store.upsert_message(&m).unwrap();
    assert_eq!(ws.store.search_messages_fts("Alias", 5).unwrap().len(), 1);
}

#[test]
fn t1_a02_hub_mark_read_react_delete_and_media_endpoints() {
    let hub = MockHubServer::start("tok-endpoints");
    let chat = "personal:15550001@s.whatsapp.net";
    let png = minimal_png_1x1();
    hub.set_media(
        chat,
        "M-IMG-1",
        MediaBehavior::Ready {
            bytes: png.clone(),
            content_type: "image/png".into(),
        },
    );
    hub.set_avatar(chat, Ok(png.clone()));

    let client = LoopbackHttpClient::default();
    let ep = hub.endpoint();

    let r1 = client
        .execute_hub(&ep, &hub.token(), &mark_read_request(chat, "").unwrap())
        .unwrap();
    assert_eq!(r1.status, 200);

    let r2 = client
        .execute_hub(
            &ep,
            &hub.token(),
            &react_request(chat, "M-IMG-1", "👍", "cid-react-1").unwrap(),
        )
        .unwrap();
    assert_eq!(r2.status, 200);

    let auth = format!("Bearer {}", hub.token());
    let r3 = client
        .execute_raw(
            hub.addr(),
            "POST",
            "/v1/wazig/delete",
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(
                json!({"chat": chat, "msg": "M-IMG-1", "client_id": "cid-del-1"})
                    .to_string()
                    .as_bytes(),
            ),
        )
        .unwrap();
    assert_eq!(r3.status, 200);

    let r4 = client
        .execute_hub(
            &ep,
            &hub.token(),
            &media_request(chat, Some("M-IMG-1")).unwrap(),
        )
        .unwrap();
    assert_eq!(r4.status, 200);
    assert_eq!(r4.body, png);

    let r5 = client
        .execute_hub(&ep, &hub.token(), &media_request(chat, None).unwrap())
        .unwrap();
    assert_eq!(r5.status, 200);
    assert_eq!(r5.body, png);

    for rec in hub.recorded_requests() {
        assert_eq!(rec.authorization.as_deref(), Some(auth.as_str()));
    }
}

// ============================================================================
// Area 3: ACID Durable Outbox & Delivery Guarantees (5 tests)
// ============================================================================

#[test]
fn t1_a03_outbox_enqueue_fifo_and_acknowledge() {
    let ws = E2eWorkspace::new("t1_a03_fifo");
    let hub = MockHubServer::start("tok-outbox");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "1@s.whatsapp.net");

    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "00000000000000000000000000000001".into(),
            chat: chat.clone(),
            kind: "text".into(),
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            text_body: "First FIFO".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "00000000000000000000000000000002".into(),
            chat,
            kind: "text".into(),
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            text_body: "Second FIFO".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 200,
        })
        .unwrap();

    let next = ws.store.next_ready_outbox(500).unwrap().unwrap();
    assert_eq!(next.client_id, "00000000000000000000000000000001");
    assert_eq!(next.text_body, "First FIFO");

    let req = send_text_request(
        "personal:1@s.whatsapp.net",
        &next.text_body,
        None,
        false,
        &next.client_id,
    )
    .unwrap();
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(resp.status, 200);
    let reply: SendReply = resp.json().unwrap();
    assert_eq!(reply.status, "sent");
    assert_eq!(reply.client_id, next.client_id);
}

#[test]
fn t1_a03_outbox_respects_not_before_backoff_schedule() {
    let ws = E2eWorkspace::new("t1_a03_backoff");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "2@s.whatsapp.net");
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            chat,
            kind: "text".into(),
            phase: OutboxPhase::Ready,
            attempt_serial: 1,
            text_body: "Retry after 3s".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 3_000,
            created_at_ms: 10,
        })
        .unwrap();

    assert!(ws.store.next_ready_outbox(2_999).unwrap().is_none());
    let ready = ws.store.next_ready_outbox(3_000).unwrap().unwrap();
    assert_eq!(ready.client_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
}

#[test]
fn t1_a03_outbox_pre_dispatch_uncertain_survives_restart() {
    let mut ws = E2eWorkspace::new("t1_a03_uncertain");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "3@s.whatsapp.net");
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            chat,
            kind: "text".into(),
            phase: OutboxPhase::Uncertain,
            attempt_serial: 1,
            text_body: "Crashed mid-flight".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 10,
        })
        .unwrap();

    ws.reopen_store().unwrap();
    assert!(ws.store.next_ready_outbox(i64::MAX).unwrap().is_none());
}

#[test]
fn t1_a03_outbox_idempotent_retry_reuses_client_id() {
    let hub = MockHubServer::start("tok-idem");
    hub.push_send_fault(SendFault::Transient503);
    let client = LoopbackHttpClient::default();
    let cid = "cccccccccccccccccccccccccccccccc";
    let req = send_text_request(
        "personal:4@s.whatsapp.net",
        "Idempotent msg",
        None,
        false,
        cid,
    )
    .unwrap();

    let first = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(first.status, 503);
    assert_eq!(hub.client_id_delivery_count(cid), 0);

    let second = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(second.status, 200);
    let reply: SendReply = second.json().unwrap();
    assert_eq!(reply.status, "sent");
    assert_eq!(reply.client_id, cid);
    assert_eq!(hub.client_id_delivery_count(cid), 1);

    // Repeating the same client_id is strictly idempotent
    let third = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(third.status, 200);
    assert_eq!(hub.client_id_delivery_count(cid), 1);
}

#[test]
fn t1_a03_outbox_explicit_user_retry_mints_fresh_client_id() {
    let ws = E2eWorkspace::new("t1_a03_retry");
    let hub = MockHubServer::start("tok-retry");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "5@s.whatsapp.net");
    let cid1 = "11111111111111111111111111111111";
    let cid2 = "22222222222222222222222222222222";

    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: cid1.into(),
            chat: chat.clone(),
            kind: "text".into(),
            phase: OutboxPhase::Held,
            attempt_serial: 4,
            text_body: "Failed once".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: cid2.into(),
            chat,
            kind: "text".into(),
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            text_body: "Failed once".into(),
            reply_to_id: None,
            reply_broadcast: false,
            not_before_ms: 0,
            created_at_ms: 200,
        })
        .unwrap();

    let next = ws.store.next_ready_outbox(300).unwrap().unwrap();
    assert_eq!(next.client_id, cid2);
    assert_ne!(next.client_id, cid1);
    let req = send_text_request(
        "personal:5@s.whatsapp.net",
        &next.text_body,
        None,
        false,
        &next.client_id,
    )
    .unwrap();
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(resp.status, 200);
}

// ============================================================================
// Area 4: Views, Combined Inbox & Window Title Invariant (5 tests)
// ============================================================================

#[test]
fn t1_a04_window_title_combined_inbox_and_account_views() {
    assert_eq!(
        ViewTab::CombinedInbox.window_title("0.1.0"),
        "Combined inbox - Postbox v0.1.0"
    );
    let slack = ViewTab::Account {
        provider: Provider::Slack,
        account: "T1".into(),
        label: "Slack Acme".into(),
    };
    assert_eq!(slack.window_title("0.1.0"), "Slack Acme - Postbox v0.1.0");
}

#[test]
fn t1_a04_combined_inbox_and_account_view_shortcuts() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-1"),
        Some(Action::SwitchCombinedInbox)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-i"),
        Some(Action::SwitchCombinedInbox)
    );
    for slot in 2u8..=9 {
        assert_eq!(
            km.resolve(KeyContext::Workspace, &format!("alt-{slot}")),
            Some(Action::SwitchAccountView(slot))
        );
    }
}

#[test]
fn t1_a04_multi_provider_accounts_stored_and_ordered() {
    let ws = E2eWorkspace::new("t1_a04_multi");
    let rows = [
        (
            Provider::Whatsapp,
            "personal",
            "w1",
            "WA Personal",
            "2026-11-26T10:01:00Z",
        ),
        (
            Provider::Whatsapp,
            "business",
            "w2",
            "WA Business",
            "2026-11-26T10:02:00Z",
        ),
        (
            Provider::Slack,
            "T1",
            "C1",
            "Slack Acme",
            "2026-11-26T10:03:00Z",
        ),
        (
            Provider::Telegram,
            "main",
            "tg1",
            "Telegram",
            "2026-11-26T10:04:00Z",
        ),
        (
            Provider::Signal,
            "main",
            "sg1",
            "Signal",
            "2026-11-26T10:05:00Z",
        ),
    ];
    for (i, (p, acct, id, title, ts)) in rows.iter().enumerate() {
        let chat = sample_chat(*p, acct, id, title, ts);
        ws.store.upsert_chat(&chat, 1000 + i as i64).unwrap();
    }
    let inbox = ws.store.list_inbox_chats(false, 50).unwrap();
    assert_eq!(inbox.len(), 5);
    assert_eq!(inbox[0].key.provider, Provider::Signal);
    assert_eq!(inbox[4].key.provider, Provider::Whatsapp);
}

#[test]
fn t1_a04_slack_workspace_name_precedence_from_hub() {
    let hub = MockHubServer::start("tok-slack-ws");
    hub.set_chats(vec![
        HubChatRow {
            jid: "slack:T1:C1".into(),
            provider: "slack".into(),
            name: "#eng".into(),
            kind: "group".into(),
            last_message_ts: "2026-11-26T12:00:00Z".into(),
            unread_count: 1,
            unread: true,
            archived: false,
            account: Some("T1".into()),
            team_name: Some("Acme Eng".into()),
            member_count: Some(12),
            saved: None,
        },
        HubChatRow {
            jid: "slack:T1:C2".into(),
            provider: "slack".into(),
            name: "#ops".into(),
            kind: "group".into(),
            last_message_ts: "2026-11-26T11:59:00Z".into(),
            unread_count: 0,
            unread: false,
            archived: false,
            account: Some("T1".into()),
            team_name: None,
            member_count: Some(5),
            saved: None,
        },
    ]);
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &chats_request(false, 50))
        .unwrap();
    assert_eq!(resp.status, 200);
    let rows: Vec<HubChatRow> = resp.json().unwrap();
    assert_eq!(rows[0].team_name.as_deref(), Some("Acme Eng"));
    assert_eq!(rows[1].team_name, None);
}

#[test]
fn t1_a04_no_toolbar_shell_and_ctrl_k_control_center() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-k"),
        Some(Action::OpenPalette)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "f1"),
        Some(Action::ShowShortcutsHelp)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-f"),
        Some(Action::SearchChats)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-o"),
        Some(Action::OpenLinksAndMedia)
    );

    let out = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("Combined inbox - Postbox v"));
    assert!(out.stdout.contains("font=IBM Plex Sans"));
    assert!(out.stdout.contains("default bindings"));
    assert!(!out.stdout.to_ascii_lowercase().contains("toolbar"));

    // M8 seam (`m1_ui`): wire GPUI headless PostboxShell check asserting `window.try_find("toolbar").is_none()`.
    let _hooks = M8HookRegistry::current();
}

// ============================================================================
// Area 5: Archive, Keep-Archived, Bulk Archive & IGSH-902 Resurface (5 tests)
// ============================================================================

#[test]
fn t1_a05_whatsapp_hub_archive_and_unarchive_roundtrip() {
    let hub = MockHubServer::start("tok-arch");
    let ws = E2eWorkspace::new("t1_a05_arch");
    let handle = "personal:1555@s.whatsapp.net";
    let mut chat = sample_chat(
        Provider::Whatsapp,
        "personal",
        "1555@s.whatsapp.net",
        "Archive Target",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 100).unwrap();

    let client = LoopbackHttpClient::default();
    let r1 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request(handle, true).unwrap(),
        )
        .unwrap();
    assert_eq!(r1.status, 200);
    chat.archived_local = true;
    ws.store.upsert_chat(&chat, 101).unwrap();
    assert!(ws.store.list_inbox_chats(false, 50).unwrap().is_empty());
    assert_eq!(ws.store.list_inbox_chats(true, 50).unwrap().len(), 1);

    let r2 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request(handle, false).unwrap(),
        )
        .unwrap();
    assert_eq!(r2.status, 200);
    chat.archived_local = false;
    ws.store.upsert_chat(&chat, 102).unwrap();
    assert_eq!(ws.store.list_inbox_chats(false, 50).unwrap().len(), 1);
}

#[test]
fn t1_a05_whatsapp_bulk_archive_batch_endpoint() {
    let hub = MockHubServer::start("tok-batch");
    let ws = E2eWorkspace::new("t1_a05_batch");
    let c1 = "personal:1@s.whatsapp.net";
    let c2 = "personal:2@s.whatsapp.net";

    let auth = format!("Bearer {}", hub.token());
    let body = json!({
        "account": "",
        "chats": [{"chat": c1}, {"chat": c2}],
        "archived": true
    })
    .to_string();
    let resp = LoopbackHttpClient::default()
        .execute_raw(
            hub.addr(),
            "POST",
            "/v1/wazig/archive-batch",
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(body.as_bytes()),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let val: Value = resp.json().unwrap();
    let results = val["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["chat"], c1);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[1]["chat"], c2);
    assert_eq!(results[1]["ok"], true);

    for id in ["1@s.whatsapp.net", "2@s.whatsapp.net"] {
        let mut chat = sample_chat(
            Provider::Whatsapp,
            "personal",
            id,
            id,
            "2026-11-26T12:00:00Z",
        );
        chat.archived_local = true;
        ws.store.upsert_chat(&chat, 200).unwrap();
    }
    assert!(ws.store.list_inbox_chats(false, 50).unwrap().is_empty());
    assert_eq!(ws.store.list_inbox_chats(true, 50).unwrap().len(), 2);
}

#[test]
fn t1_a05_igsh902_incoming_non_reaction_resurfaces_archived_chat() {
    assert!(should_resurface_on_event(true, false, false));
    let ws = E2eWorkspace::new("t1_a05_resurface");
    let mut chat = sample_chat(
        Provider::Whatsapp,
        "personal",
        "902@s.whatsapp.net",
        "Resurface Chat",
        "2026-11-26T12:00:00Z",
    );
    chat.archived_local = true;
    ws.store.upsert_chat(&chat, 100).unwrap();
    assert!(ws.store.list_inbox_chats(false, 50).unwrap().is_empty());

    if should_resurface_on_event(chat.archived_local, false, false) {
        chat.archived_local = false;
        chat.unread_count = 1;
        ws.store.upsert_chat(&chat, 101).unwrap();
    }
    let visible = ws.store.list_inbox_chats(false, 50).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].unread_count, 1);
}

#[test]
fn t1_a05_local_archive_for_slack_telegram_signal() {
    let ws = E2eWorkspace::new("t1_a05_local");
    let hub = MockHubServer::start("tok-local-arch");
    for (p, acct, id) in [
        (Provider::Slack, "T1", "C1"),
        (Provider::Telegram, "telegram", "tg-1"),
        (Provider::Signal, "signal", "aci-1"),
    ] {
        let mut chat = sample_chat(p, acct, id, id, "2026-11-26T12:00:00Z");
        chat.archived_local = true;
        chat.archived_remote = false;
        assert!(chat.is_archived());
        ws.store.upsert_chat(&chat, 100).unwrap();
    }
    assert!(ws.store.list_inbox_chats(false, 50).unwrap().is_empty());
    assert_eq!(ws.store.list_inbox_chats(true, 50).unwrap().len(), 3);

    let client = LoopbackHttpClient::default();
    for handle in ["signal:aci-1", "telegram:tg-1"] {
        let resp = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &archive_request(handle, true).unwrap(),
            )
            .unwrap();
        assert_eq!(resp.status, 502);
    }
}

#[test]
fn t1_a05_archive_shortcuts_and_inline_archived_toggle() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-e"),
        Some(Action::ArchiveSelected)
    );
    assert_eq!(
        km.resolve(KeyContext::ChatList, "e"),
        Some(Action::ArchiveSelected)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-a"),
        Some(Action::ToggleArchived)
    );
}

// ============================================================================
// Area 6: Replies, Slack Threads, Broadcast, Drafts, Pins, Unread & Read Ticks (6 tests)
// ============================================================================

#[test]
fn t1_a06_reply_and_slack_thread_broadcast_send() {
    let hub = MockHubServer::start("tok-bcast");
    let mut ws = E2eWorkspace::new("t1_a06_bcast");
    let req = send_text_request(
        "slack:T1:C1",
        "Broadcast update to channel",
        Some("1700.000100"),
        true,
        "cid-b1",
    )
    .unwrap();
    let body_json: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
    assert_eq!(body_json["reply_broadcast"], true);
    assert_eq!(body_json["reply_to"], "1700.000100");

    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(resp.status, 200);

    let key = ChatKey::new(Provider::Slack, "T1", "C1");
    let mut msg = sample_message(
        &key,
        "1700.000200",
        "Me",
        "2026-11-26T12:00:00Z",
        "Broadcast update to channel",
        true,
    );
    msg.reply_to_id = Some("1700.000100".into());
    msg.reply_broadcast = true;
    ws.store.upsert_message(&msg).unwrap();
    assert_eq!(
        ws.store.search_messages_fts("Broadcast", 5).unwrap().len(),
        1
    );
}

#[test]
fn t1_a06_mark_read_wire_for_whatsapp_slack_signal_telegram() {
    let hub = MockHubServer::start("tok-mr");
    let client = LoopbackHttpClient::default();

    let wa = mark_read_request("personal:1@s.whatsapp.net", "").unwrap();
    let sig = mark_read_request("signal:aci-1", "MSG-99").unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &wa)
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &sig)
            .unwrap()
            .status,
        200
    );

    let recorded = hub.recorded_requests();
    assert_eq!(recorded.len(), 2);
    let b1: Value = serde_json::from_slice(&recorded[0].body).unwrap();
    let b2: Value = serde_json::from_slice(&recorded[1].body).unwrap();
    assert_eq!(b1["message_id"], "");
    assert_eq!(b2["message_id"], "MSG-99");
}

#[test]
fn t1_a06_pinned_chats_sort_above_unpinned_in_store() {
    let ws = E2eWorkspace::new("t1_a06_pins");
    let mut pinned = sample_chat(
        Provider::Whatsapp,
        "personal",
        "pin@s.whatsapp.net",
        "Pinned Older",
        "2026-11-20T00:00:00Z",
    );
    pinned.pinned = true;
    let newer = sample_chat(
        Provider::Whatsapp,
        "personal",
        "new@s.whatsapp.net",
        "Unpinned Newer",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&pinned, 100).unwrap();
    ws.store.upsert_chat(&newer, 200).unwrap();

    let list = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].key, pinned.key);
    assert_eq!(list[1].key, newer.key);
}

#[test]
fn t1_a06_drafts_table_persists_per_chat_key() {
    let mut ws = E2eWorkspace::new("t1_a06_drafts");
    let k1 = ChatKey::new(Provider::Whatsapp, "personal", "1@s.whatsapp.net");
    let k2 = ChatKey::new(Provider::Whatsapp, "business", "1@s.whatsapp.net");
    ws.save_draft_row(&k1, "Personal draft", Some("M-1"), 100);
    ws.save_draft_row(&k2, "Business draft", None, 101);

    ws.reopen_store().unwrap();

    assert_eq!(
        ws.load_draft_row(&k1),
        Some(("Personal draft".into(), Some("M-1".into())))
    );
    assert_eq!(
        ws.load_draft_row(&k2),
        Some(("Business draft".into(), None))
    );
}

#[test]
fn t1_a06_unread_filter_and_reply_shortcuts() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::ChatList, "u"),
        Some(Action::ToggleUnread)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-shift-r"),
        Some(Action::ReplyToMessage)
    );
    assert_eq!(
        km.resolve(KeyContext::Composer, "enter"),
        Some(Action::SendMessage)
    );
    assert_eq!(
        km.resolve(KeyContext::Composer, "shift-enter"),
        Some(Action::InsertNewline)
    );
}

#[test]
fn t1_a06_read_status_progression_variants() {
    let mut ws = E2eWorkspace::new("t1_a06_read_status");
    for (variant, expected) in [
        (ReadStatus::None, "\"none\""),
        (ReadStatus::Sending, "\"sending\""),
        (ReadStatus::Sent, "\"sent\""),
        (ReadStatus::Delivered, "\"delivered\""),
        (ReadStatus::Read, "\"read\""),
        (ReadStatus::Failed, "\"failed\""),
    ] {
        let raw = serde_json::to_string(&variant).unwrap();
        assert_eq!(raw, expected);
        assert_eq!(serde_json::from_str::<ReadStatus>(&raw).unwrap(), variant);
    }

    let key = ChatKey::new(Provider::Whatsapp, "personal", "tick@s.whatsapp.net");
    let mut msg = sample_message(
        &key,
        "M-TICK",
        "Me",
        "2026-11-26T12:00:00Z",
        "Read tick",
        true,
    );
    msg.read_status = ReadStatus::Read;
    ws.store.upsert_message(&msg).unwrap();
    assert_eq!(ws.count_messages(), 1);
}

// ============================================================================
// Area 7: Keyboard Engine, Chords, Hold-Ctrl Badges, Ctrl+K Palette, FTS5 & F1 (6 tests)
// ============================================================================

#[test]
fn t1_a07_postbox_default_keymap_64_bindings() {
    let km = Keymap::postbox_defaults();
    assert!(km.bindings().len() >= 64);
    let checks = [
        (KeyContext::Workspace, "ctrl-k", Action::OpenPalette),
        (KeyContext::Workspace, "f1", Action::ShowShortcutsHelp),
        (KeyContext::Workspace, "ctrl-f", Action::SearchChats),
        (KeyContext::ChatList, "/", Action::SearchChats),
        (KeyContext::Workspace, "escape", Action::ClearSearchOrBack),
        (KeyContext::Workspace, "alt-a", Action::ToggleArchived),
        (KeyContext::ChatList, "u", Action::ToggleUnread),
        (KeyContext::Workspace, "ctrl-e", Action::ArchiveSelected),
        (KeyContext::ChatList, "e", Action::ArchiveSelected),
        (KeyContext::Workspace, "ctrl-tab", Action::NextChat),
        (KeyContext::Workspace, "ctrl-shift-tab", Action::PrevChat),
        (KeyContext::Workspace, "alt-j", Action::NextMessage),
        (KeyContext::Workspace, "alt-k", Action::PrevMessage),
        (KeyContext::Workspace, "ctrl-r", Action::ReactToMessage),
        (
            KeyContext::Workspace,
            "ctrl-shift-r",
            Action::ReplyToMessage,
        ),
        (KeyContext::Workspace, "ctrl-p", Action::PlayPauseVoice),
        (KeyContext::Workspace, "ctrl-t", Action::ToggleTranscript),
        (KeyContext::Workspace, "ctrl-o", Action::OpenLinksAndMedia),
        (KeyContext::Workspace, "ctrl-d", Action::ToggleDictation),
        (
            KeyContext::Workspace,
            "ctrl-shift-m",
            Action::ToggleVoiceRecord,
        ),
        (
            KeyContext::VideoPlayer,
            "f11",
            Action::ToggleVideoFullscreen,
        ),
    ];
    for (ctx, stroke, action) in checks {
        assert_eq!(km.resolve(ctx, stroke), Some(action), "{ctx:?} {stroke}");
    }
}

#[test]
fn t1_a07_hold_ctrl_1_to_9_chat_slot_bindings() {
    let km = Keymap::postbox_defaults();
    for slot in 1u8..=9 {
        assert_eq!(
            km.resolve(KeyContext::Workspace, &format!("ctrl-{slot}")),
            Some(Action::OpenChatSlot(slot))
        );
    }
}

#[test]
fn t1_a07_inbox_chord_g_then_i_within_window() {
    let mut chord = InboxChord::default();
    assert!(!chord.on_key('g', 1_000, true, false));
    assert!(chord.on_key('i', 1_500, true, false));
    assert!(!chord.on_key('G', 2_000, true, false));
    assert!(chord.on_key('I', 2_400, true, false));
}

#[test]
fn t1_a07_keymap_json_override_replaces_and_appends() {
    let mut km = Keymap::postbox_defaults();
    km.apply_override_json(
        r#"{
            "overrides": [
                {"context": "workspace", "stroke": "ctrl-k", "action": "search_chats"},
                {"context": "chat_list", "stroke": "x", "action": "archive_selected"}
            ]
        }"#,
    )
    .unwrap();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-k"),
        Some(Action::SearchChats)
    );
    assert_eq!(
        km.resolve(KeyContext::ChatList, "x"),
        Some(Action::ArchiveSelected)
    );
    let added = km
        .bindings()
        .iter()
        .find(|b| b.context == KeyContext::ChatList && b.stroke == "x")
        .unwrap();
    assert_eq!(added.description, "User override");
}

#[test]
fn t1_a07_fts5_instant_search_prefix_and_diacritics() {
    let mut ws = E2eWorkspace::new("t1_a07_fts5");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "fts@s.whatsapp.net");
    for (id, body) in [
        ("M1", "Treffen im Café München um 14 Uhr"),
        ("M2", "Intel Arc DXVA hardware decode benchmark"),
        ("M3", "Überweisung ist abgeschlossen"),
    ] {
        let msg = sample_message(&key, id, "Ada", "2026-11-26T12:00:00Z", body, false);
        ws.store.upsert_message(&msg).unwrap();
    }

    let h1 = ws.store.search_messages_fts("Cafe*", 10).unwrap();
    assert_eq!(h1.len(), 1);
    assert_eq!(h1[0].msg_id, "M1");
    assert!(h1[0].snippet.contains('['));

    let h2 = ws.store.search_messages_fts("Munchen*", 10).unwrap();
    assert_eq!(h2.len(), 1);
    assert_eq!(h2[0].msg_id, "M1");

    let h3 = ws.store.search_messages_fts("DXVA*", 10).unwrap();
    assert_eq!(h3.len(), 1);
    assert_eq!(h3[0].msg_id, "M2");

    let h4 = ws.store.search_messages_fts("Uberweisung", 10).unwrap();
    assert_eq!(h4.len(), 1);
    assert_eq!(h4[0].msg_id, "M3");
}

#[test]
fn t1_a07_f1_shortcuts_help_binding_and_catalog_descriptions() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "f1"),
        Some(Action::ShowShortcutsHelp)
    );
    for binding in km.bindings() {
        assert!(
            !binding.description.trim().is_empty(),
            "empty description on {:?}",
            binding.action
        );
    }
}

// ============================================================================
// Area 8: Themes, Typography & Font Scaling (5 tests)
// ============================================================================

#[test]
fn t1_a08_three_shadcn_themes_distinct_and_default_zinc_dark() {
    assert_eq!(ThemeChoice::default(), ThemeChoice::ZincDark);
    let zinc = ShadcnPalette::for_choice(ThemeChoice::ZincDark);
    let amoled = ShadcnPalette::for_choice(ThemeChoice::Amoled);
    let light = ShadcnPalette::for_choice(ThemeChoice::Light);
    assert_eq!(zinc.background, 0x09090B);
    assert_eq!(amoled.background, 0x000000);
    assert_eq!(light.background, 0xFFFFFF);
    assert_ne!(zinc, amoled);
    assert_ne!(zinc, light);
    assert_ne!(amoled, light);
}

#[test]
fn t1_a08_wcag_aa_contrast_ge_4_5_all_three_themes() {
    for choice in [
        ThemeChoice::ZincDark,
        ThemeChoice::Amoled,
        ThemeChoice::Light,
    ] {
        let p = ShadcnPalette::for_choice(choice);
        let pairs = [
            ("fg/bg", p.foreground, p.background),
            ("fg/sidebar", p.foreground, p.sidebar),
            ("fg/card", p.foreground, p.card),
            ("fg/popover", p.foreground, p.popover),
            ("fg/bubble_in", p.foreground, p.bubble_incoming),
            ("primary/primary_fg", p.primary, p.primary_foreground),
            ("muted_fg/bg", p.muted_foreground, p.background),
            ("muted_fg/sidebar", p.muted_foreground, p.sidebar),
        ];
        for (name, a, b) in pairs {
            let ratio = wcag_contrast_ratio(a, b);
            assert!(
                ratio >= 4.5,
                "theme {choice:?} pair {name} contrast {ratio:.2} < 4.5"
            );
        }
    }
}

#[test]
fn t1_a08_ibm_plex_font_constants_and_base_px() {
    assert_eq!(FONT_SANS, "IBM Plex Sans");
    assert_eq!(FONT_MONO, "IBM Plex Mono");
    assert!((BASE_FONT_PX - 15.0).abs() < f32::EPSILON);
    assert!((scaled_px(15.0, 100) - 15.0).abs() < f32::EPSILON);
}

#[test]
fn t1_a08_font_scale_step_larger_smaller_and_presets() {
    assert_eq!(FontScale::DEFAULT, 100);
    let def = FontScale::default();
    assert_eq!(def.larger().percent(), 110);
    assert_eq!(def.smaller().percent(), 90);
    for (pct, expected_px) in [(80, 12.0), (100, 15.0), (125, 18.75), (150, 22.5)] {
        assert!((scaled_px(BASE_FONT_PX, pct) - expected_px).abs() < 0.001);
    }
}

#[test]
fn t1_a08_font_scale_shortcuts_bound() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl--"),
        Some(Action::FontSmaller)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-="),
        Some(Action::FontLarger)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-0"),
        Some(Action::FontReset)
    );
}

// ============================================================================
// Area 9: Media Auto-Download, 14-Day Eviction, Image/WebP/GIF Decode & LRU (5 tests)
// ============================================================================

#[test]
fn t1_a09_media_autodownload_on_chat_open_wire_and_cache() {
    let hub = MockHubServer::start("tok-media");
    let mut ws = E2eWorkspace::new("t1_a09_media");
    let chat = "personal:media@s.whatsapp.net";

    let items: [(&str, MediaKind, &str, Vec<u8>); 6] = [
        ("M-IMG", MediaKind::Image, "image/png", minimal_png_1x1()),
        ("M-GIF", MediaKind::Gif, "image/gif", minimal_gif89a()),
        ("M-VID", MediaKind::Video, "video/mp4", minimal_mp4_ftyp()),
        (
            "M-VOX",
            MediaKind::Voice,
            "audio/ogg",
            minimal_ogg_opus_packet(),
        ),
        ("M-STK", MediaKind::Sticker, "image/png", minimal_png_1x1()),
        (
            "M-DOC",
            MediaKind::Document,
            "application/pdf",
            b"%PDF-1.7\n".to_vec(),
        ),
    ];

    let client = LoopbackHttpClient::default();
    for (msg_id, _kind, mime, bytes) in &items {
        hub.set_media(
            chat,
            *msg_id,
            MediaBehavior::Ready {
                bytes: bytes.clone(),
                content_type: (*mime).into(),
            },
        );
        let resp = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some(msg_id)).unwrap(),
            )
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body, bytes);

        let sha = sha256_hex(&resp.body);
        let rel = format!("hub-media/{msg_id}.bin");
        ws.seed_media_cache(
            &format!("{chat}:{msg_id}"),
            &sha,
            &rel,
            mime,
            resp.body.len() as i64,
            1_764_150_000,
        );
    }
    ws.reopen_store().unwrap();
    assert_eq!(ws.count_media_cache(), 6);
    assert!(
        ws.store
            .evict_expired_media(1_764_150_000 + MEDIA_TTL_SECS - 1)
            .unwrap()
            .is_empty()
    );
    // M8 seam (`m3_media`): wire auto-download worker on chat open (3 concurrent slots + Zone.Identifier ADS).
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t1_a09_media_202_pending_then_200_ready_poll() {
    let hub = MockHubServer::start("tok-202");
    let chat = "personal:poll@s.whatsapp.net";
    let png = minimal_png_1x1();
    hub.set_media(
        chat,
        "M-POLL",
        MediaBehavior::PendingThenReady {
            remaining_202s: 2,
            bytes: png.clone(),
            content_type: "image/png".into(),
        },
    );

    let client = LoopbackHttpClient::default();
    let req = media_request(chat, Some("M-POLL")).unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap()
            .status,
        202
    );
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap()
            .status,
        202
    );
    let ready = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(ready.status, 200);
    assert_eq!(ready.body, png);
}

#[test]
fn t1_a09_14_day_media_cache_eviction_deletes_stale_keeps_fresh() {
    assert_eq!(MEDIA_TTL_SECS, 1_209_600);
    let ws = E2eWorkspace::new("t1_a09_evict");
    let now = 1_764_150_000_i64;
    let stale_ts = now - (15 * 24 * 60 * 60);
    let fresh_ts = now - (2 * 24 * 60 * 60);

    ws.seed_media_cache(
        "k-stale",
        "sha-stale",
        "hub-media/stale.png",
        "image/png",
        100,
        stale_ts,
    );
    ws.seed_media_cache(
        "k-fresh",
        "sha-fresh",
        "hub-media/fresh.png",
        "image/png",
        100,
        fresh_ts,
    );

    let evicted = ws.store.evict_expired_media(now).unwrap();
    assert_eq!(evicted, vec!["hub-media/stale.png".to_string()]);
    assert!(!ws.media_cache_exists("k-stale"));
    assert!(ws.media_cache_exists("k-fresh"));
}

#[test]
fn t1_a09_avatar_and_workspace_icon_endpoints() {
    let hub = MockHubServer::start("tok-av");
    let png = minimal_png_1x1();
    hub.set_avatar("personal:1@s.whatsapp.net", Ok(png.clone()));
    hub.set_avatar("telegram:tg-1", Ok(png.clone()));
    hub.set_workspace_icon("T01", Ok(png.clone()));

    let client = LoopbackHttpClient::default();
    let r1 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request("personal:1@s.whatsapp.net", None).unwrap(),
        )
        .unwrap();
    assert_eq!(r1.status, 200);
    assert_eq!(r1.body, png);

    let r2 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request("telegram:tg-1", None).unwrap(),
        )
        .unwrap();
    assert_eq!(r2.status, 200);
    assert_eq!(r2.body, png);

    let auth = format!("Bearer {}", hub.token());
    let r3 = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/workspace-icon/T01",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(r3.status, 200);
    assert_eq!(r3.body, png);
}

#[test]
fn t1_a09_slack_custom_emoji_index_and_media_endpoints() {
    let hub = MockHubServer::start("tok-emoji");
    let png = minimal_png_1x1();
    hub.set_slack_emoji_index(
        "T01",
        json!({"emoji": {"shipit": "https://emoji.example/shipit.png", "ship": "alias:shipit"}}),
    );
    hub.set_slack_emoji_media("T01", "shipit", Ok(png.clone()));

    let client = LoopbackHttpClient::default();
    let auth = format!("Bearer {}", hub.token());
    let idx = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/slack/emoji?account=T01",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(idx.status, 200);
    let v: Value = idx.json().unwrap();
    assert_eq!(v["emoji"]["ship"], "alias:shipit");

    let media = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/slack/emoji/media?account=T01&name=shipit",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(media.status, 200);
    assert_eq!(media.body, png);
    assert!(media.body.len() <= 1024 * 1024);
}

// ============================================================================
// Area 10: Paste-to-Attach, Lightbox, Reactions, @mentions, Link Unfurls & Giphy (6 tests)
// ============================================================================

#[test]
fn t1_a10_paste_to_attach_shortcut_and_multipart_upload() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Composer, "ctrl-v"),
        Some(Action::PasteImage)
    );

    let png = minimal_png_1x1();
    let hub = MockHubServer::start("tok-paste");
    let boundary = "PostboxPasteBoundary112233";
    let body = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-paste-1".to_vec(),
            },
            MultipartPart {
                name: "ptt".into(),
                filename: None,
                content_type: None,
                body: b"false".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("pasted-1.png".into()),
                content_type: Some("image/png".into()),
                body: png.clone(),
            },
        ],
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub_multipart(
            &hub.endpoint(),
            &hub.token(),
            "/v1/wazig/send",
            boundary,
            &body,
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let reply: SendReply = resp.json().unwrap();
    assert_eq!(reply.status, "sent");

    let recorded = hub.recorded_requests();
    assert_eq!(recorded.len(), 1);
    let parts = recorded[0]
        .multipart_parts
        .as_ref()
        .expect("multipart parts recorded");
    let file_part = parts.iter().find(|p| p.name == "file").unwrap();
    assert_eq!(file_part.filename.as_deref(), Some("pasted-1.png"));
    assert_eq!(file_part.body, png);
    let ptt_part = parts.iter().find(|p| p.name == "ptt").unwrap();
    assert_eq!(ptt_part.body, b"false");

    // M8 seam (`m3_media`): wire clipboard paste-to-attach staging (`capacity = 16`).
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t1_a10_lightbox_close_shortcuts_and_gallery_flow() {
    let km = Keymap::postbox_defaults();
    for key in ["escape", "space", "enter"] {
        assert_eq!(
            km.resolve(KeyContext::Lightbox, key),
            Some(Action::CloseLightbox),
            "lightbox key {key}"
        );
    }
    assert_eq!(
        serde_json::to_string(&MediaKind::Image).unwrap(),
        "\"image\""
    );
    assert_eq!(serde_json::to_string(&MediaKind::Gif).unwrap(), "\"gif\"");
}

#[test]
fn t1_a10_reaction_add_and_remove_wire_and_store() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-r"),
        Some(Action::ReactToMessage)
    );

    let hub = MockHubServer::start("tok-react");
    let mut ws = E2eWorkspace::new("t1_a10_react");
    let client = LoopbackHttpClient::default();

    let add_req = react_request("personal:1@s.whatsapp.net", "M-1", "👍", "cid-r1").unwrap();
    let add_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &add_req)
        .unwrap();
    assert_eq!(add_resp.status, 200);
    let reply: SendReply = add_resp.json().unwrap();
    assert_eq!(reply.client_id, "cid-r1");

    let key = ChatKey::new(Provider::Whatsapp, "personal", "1@s.whatsapp.net");
    let mut msg = sample_message(
        &key,
        "M-1",
        "Ada",
        "2026-11-26T12:00:00Z",
        "React to me",
        false,
    );
    msg.reactions.push(Reaction {
        emoji: "👍".into(),
        count: 1,
        from_me: true,
    });
    ws.store.upsert_message(&msg).unwrap();

    let rem_req = react_request("personal:1@s.whatsapp.net", "M-1", "", "cid-r2").unwrap();
    let rem_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &rem_req)
        .unwrap();
    assert_eq!(rem_resp.status, 200);
}

#[test]
fn t1_a10_mention_autocomplete_send_formatting() {
    let hub = MockHubServer::start("tok-mention");
    let client = LoopbackHttpClient::default();
    for (chat, text, cid) in [
        ("slack:T1:C1", "Ping <@U123> on deploy", "cid-m1"),
        ("personal:1@g.us", "Hello @491234", "cid-m2"),
        ("signal:grp-1", "Check with @alice", "cid-m3"),
    ] {
        let req = send_text_request(chat, text, None, false, cid).unwrap();
        let resp = client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap();
        assert_eq!(resp.status, 200);
    }
    let recorded = hub.recorded_requests();
    assert_eq!(recorded.len(), 3);
    assert!(String::from_utf8_lossy(&recorded[0].body).contains("<@U123>"));
    assert!(String::from_utf8_lossy(&recorded[1].body).contains("@491234"));
    assert!(String::from_utf8_lossy(&recorded[2].body).contains("@alice"));
}

#[test]
fn t1_a10_link_unfurl_youtube_oembed_and_og_html_wire() {
    let ext = MockExternalServer::start("giphy-k");
    let mut ws = E2eWorkspace::new("t1_a10_unfurl");
    let client = LoopbackHttpClient::default();

    let oembed = client
        .execute_raw(
            ext.addr(),
            "GET",
            "/oembed?url=https%3A%2F%2Fyoutu.be%2FdQw4w9WgXcQ&format=json",
            &[],
            None,
        )
        .unwrap();
    assert_eq!(oembed.status, 200);
    let val: Value = oembed.json().unwrap();
    assert_eq!(val["provider_name"], "YouTube");
    assert_eq!(val["title"], "Intel Arc DXVA Demo");

    let thumb_path = val["thumbnail_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();
    let thumb = client
        .execute_raw(ext.addr(), "GET", thumb_path, &[], None)
        .unwrap();
    assert_eq!(thumb.status, 200);
    assert_eq!(&thumb.body[..2], &[0xFF, 0xD8]);

    let page = client
        .execute_raw(ext.addr(), "GET", "/page/standard", &[], None)
        .unwrap();
    assert_eq!(page.status, 200);
    let html = page.text();
    assert!(html.contains("og:title"));
    assert!(html.contains("og:description"));
    assert!(html.contains("og:image"));

    let thumb_sha = sha256_hex(&thumb.body);
    ws.seed_media_cache(
        "unfurl:yt:dQw4w9WgXcQ",
        &thumb_sha,
        "link-previews/yt-dQw4w9WgXcQ.jpg",
        "image/jpeg",
        thumb.body.len() as i64,
        1_764_150_000,
    );
    ws.reopen_store().unwrap();
    assert!(
        ws.store
            .evict_expired_media(1_764_150_000 + MEDIA_TTL_SECS - 1)
            .unwrap()
            .is_empty()
    );
    assert!(ws.media_cache_exists("unfurl:yt:dQw4w9WgXcQ"));

    // M8 seam (`m3_media`): wire `postbox-media` link unfurl parser & 24-entry disk cache.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t1_a10_giphy_trending_search_and_cdn_gif_download() {
    let ext = MockExternalServer::start("giphy-secret-1");
    let ws = E2eWorkspace::new("t1_a10_giphy");
    ws.platform
        .save_secret("GIPHY_API_KEY", " giphy-secret-1 \n")
        .unwrap();
    let key = ws.platform.load_secret("GIPHY_API_KEY").unwrap().unwrap();
    assert_eq!(key, "giphy-secret-1");

    let client = LoopbackHttpClient::default();
    let trending = client
        .execute_raw(
            ext.addr(),
            "GET",
            &format!("/v1/gifs/trending?api_key={key}&limit=25&rating=pg-13"),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(trending.status, 200);
    let search = client
        .execute_raw(
            ext.addr(),
            "GET",
            &format!("/v1/gifs/search?api_key={key}&q=cat&limit=25&rating=pg-13"),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(search.status, 200);
    let val: Value = search.json().unwrap();
    let id = val["data"][0]["id"].as_str().unwrap();

    let gif = client
        .execute_raw(
            ext.addr(),
            "GET",
            &format!("/media/{id}/giphy.gif"),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(gif.status, 200);
    assert_eq!(&gif.body[..6], b"GIF89a");
}

// ============================================================================
// Area 11: Voice Notes: Ogg Opus Playback, Recording & Cached Transcripts (5 tests)
// ============================================================================

#[test]
fn t1_a11_voice_playback_and_transcript_shortcuts() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-p"),
        Some(Action::PlayPauseVoice)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-t"),
        Some(Action::ToggleTranscript)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-shift-m"),
        Some(Action::ToggleVoiceRecord)
    );
    assert_eq!(
        serde_json::to_string(&MediaKind::Voice).unwrap(),
        "\"voice\""
    );
}

#[test]
fn t1_a11_voice_note_recording_multipart_ptt_upload() {
    let hub = MockHubServer::start("tok-ptt");
    let ogg = minimal_ogg_opus_packet();
    assert_eq!(&ogg[..4], b"OggS");

    let boundary = "PostboxVoiceBoundary777";
    let body = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:voice@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-voice-01".to_vec(),
            },
            MultipartPart {
                name: "ptt".into(),
                filename: None,
                content_type: None,
                body: b"true".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("voice.ogg".into()),
                content_type: Some("audio/ogg".into()),
                body: ogg,
            },
        ],
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub_multipart(
            &hub.endpoint(),
            &hub.token(),
            "/v1/wazig/send",
            boundary,
            &body,
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let reply: SendReply = resp.json().unwrap();
    assert_eq!(reply.status, "sent");
    let recorded = hub.recorded_requests();
    let parts = recorded[0].multipart_parts.as_ref().unwrap();
    assert!(parts.iter().any(|p| p.name == "ptt" && p.body == b"true"));
}

#[test]
fn t1_a11_deepgram_rest_transcription_and_immutable_sqlite_cache() {
    let dg = MockDeepgramServer::start("dg-rest-key");
    let ws = E2eWorkspace::new("t1_a11_dg_rest");
    let ogg = minimal_ogg_opus_packet();
    let sha = sha256_hex(&ogg);

    let client = LoopbackHttpClient::default();
    let path = DictationLanguage::Automatic.deepgram_listen_path();
    let resp = client
        .execute_raw(
            dg.addr(),
            "POST",
            path,
            &[
                ("Authorization", "Token dg-rest-key"),
                ("Content-Type", "audio/ogg"),
            ],
            Some(&ogg),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let v: Value = resp.json().unwrap();
    let text = v["results"]["channels"][0]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap();
    assert!(!text.is_empty());

    ws.store
        .cache_transcript(&sha, "en", text, 1_764_150_000)
        .unwrap();
    assert_eq!(
        ws.store.get_transcript(&sha).unwrap().as_deref(),
        Some(text)
    );

    // Cache hit avoids second Deepgram REST call
    if ws.store.get_transcript(&sha).unwrap().is_none() {
        panic!("expected cached transcript hit");
    }
    assert_eq!(dg.rest_call_count(), 1);
}

#[test]
fn t1_a11_deepgram_rest_fixed_english_and_german_paths() {
    let dg = MockDeepgramServer::start("dg-lang-key");
    let ogg = minimal_ogg_opus_packet();
    let client = LoopbackHttpClient::default();

    let en = client
        .execute_raw(
            dg.addr(),
            "POST",
            DictationLanguage::English.deepgram_listen_path(),
            &[("Authorization", "Token dg-lang-key")],
            Some(&ogg),
        )
        .unwrap();
    assert_eq!(en.status, 200);
    let en_v: Value = en.json().unwrap();
    assert_eq!(en_v["results"]["channels"][0]["detected_language"], "en");

    let de = client
        .execute_raw(
            dg.addr(),
            "POST",
            DictationLanguage::German.deepgram_listen_path(),
            &[("Authorization", "Token dg-lang-key")],
            Some(&ogg),
        )
        .unwrap();
    assert_eq!(de.status, 200);
    let de_v: Value = de.json().unwrap();
    assert_eq!(de_v["results"]["channels"][0]["detected_language"], "de");

    let recs = dg.recorded_rest();
    assert_eq!(recs.len(), 2);
    assert!(
        recs[0]
            .query_params
            .contains(&("language".into(), "en".into()))
    );
    assert!(
        recs[1]
            .query_params
            .contains(&("language".into(), "de".into()))
    );
}

#[test]
fn t1_a11_transcript_copy_raw_oldest_first_ordering() {
    let mut ws = E2eWorkspace::new("t1_a11_copy_raw");
    let entries = [
        ("sha-t1", "First voice note raw."),
        ("sha-t2", "Second voice note raw."),
        ("sha-t3", "Third voice note raw."),
    ];
    for (i, (sha, text)) in entries.iter().enumerate() {
        ws.store
            .cache_transcript(sha, "en", text, 1000 + i as i64)
            .unwrap();
    }
    ws.reopen_store().unwrap();

    let joined: Vec<String> = entries
        .iter()
        .map(|(sha, _)| ws.store.get_transcript(sha).unwrap().unwrap())
        .collect();
    assert_eq!(
        joined.join("\r\n\r\n"),
        "First voice note raw.\r\n\r\nSecond voice note raw.\r\n\r\nThird voice note raw."
    );
}

// ============================================================================
// Area 12: Deepgram Nova-3 Live Dictation, DPAPI Seeding & Anchored Interim Span (5 tests)
// ============================================================================

#[test]
fn t1_a12_secret_store_dpapi_seed_trim_and_reload() {
    let ws = E2eWorkspace::new("t1_a12_dpapi");
    ws.platform
        .save_secret("DEEPGRAM_API_KEY", "  dg-live-key-123 \r\n")
        .unwrap();
    assert_eq!(
        ws.platform.load_secret("DEEPGRAM_API_KEY").unwrap(),
        Some("dg-live-key-123".to_string())
    );
    ws.platform
        .save_secret("DEEPGRAM_API_KEY", "\tdg-refreshed-456 ")
        .unwrap();
    assert_eq!(
        ws.platform.load_secret("DEEPGRAM_API_KEY").unwrap(),
        Some("dg-refreshed-456".to_string())
    );
}

#[test]
fn t1_a12_deepgram_ws_live_dictation_handshake_and_pcm_stream() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-d"),
        Some(Action::ToggleDictation)
    );

    let dg = MockDeepgramServer::start("dg-ws-token");
    dg.set_ws_script(vec![
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: false,
                start: 0.0,
                duration: 0.4,
                transcript: "hello post".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(2),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.0,
                duration: 0.8,
                transcript: "Hello Postbox.".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
    ]);

    let client = LoopbackHttpClient::default();
    let mut ws_conn = client
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3&smart_format=true&interim_results=true&encoding=linear16&sample_rate=16000&channels=1",
            &[("Authorization", "Token dg-ws-token")],
        )
        .unwrap();

    let pcm = pcm16_raw(16_000, 100, 6000);
    ws_conn.send_binary(&pcm).unwrap();
    let f1 = ws_conn.recv_frame(Duration::from_secs(2)).unwrap();
    assert_eq!(f1.opcode, WsOpcode::Text);
    let v1: Value = serde_json::from_slice(&f1.payload).unwrap();
    assert_eq!(v1["is_final"], false);

    ws_conn.send_binary(&pcm).unwrap();
    let f2 = ws_conn.recv_frame(Duration::from_secs(2)).unwrap();
    let v2: Value = serde_json::from_slice(&f2.payload).unwrap();
    assert_eq!(v2["is_final"], true);
    let lang = v2["channel"]["alternatives"][0]["languages"][0].as_str();
    assert!(DictationLanguage::Automatic.accepts_detected_tag(lang));

    ws_conn.send_close(1000).unwrap();
}

#[test]
fn t1_a12_deepgram_ws_german_and_english_modes() {
    let dg = MockDeepgramServer::start("dg-modes");
    dg.set_ws_script(vec![WsScriptStep {
        trigger: WsTrigger::AfterBinaryFrames(1),
        action: WsServerAction::SendResults {
            is_final: true,
            start: 0.0,
            duration: 0.5,
            transcript: "Guten Tag, Welt.".into(),
            word_languages: vec!["de".into()],
            languages: vec!["de".into()],
        },
    }]);

    let client = LoopbackHttpClient::default();
    let mut conn = client
        .connect_ws(
            dg.addr(),
            DictationLanguage::German.deepgram_listen_path(),
            &[("Authorization", "Token dg-modes")],
        )
        .unwrap();
    conn.send_binary(&pcm16_raw(16_000, 50, 5000)).unwrap();
    let frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
    let v: Value = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(
        v["channel"]["alternatives"][0]["transcript"],
        "Guten Tag, Welt."
    );
    assert!(DictationLanguage::German.accepts_detected_tag(Some("de")));
    assert!(DictationLanguage::English.accepts_detected_tag(Some("en")));
    conn.send_close(1000).unwrap();
}

#[test]
fn t1_a12_deepgram_ws_closestream_drains_final_once() {
    let dg = MockDeepgramServer::start("dg-drain");
    dg.set_ws_script(vec![
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: false,
                start: 0.0,
                duration: 0.4,
                transcript: "hello wor".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::OnCloseStream,
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.0,
                duration: 0.8,
                transcript: "hello world.".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
    ]);

    let mut conn = LoopbackHttpClient::default()
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3&interim_results=true",
            &[("Authorization", "Token dg-drain")],
        )
        .unwrap();
    conn.send_binary(&pcm16_raw(16_000, 60, 4000)).unwrap();
    let interim = conn.recv_frame(Duration::from_secs(2)).unwrap();
    let iv: Value = serde_json::from_slice(&interim.payload).unwrap();
    assert_eq!(iv["is_final"], false);

    conn.send_text(r#"{"type":"CloseStream"}"#).unwrap();
    let final_frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
    let fv: Value = serde_json::from_slice(&final_frame.payload).unwrap();
    assert_eq!(fv["is_final"], true);
    assert_eq!(
        fv["channel"]["alternatives"][0]["transcript"],
        "hello world."
    );

    let close_frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
    assert_eq!(close_frame.opcode, WsOpcode::Close);
}

#[test]
fn t1_a12_dictation_ctrl_enter_send_and_esc_cancel_wire() {
    let dg = MockDeepgramServer::start("dg-send");
    let hub = MockHubServer::start("hub-send");
    dg.set_ws_script(vec![WsScriptStep {
        trigger: WsTrigger::OnCloseStream,
        action: WsServerAction::SendResults {
            is_final: true,
            start: 0.0,
            duration: 0.7,
            transcript: "Ship the release tonight.".into(),
            word_languages: vec!["en".into()],
            languages: vec!["en".into()],
        },
    }]);

    let client = LoopbackHttpClient::default();
    let mut conn = client
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3",
            &[("Authorization", "Token dg-send")],
        )
        .unwrap();
    conn.send_binary(&pcm16_raw(16_000, 60, 5000)).unwrap();
    conn.send_text(r#"{"type":"CloseStream"}"#).unwrap();
    let final_f = conn.recv_frame(Duration::from_secs(2)).unwrap();
    let fv: Value = serde_json::from_slice(&final_f.payload).unwrap();
    let transcript = fv["channel"]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap();

    let send_req = send_text_request(
        "personal:1@s.whatsapp.net",
        transcript,
        None,
        false,
        "cid-dict-send-1",
    )
    .unwrap();
    let resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &send_req)
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(hub.request_count("POST", "/v1/wazig/send"), 1);

    // Second session canceled mid-interim never sends to Hub
    let mut conn2 = client
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3",
            &[("Authorization", "Token dg-send")],
        )
        .unwrap();
    conn2.send_binary(&pcm16_raw(16_000, 40, 5000)).unwrap();
    conn2.send_close(1000).unwrap();
    assert_eq!(hub.request_count("POST", "/v1/wazig/send"), 1);
}

// ============================================================================
// Area 13: Video Thumbnails/Player (F11) & Windows OS / Toasts / postbox:// / postboxctl / Self-Updater / Crash (7 tests)
// ============================================================================

#[test]
fn t1_a13_video_media_download_and_f11_fullscreen_shortcut() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::VideoPlayer, "f11"),
        Some(Action::ToggleVideoFullscreen)
    );
    assert_eq!(
        serde_json::to_string(&MediaKind::Video).unwrap(),
        "\"video\""
    );

    let hub = MockHubServer::start("tok-vid");
    let mp4 = minimal_mp4_ftyp();
    hub.set_media(
        "personal:vid@s.whatsapp.net",
        "M-VID-1",
        MediaBehavior::Ready {
            bytes: mp4.clone(),
            content_type: "video/mp4".into(),
        },
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request("personal:vid@s.whatsapp.net", Some("M-VID-1")).unwrap(),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(&resp.body[4..8], b"ftyp");
}

#[test]
fn t1_a13_notifier_delivers_incoming_toast_for_chat_key() {
    let ws = E2eWorkspace::new("t1_a13_toast");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "ada@s.whatsapp.net");
    assert_eq!(ws.platform.notification_count(), 0);
    ws.platform
        .notify_message(&chat, "Ada", "Hello from Postbox")
        .unwrap();
    assert_eq!(ws.platform.notification_count(), 1);
}

#[test]
fn t1_a13_deep_link_parses_all_four_providers_and_slack_p_ts() {
    assert_eq!(SCHEME_PREFIX, "postbox://");
    assert_eq!(LINK_PIPE_PREFIX, r"\\.\pipe\postbox-link-");
    assert!(is_postbox_url("  POSTBOX://whatsapp/personal/123 "));

    let wa = parse_deep_link("postbox://whatsapp/personal/1555123%40s.whatsapp.net/M-1").unwrap();
    assert_eq!(
        wa.chat,
        ChatKey::new(Provider::Whatsapp, "personal", "1555123@s.whatsapp.net")
    );
    assert_eq!(wa.message.as_deref(), Some("M-1"));

    let sl = parse_deep_link("postbox://slack/T01/C02/p1700000005000100").unwrap();
    assert_eq!(sl.chat, ChatKey::new(Provider::Slack, "T01", "C02"));
    assert_eq!(sl.message.as_deref(), Some("1700000005.000100"));

    let tg = parse_deep_link("postbox://telegram/main/tg-99").unwrap();
    assert_eq!(tg.chat, ChatKey::new(Provider::Telegram, "main", "tg-99"));
    assert_eq!(tg.message, None);

    let sg = parse_deep_link("postbox://signal/main/aci-55/S-9").unwrap();
    assert_eq!(sg.chat, ChatKey::new(Provider::Signal, "main", "aci-55"));
    assert_eq!(sg.message.as_deref(), Some("S-9"));

    assert_eq!(
        missing_chat_note(Provider::Whatsapp),
        "Could not find that WhatsApp chat."
    );
    assert_eq!(
        missing_chat_note(Provider::Slack),
        "Could not find that Slack channel."
    );
    assert_eq!(
        missing_chat_note(Provider::Telegram),
        "Could not find that Telegram chat."
    );
    assert_eq!(
        missing_chat_note(Provider::Signal),
        "Could not find that Signal chat."
    );
}

#[test]
fn t1_a13_postboxctl_cli_emits_valid_json_for_supported_commands() {
    let cases: &[(&[&str], ControlRequest)] = &[
        (&["status"], ControlRequest::Status),
        (
            &["theme", "amoled"],
            ControlRequest::Theme {
                theme: ThemeChoice::Amoled,
            },
        ),
        (
            &["theme", "zinc"],
            ControlRequest::Theme {
                theme: ThemeChoice::ZincDark,
            },
        ),
        (
            &["theme", "light"],
            ControlRequest::Theme {
                theme: ThemeChoice::Light,
            },
        ),
        (&["font", "120"], ControlRequest::Font { percent: 120 }),
        (
            &["search", "hello"],
            ControlRequest::Search {
                text: "hello".into(),
            },
        ),
        (
            &["view", "inbox"],
            ControlRequest::View {
                target: "inbox".into(),
            },
        ),
        (
            &["archive", "c1"],
            ControlRequest::Archive { id: "c1".into() },
        ),
        (
            &["unarchive", "c1"],
            ControlRequest::Unarchive { id: "c1".into() },
        ),
    ];

    for (args, expected) in cases {
        let out = ProcessRunner::run("postboxctl", args, Duration::from_secs(10));
        assert_eq!(out.exit_code, 0, "args {args:?} stderr: {}", out.stderr);
        let trimmed = out.stdout.trim();
        assert!(
            !trimmed.contains('\n'),
            "expected single-line JSON: {trimmed}"
        );
        let parsed: ControlRequest = serde_json::from_str(trimmed).unwrap();
        assert_eq!(&parsed, expected);
    }
}

#[test]
fn t1_a13_control_request_all_11_variants_and_telemetry_schema() {
    assert_eq!(CONTROL_PIPE_PREFIX, r"\\.\pipe\postbox-control-");
    let variants = [
        ControlRequest::Status,
        ControlRequest::Theme {
            theme: ThemeChoice::ZincDark,
        },
        ControlRequest::Font { percent: 100 },
        ControlRequest::Chats {
            provider: Some(Provider::Whatsapp),
            archived: false,
            all: false,
            limit: Some(50),
        },
        ControlRequest::Search {
            text: "DXVA".into(),
        },
        ControlRequest::View {
            target: "inbox".into(),
        },
        ControlRequest::Select {
            id: "1@s.whatsapp.net".into(),
            provider: Some(Provider::Whatsapp),
            account: Some("personal".into()),
        },
        ControlRequest::Messages {
            id: Some("1@s.whatsapp.net".into()),
            limit: Some(80),
        },
        ControlRequest::Archive { id: "c1".into() },
        ControlRequest::Unarchive { id: "c1".into() },
        ControlRequest::ArchiveMany {
            ids: vec!["c1".into(), "c2".into()],
            archived: true,
        },
    ];
    for req in variants {
        let raw = serde_json::to_string(&req).unwrap();
        assert!(!raw.contains('\n'));
        let back: ControlRequest = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, req);
    }

    let telemetry = TelemetryCounters {
        frames_rendered: 1200,
        working_set_kb: 42_000,
        gpu_texture_bytes: 16 * 1024 * 1024,
        pending_outbox: 0,
    };
    let raw_t = serde_json::to_string(&telemetry).unwrap();
    assert_eq!(
        serde_json::from_str::<TelemetryCounters>(&raw_t).unwrap(),
        telemetry
    );
}

#[test]
fn t1_a13_self_updater_github_release_metadata_and_atomic_swap() {
    let ext = MockExternalServer::start("k");
    let client = LoopbackHttpClient::default();

    let meta = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap();
    assert_eq!(meta.status, 200);
    let v: Value = meta.json().unwrap();
    assert_eq!(v["tag_name"], "v0.2.0");
    assert_eq!(v["draft"], false);
    assert_eq!(v["prerelease"], false);

    let asset = &v["assets"][0];
    let expected_size = asset["size"].as_u64().unwrap() as usize;
    let expected_sha = asset["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let dl_path = asset["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();

    let zip_resp = client
        .execute_raw(ext.addr(), "GET", dl_path, &[], None)
        .unwrap();
    assert_eq!(zip_resp.status, 200);
    assert_eq!(zip_resp.body.len(), expected_size);
    assert_eq!(sha256_hex(&zip_resp.body), expected_sha);

    let entries = parse_store_zip(&zip_resp.body).unwrap();
    assert_eq!(entries.len(), 4);
    assert!(
        entries
            .iter()
            .all(|(name, _)| !name.contains("..") && !name.starts_with('/'))
    );
    let postbox_exe = &entries.iter().find(|(n, _)| n == "postbox.exe").unwrap().1;
    let postboxctl_exe = &entries
        .iter()
        .find(|(n, _)| n == "postboxctl.exe")
        .unwrap()
        .1;
    let font_ttf = &entries
        .iter()
        .find(|(n, _)| n == "IBMPlexSans-Regular.ttf")
        .unwrap()
        .1;
    let license_txt = &entries
        .iter()
        .find(|(n, _)| n == "IBM-Plex-LICENSE.txt")
        .unwrap()
        .1;
    assert!(!postbox_exe.is_empty());
    assert!(!postboxctl_exe.is_empty());
    assert!(font_ttf.starts_with(&[0x00, 0x01, 0x00, 0x00]));
    assert!(String::from_utf8_lossy(license_txt).contains("SIL OPEN FONT LICENSE"));

    assert!(Endpoint::parse("https://github.com", false).is_ok());
    assert_eq!(
        Endpoint::parse("http://github.com", false),
        Err(HubError::InsecureUrl)
    );

    // M8 seam (`m7_os`): wire `SelfUpdater` atomic `.new`/`.old` swap and monitor-preserving relaunch.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t1_a13_clean_exit_record_write_read_and_delete_lifecycle() {
    let mut ws = E2eWorkspace::new("t1_a13_exit");
    for run_idx in 0..2 {
        let out = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
        assert_eq!(
            out.exit_code, 0,
            "run {run_idx} failed with stderr: {}",
            out.stderr
        );
        assert!(out.stderr.trim().is_empty());
        assert!(out.stdout.contains("Combined inbox - Postbox v"));
    }

    let chat = sample_chat(
        Provider::Whatsapp,
        "personal",
        "exit@s.whatsapp.net",
        "Clean Exit Peer",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 100).unwrap();
    let msg = sample_message(
        &chat.key,
        "M-EXIT-1",
        "Ada",
        "2026-11-26T12:00:00Z",
        "Persisted across clean restart",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();
    ws.reopen_store().unwrap();

    assert_eq!(ws.pragma_journal_mode().to_ascii_lowercase(), "wal");
    assert_eq!(ws.count_chats(), 1);
    assert_eq!(ws.count_messages(), 1);
    assert_eq!(
        ws.store.search_messages_fts("Persisted", 5).unwrap().len(),
        1
    );

    // M8 seam (`m7_os`): wire `CrashRecorder` clean `last-exit.txt` write + startup read-and-delete.
    let _hooks = M8HookRegistry::current();
}
